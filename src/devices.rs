//! Device lifecycle on the host: iOS simulators through `simctl`, Android emulators through the SDK.
//!
//! Every lease gets a fresh device that is deleted (iOS) or discarded (Android, `-read-only`) on
//! release, so nothing a tenant installs or types survives into another lease.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::actions::{HwButton, ScrollArgs, SwipeArgs, TapArgs};
use crate::config::{Config, Platform};
use crate::exec::{Account, CmdSpec, Job, Runner, tail};
use crate::ios;
use bytes::Bytes;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

const SHORT: Duration = Duration::from_secs(60);
const INSTALL: Duration = Duration::from_secs(600);
/// iOS simulators Seiji creates are named `seiji-<lease id>`; nothing else is ever deleted.
pub const SIM_PREFIX: &str = "seiji-";

/// What a backend needs to know about one lease.
#[derive(Debug, Clone)]
pub struct DeviceCtx {
    pub lease_id: String,
    pub platform: Platform,
    pub model: String,
    pub device_id: Option<String>,
    pub console_port: Option<u16>,
    pub account: Account,
}

impl DeviceCtx {
    pub fn device_id(&self) -> Result<&str> {
        self.device_id.as_deref().context("device has not been created yet")
    }
    pub fn dir(&self) -> PathBuf {
        self.account.lease_dir(&self.lease_id)
    }
}

/// What `install` learned about the app it installed.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct InstalledApp {
    pub filename: String,
    /// iOS `CFBundleIdentifier`; `None` for Android.
    pub bundle_id: Option<String>,
    /// iOS `CFBundleExecutable`: the process name in logs and crash reports.
    pub executable: Option<String>,
    pub version: Option<String>,
}

/// The decoder setup of the screen stream, as WebCodecs `VideoDecoder.configure` takes it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScreenFormat {
    /// RFC 6381 codec string, `avc1.PPCCLL`.
    pub codec: String,
    /// Coded size in pixels.
    pub width: u32,
    pub height: u32,
    /// The avcC record (SPS and PPS), base64.
    pub description: String,
}

/// One encoded H.264 sample of the screen.
#[derive(Debug, Clone)]
pub struct VideoFrame {
    /// An IDR frame: a decoder can start here.
    pub key: bool,
    /// Unix microseconds when the helper captured the screen.
    pub captured_us: i64,
    /// Unix microseconds when the encoder handed the frame over; `None` from an older helper.
    pub encoded_us: Option<i64>,
    /// The token of a long-term reference frame: `ScreenCommand::LtrAck` it once every viewer
    /// has the frame. `None` for other frames and from an older helper.
    pub ltr: Option<u64>,
    /// The frame a `ScreenCommand::Refresh` asked for: a delta frame that refers only to an
    /// acknowledged long-term reference.
    pub refresh: bool,
    /// AVCC: a 4-byte big-endian length before each NAL unit.
    pub data: Bytes,
}

/// What the screen helper reports.
#[derive(Debug, Clone)]
pub enum ScreenEvent {
    /// Capture, encoder and input are up.
    Ready,
    /// A new decoder setup; it comes before the first frame and after a size change.
    Format(ScreenFormat),
    Frame(VideoFrame),
    /// A non-fatal problem the helper reported (a bad command, an input it could not send).
    Warning(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TouchPhase {
    Down,
    Move,
    Up,
}

/// The screen edge a gesture starts at, for system gestures (home indicator, Notification Centre).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Edge {
    Top,
    Bottom,
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenButton {
    Home,
    Lock,
}

/// Input and control for the screen helper. Points are fractions (0-1) of the picture.
#[derive(Debug)]
pub enum ScreenCommand {
    Touch {
        phase: TouchPhase,
        x: f64,
        y: f64,
        edge: Option<Edge>,
    },
    /// A HID keyboard usage (page 7) down or up.
    Key {
        usage: u8,
        down: bool,
    },
    Button {
        button: ScreenButton,
        down: bool,
    },
    /// Make the next frame a keyframe, now.
    Keyframe,
    /// Make the next frame refer only to an acknowledged long-term reference (a keyframe if none
    /// is acknowledged), now: a viewer that skipped frames resumes there.
    Refresh,
    /// Every viewer has the long-term reference frame with this token.
    LtrAck(u64),
    Encode(StreamOpts),
    /// Answered once every command before it is with the helper, so input that does not go
    /// through the helper (AXe typing) comes after it.
    Sync(tokio::sync::oneshot::Sender<()>),
}

impl ScreenCommand {
    /// The command as one line of the helper's stdin protocol; `None` for `Sync`.
    pub fn to_line(&self) -> Option<String> {
        let v = match self {
            ScreenCommand::Touch { phase, x, y, edge } => {
                let mut v = serde_json::json!({ "touch": phase, "x": x, "y": y });
                if let Some(e) = edge {
                    v["edge"] = serde_json::json!(e);
                }
                v
            }
            ScreenCommand::Key { usage, down } => serde_json::json!({ "key": usage, "down": down }),
            ScreenCommand::Button { button, down } => serde_json::json!({ "button": button, "down": down }),
            ScreenCommand::Keyframe => serde_json::json!({ "keyframe": true }),
            ScreenCommand::Refresh => serde_json::json!({ "refresh": true }),
            ScreenCommand::LtrAck(token) => serde_json::json!({ "ltr_ack": token }),
            ScreenCommand::Encode(o) => {
                serde_json::json!({ "encode": { "fps": o.fps, "scale": o.scale, "bitrate": o.bitrate } })
            }
            ScreenCommand::Sync(_) => return None,
        };
        let mut line = v.to_string();
        line.push('\n');
        Some(line)
    }
}

/// One input sent to the screen (AXe on iOS).
#[derive(Debug, Clone)]
pub enum UiInput {
    Tap(TapArgs),
    /// Text read from a daemon-owned file, so it never appears on a command line.
    Type {
        text_file: PathBuf,
    },
    Swipe(SwipeArgs),
    Scroll(ScrollArgs),
    Button(HwButton),
    Key(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BatteryState {
    Charging,
    Charged,
    Discharging,
}

impl BatteryState {
    pub fn as_str(self) -> &'static str {
        match self {
            BatteryState::Charging => "charging",
            BatteryState::Charged => "charged",
            BatteryState::Discharging => "discharging",
        }
    }
}

/// `simctl status_bar override`: fixed values for clean, comparable screenshots.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusBar {
    /// e.g. `9:41`.
    pub time: Option<String>,
    /// 0-100.
    pub battery_level: Option<u8>,
    pub battery_state: Option<BatteryState>,
    /// 0-3.
    pub wifi_bars: Option<u8>,
    /// 0-4.
    pub cellular_bars: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Appearance {
    Light,
    Dark,
}

impl Appearance {
    pub fn as_str(self) -> &'static str {
        match self {
            Appearance::Light => "light",
            Appearance::Dark => "dark",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyAction {
    Grant,
    Revoke,
    Reset,
}

impl PrivacyAction {
    pub fn as_str(self) -> &'static str {
        match self {
            PrivacyAction::Grant => "grant",
            PrivacyAction::Revoke => "revoke",
            PrivacyAction::Reset => "reset",
        }
    }
}

/// Services `simctl privacy` knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum PrivacyService {
    All,
    Calendar,
    ContactsLimited,
    Contacts,
    Location,
    LocationAlways,
    PhotosAdd,
    Photos,
    MediaLibrary,
    Microphone,
    Motion,
    Reminders,
    Siri,
}

impl PrivacyService {
    pub fn as_str(self) -> &'static str {
        match self {
            PrivacyService::All => "all",
            PrivacyService::Calendar => "calendar",
            PrivacyService::ContactsLimited => "contacts-limited",
            PrivacyService::Contacts => "contacts",
            PrivacyService::Location => "location",
            PrivacyService::LocationAlways => "location-always",
            PrivacyService::PhotosAdd => "photos-add",
            PrivacyService::Photos => "photos",
            PrivacyService::MediaLibrary => "media-library",
            PrivacyService::Microphone => "microphone",
            PrivacyService::Motion => "motion",
            PrivacyService::Reminders => "reminders",
            PrivacyService::Siri => "siri",
        }
    }
}

/// A native `simctl` operation on the lease's simulator.
#[derive(Debug, Clone)]
pub enum DeviceOp {
    OpenUrl(String),
    Launch {
        bundle_id: String,
        arguments: Vec<String>,
        restart: bool,
    },
    Terminate {
        bundle_id: String,
    },
    Uninstall {
        bundle_id: String,
    },
    StatusBar(StatusBar),
    ClearStatusBar,
    Appearance(Appearance),
    /// `None` clears the simulated location.
    Location(Option<(f64, f64)>),
    Privacy {
        action: PrivacyAction,
        service: PrivacyService,
        bundle_id: String,
    },
    /// APNs payload in a daemon-owned file.
    Push {
        bundle_id: String,
        payload_file: PathBuf,
    },
    /// Returns the text of `log show`.
    LogShow {
        last_seconds: u64,
        predicate: Option<String>,
    },
    /// React Native's `RCT_jsLocation` default for `bundle_id`; `None` deletes it.
    Bundler {
        bundle_id: String,
        location: Option<String>,
    },
    /// Text placed on the simulator clipboard via `axe clipboard copy` (verified by readback).
    Pasteboard {
        text_file: PathBuf,
    },
}

impl DeviceOp {
    pub fn name(&self) -> &'static str {
        match self {
            DeviceOp::OpenUrl(_) => "open_url",
            DeviceOp::Launch { .. } => "launch_app",
            DeviceOp::Terminate { .. } => "terminate_app",
            DeviceOp::Uninstall { .. } => "uninstall_app",
            DeviceOp::StatusBar(_) | DeviceOp::ClearStatusBar => "status_bar",
            DeviceOp::Appearance(_) => "appearance",
            DeviceOp::Location(_) => "location",
            DeviceOp::Privacy { .. } => "permissions",
            DeviceOp::Push { .. } => "push_notification",
            DeviceOp::LogShow { .. } => "get_logs",
            DeviceOp::Bundler { .. } => "react_native_bundler",
            DeviceOp::Pasteboard { .. } => "pasteboard",
        }
    }
}

/// A recording that runs while a run executes.
#[derive(Debug, Clone)]
pub enum Capture {
    /// Screen video, written by the tenant to `dest` (in the tenant's run dir).
    Video { dest: PathBuf },
    /// Unified log lines matching `predicate`, streamed to `out` (daemon-owned).
    Log { predicate: String, out: PathBuf },
}

#[derive(Debug, Clone, Default)]
pub struct XctestOptions {
    /// `.xctestrun` file name inside the bundle; the first one found when `None`.
    pub xctestrun: Option<String>,
    pub only_testing: Vec<String>,
    pub skip_testing: Vec<String>,
    pub test_configuration: Option<String>,
}

#[derive(Debug, Clone)]
pub struct XctestOutcome {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
    /// Last lines of `xcodebuild`'s output.
    pub output_tail: String,
}

#[async_trait]
pub trait Devices: Send + Sync {
    /// Creates the device and returns its id (simulator UDID / adb serial). Cheap; does not wait for boot.
    async fn create(&self, ctx: &DeviceCtx) -> Result<String>;
    /// Waits until the device is usable.
    async fn boot(&self, ctx: &DeviceCtx, cancel: &CancellationToken) -> Result<()>;
    /// Shuts the device down and deletes it. Best effort; must not fail on an already-gone device.
    async fn destroy(&self, ctx: &DeviceCtx);
    /// Installs an app bundle (`.zip`/`.tar.gz` of a `.app` for iOS, `.apk` for Android).
    async fn install(&self, ctx: &DeviceCtx, app: &Path, filename: &str) -> Result<InstalledApp>;
    /// JPEG of the current screen, longest side <= 2000 px (vision models reject larger images).
    async fn screenshot(&self, ctx: &DeviceCtx) -> Result<Vec<u8>>;
    /// Deletes Seiji devices of `account` that no active lease owns (after a crash or restart).
    async fn cleanup_orphans(&self, account: &Account, keep: &[String]);

    // iOS: evidence and native control. Android returns an "iOS only" error.

    /// Sends one input to the screen.
    async fn ui(&self, ctx: &DeviceCtx, input: &UiInput) -> Result<()>;
    /// The accessibility tree (AXe `describe-ui`): an array of nodes with `children`.
    async fn describe_ui(&self, ctx: &DeviceCtx) -> Result<serde_json::Value>;
    /// Full-resolution PNG written by the tenant to `dest` (its directory must exist).
    async fn save_screenshot(&self, ctx: &DeviceCtx, dest: &Path) -> Result<()>;
    /// Starts a recording; returns a handle for `stop_capture`.
    async fn start_capture(&self, ctx: &DeviceCtx, capture: &Capture) -> Result<String>;
    /// Stops a recording and lets it finish its file. Best effort.
    async fn stop_capture(&self, ctx: &DeviceCtx, handle: &str);
    /// Runs a native operation; returns its text output.
    async fn device_op(&self, ctx: &DeviceCtx, op: &DeviceOp) -> Result<String>;
    /// Runs a `build-for-testing` bundle; leaves `result.xcresult.zip`, summaries, attachments
    /// and `xcodebuild.log` in `out_dir` (tenant).
    #[allow(clippy::too_many_arguments)]
    async fn run_xctest(
        &self,
        ctx: &DeviceCtx,
        bundle: &Path,
        filename: &str,
        opts: &XctestOptions,
        out_dir: &Path,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<XctestOutcome>;
    /// Copies crash reports of `executable` written during the run into `<run_dir>/crashes`.
    async fn collect_crashes(&self, ctx: &DeviceCtx, run_dir: &Path, executable: &str) -> Result<usize>;
    /// Runs the screen helper until `cancel` fires or the helper ends: it streams the encoded
    /// screen into `events` and takes input and control from `commands`.
    async fn screen(
        &self,
        ctx: &DeviceCtx,
        opts: StreamOpts,
        commands: tokio::sync::mpsc::Receiver<ScreenCommand>,
        events: tokio::sync::mpsc::Sender<ScreenEvent>,
        cancel: &CancellationToken,
    ) -> Result<()>;
}

/// How the screen helper encodes the stream.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct StreamOpts {
    pub fps: u8,
    pub scale: f64,
    /// Bits per second.
    pub bitrate: u32,
}

/// A helper message larger than this is not a frame but a broken stream.
const MAX_MESSAGE: usize = 4 * 1024 * 1024;
const MSG_FORMAT: u8 = 1;
const MSG_FRAME: u8 = 2;
const MSG_STATUS: u8 = 3;

/// Checks a `format` message from the helper. The helper runs as the tenant, so its output is
/// untrusted: only a well-formed codec string, sane sizes and a base64 record go to a viewer.
fn parse_format(payload: &[u8]) -> Result<ScreenFormat> {
    let v: serde_json::Value = serde_json::from_slice(payload).context("format is not JSON")?;
    let codec = v["codec"].as_str().unwrap_or_default();
    let hex = codec.strip_prefix("avc1.").unwrap_or_default();
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("format has a bad codec {codec:?}");
    }
    let size = |k: &str| v[k].as_u64().filter(|n| (2..=8192).contains(n)).map(|n| n as u32);
    let (Some(width), Some(height)) = (size("width"), size("height")) else { bail!("format has a bad size") };
    let description = v["description"].as_str().unwrap_or_default();
    if description.is_empty()
        || description.len() > 4096
        || !description.bytes().all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b))
    {
        bail!("format has a bad description");
    }
    Ok(ScreenFormat { codec: codec.to_string(), width, height, description: description.to_string() })
}

/// One helper message as an event; `None` for a status the daemon does not use.
fn parse_message(kind: u8, payload: Vec<u8>) -> Result<Option<ScreenEvent>> {
    match kind {
        MSG_FORMAT => Ok(Some(ScreenEvent::Format(parse_format(&payload)?))),
        MSG_FRAME => {
            // Flags (bit 0: keyframe; bit 1: the encode time follows; bit 2: an LTR token follows;
            // bit 3: the refresh asked for), capture time, [encode time,] [token,] then the sample.
            let flags = payload.first().copied().unwrap_or(0);
            let (timed, ltr) = (flags & 2 == 2, flags & 4 == 4);
            let header = 9 + if timed { 8 } else { 0 } + if ltr { 8 } else { 0 };
            if payload.len() <= header {
                bail!("a {}-byte frame", payload.len());
            }
            let word = |at: usize| u64::from_be_bytes(payload[at..at + 8].try_into().expect("8 bytes"));
            let captured_us = word(1) as i64;
            let encoded_us = timed.then(|| word(9) as i64);
            Ok(Some(ScreenEvent::Frame(VideoFrame {
                key: flags & 1 == 1,
                captured_us,
                encoded_us,
                ltr: ltr.then(|| word(header - 8)),
                refresh: flags & 8 == 8,
                data: Bytes::from(payload).slice(header..),
            })))
        }
        MSG_STATUS => {
            let v: serde_json::Value = serde_json::from_slice(&payload).context("status is not JSON")?;
            if v["ready"] == true {
                return Ok(Some(ScreenEvent::Ready));
            }
            Ok(v["error"].as_str().map(|e| ScreenEvent::Warning(crate::exec::head(e, 300).to_string())))
        }
        other => bail!("unknown message kind {other}"),
    }
}

pub struct HostDevices {
    config: Arc<Config>,
    runner: Arc<Runner>,
    emulators: Mutex<HashMap<String, Job>>,
    captures: Mutex<HashMap<String, (Job, bool)>>,
}

impl HostDevices {
    pub fn new(config: Arc<Config>, runner: Arc<Runner>) -> Self {
        Self { config, runner, emulators: Mutex::new(HashMap::new()), captures: Mutex::new(HashMap::new()) }
    }

    fn ios_only(&self, ctx: &DeviceCtx, what: &str) -> Result<()> {
        if ctx.platform != Platform::Ios {
            bail!("{what} supports iOS simulators only for now");
        }
        Ok(())
    }

    async fn axe_run(&self, ctx: &DeviceCtx, spec: CmdSpec, what: &str) -> Result<serde_json::Value> {
        let timeout = Duration::from_secs(self.config.axe.step_timeout_seconds);
        let out = self.runner.run(&ctx.account, spec, timeout, &CancellationToken::new()).await?;
        ios::parse_axe(&out, what)
    }

    async fn run(&self, account: &Account, spec: CmdSpec, timeout: Duration, what: &str) -> Result<String> {
        let out = self.runner.run(account, spec, timeout, &CancellationToken::new()).await?.check(what)?;
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn android(&self) -> Result<&crate::config::Android> {
        self.config.android.as_ref().context("android is not configured on this host")
    }

    fn android_env(&self, spec: CmdSpec) -> Result<CmdSpec> {
        let a = self.android()?;
        let sdk = a.sdk_root.to_string_lossy().into_owned();
        Ok(spec
            .env("ANDROID_SDK_ROOT", &sdk)
            .env("ANDROID_HOME", &sdk)
            .env("ANDROID_AVD_HOME", a.avd_home.to_string_lossy()))
    }

    fn adb(&self, serial: &str) -> Result<CmdSpec> {
        self.android_env(CmdSpec::new(self.android()?.adb()).arg("-s").arg(serial))
    }

    /// Copies a daemon-owned file into the tenant's lease dir (the daemon cannot write there itself
    /// under os_user isolation). Returns the destination path.
    async fn copy_in(&self, ctx: &DeviceCtx, src: &Path, name: &str) -> Result<PathBuf> {
        let dir = ctx.dir().join("app");
        let dest = dir.join(name);
        let spec =
            CmdSpec::sh(r#"set -e; rm -rf "$1"; mkdir -p "$1"; cat > "$2""#, [dir.as_os_str(), dest.as_os_str()])
                .stdin_file(src);
        self.run(&ctx.account, spec, INSTALL, "copying app").await?;
        Ok(dest)
    }

    async fn sips_jpeg(&self, ctx: &DeviceCtx, capture: CmdSpec) -> Result<Vec<u8>> {
        let dir = ctx.dir();
        let png = dir.join("screen.png");
        self.runner.run(&ctx.account, capture, SHORT, &CancellationToken::new()).await?.check("taking screenshot")?;
        let jpg = dir.join("screen.jpg");
        let spec = CmdSpec::sh(
            r#"set -e; /usr/bin/sips -Z 2000 -s format jpeg -s formatOptions 85 "$1" --out "$2" >/dev/null; cat "$2"; rm -f "$1" "$2""#,
            [png.as_os_str(), jpg.as_os_str()],
        );
        let out = self
            .runner
            .run(&ctx.account, spec, SHORT, &CancellationToken::new())
            .await?
            .check("encoding screenshot")?;
        Ok(out.stdout)
    }
}

#[async_trait]
impl Devices for HostDevices {
    async fn create(&self, ctx: &DeviceCtx) -> Result<String> {
        let mkdir = CmdSpec::new("/bin/mkdir").arg("-p").arg(ctx.dir());
        self.run(&ctx.account, mkdir, SHORT, "creating lease dir").await?;
        match ctx.platform {
            Platform::Ios => {
                let ios = self.config.ios.as_ref().context("ios is not configured on this host")?;
                let model = ios.models.get(&ctx.model).with_context(|| format!("unknown iOS model {:?}", ctx.model))?;
                let spec = CmdSpec::new("/usr/bin/xcrun")
                    .args(["simctl", "create"])
                    .arg(format!("{SIM_PREFIX}{}", ctx.lease_id))
                    .arg(&model.device_type)
                    .arg(&model.runtime);
                let udid = self.run(&ctx.account, spec, SHORT, "simctl create").await?;
                if udid.is_empty() || udid.contains(char::is_whitespace) {
                    bail!("simctl create returned {udid:?}");
                }
                Ok(udid)
            }
            Platform::Android => {
                let a = self.android()?;
                let model =
                    a.models.get(&ctx.model).with_context(|| format!("unknown Android model {:?}", ctx.model))?;
                let port = ctx.console_port.context("android lease without console port")?;
                let spec = self.android_env(
                    CmdSpec::new(a.emulator())
                        .arg("-avd")
                        .arg(&model.avd)
                        .arg("-port")
                        .arg(port.to_string())
                        .args(["-read-only", "-no-window", "-no-snapshot-save"])
                        .args(&a.emulator_args),
                )?;
                let log = self.config.data_dir.join("run").join(format!("emulator-{}.log", ctx.lease_id));
                let job = self.runner.spawn(&ctx.account, spec, &log)?;
                self.emulators.lock().await.insert(ctx.lease_id.clone(), job);
                Ok(format!("emulator-{port}"))
            }
        }
    }

    async fn boot(&self, ctx: &DeviceCtx, cancel: &CancellationToken) -> Result<()> {
        let id = ctx.device_id()?;
        match ctx.platform {
            Platform::Ios => {
                let timeout = Duration::from_secs(self.config.ios.as_ref().map_or(180, |c| c.boot_timeout_seconds));
                let deadline = Instant::now() + timeout;
                let spec = CmdSpec::new("/usr/bin/xcrun").args(["simctl", "bootstatus", id, "-b"]);
                self.runner.run(&ctx.account, spec, timeout, cancel).await?.check("simctl bootstatus")?;
                loop {
                    let remaining = deadline
                        .checked_duration_since(Instant::now())
                        .context("iOS simulator did not expose an accessibility tree before the boot deadline")?;
                    let spec = ios::axe_describe(&self.config.axe, id);
                    let out = self.runner.run(&ctx.account, spec, remaining.min(SHORT), cancel).await?;
                    match ios::parse_axe(&out, "describe-ui during boot") {
                        Ok(tree) => {
                            let elements =
                                tree.as_array().context("AXe describe-ui returned a non-array result during boot")?;
                            if !elements.is_empty() {
                                return Ok(());
                            }
                        }
                        Err(error) if error.is::<ios::AutomationSessionPending>() => {}
                        Err(error) => return Err(error),
                    }
                    let delay = deadline.saturating_duration_since(Instant::now()).min(Duration::from_secs(1));
                    tokio::select! {
                        _ = cancel.cancelled() => bail!("cancelled"),
                        _ = tokio::time::sleep(delay) => {}
                    }
                }
            }
            Platform::Android => {
                let a = self.android()?;
                let deadline = Instant::now() + Duration::from_secs(a.boot_timeout_seconds);
                let log = self.config.data_dir.join("run").join(format!("emulator-{}.log", ctx.lease_id));
                loop {
                    if cancel.is_cancelled() {
                        bail!("cancelled");
                    }
                    if let Some(job) = self.emulators.lock().await.get_mut(&ctx.lease_id)
                        && job.try_exited()
                    {
                        let log = std::fs::read_to_string(&log).unwrap_or_default();
                        bail!("emulator exited during boot: {}", tail(&log, 3000));
                    }
                    let spec = self.adb(id)?.args(["shell", "getprop", "sys.boot_completed"]);
                    let out = self.runner.run(&ctx.account, spec, Duration::from_secs(10), cancel).await?;
                    if out.success() && String::from_utf8_lossy(&out.stdout).trim() == "1" {
                        return Ok(());
                    }
                    if Instant::now() >= deadline {
                        bail!("emulator did not finish booting in {}s", a.boot_timeout_seconds);
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }

    async fn destroy(&self, ctx: &DeviceCtx) {
        let result: Result<()> = async {
            match ctx.platform {
                Platform::Ios => {
                    if let Some(udid) = &ctx.device_id {
                        let shutdown = CmdSpec::new("/usr/bin/xcrun").args(["simctl", "shutdown", udid]);
                        let _ = self.runner.run(&ctx.account, shutdown, SHORT, &CancellationToken::new()).await;
                        let delete = CmdSpec::new("/usr/bin/xcrun").args(["simctl", "delete", udid]);
                        self.run(&ctx.account, delete, SHORT, "simctl delete").await?;
                    }
                }
                Platform::Android => {
                    let job = self.emulators.lock().await.remove(&ctx.lease_id);
                    if let Some(serial) = &ctx.device_id {
                        let _ = self
                            .runner
                            .run(
                                &ctx.account,
                                self.adb(serial)?.args(["emu", "kill"]),
                                SHORT,
                                &CancellationToken::new(),
                            )
                            .await;
                    }
                    if let Some(mut job) = job {
                        for _ in 0..30 {
                            if job.try_exited() {
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(500)).await;
                        }
                        if !job.try_exited() {
                            self.runner.kill(&ctx.account, &mut job).await;
                        }
                    }
                    let _ = std::fs::remove_file(
                        self.config.data_dir.join("run").join(format!("emulator-{}.log", ctx.lease_id)),
                    );
                }
            }
            let rm = CmdSpec::new("/bin/rm").arg("-rf").arg(ctx.dir());
            self.run(&ctx.account, rm, SHORT, "removing lease dir").await?;
            Ok(())
        }
        .await;
        if let Err(e) = result {
            tracing::warn!(lease = %ctx.lease_id, "destroy: {e:#}");
        }
    }

    async fn install(&self, ctx: &DeviceCtx, app: &Path, filename: &str) -> Result<InstalledApp> {
        let id = ctx.device_id()?;
        let file = self.copy_in(ctx, app, filename).await?;
        match ctx.platform {
            Platform::Ios => {
                let spec = CmdSpec::sh(ios::INSTALL_SCRIPT, [file.as_os_str(), id.as_ref()]);
                let out = self.run(&ctx.account, spec, INSTALL, "installing app").await?;
                let mut lines = out.lines().map(str::trim).map(|l| (!l.is_empty()).then(|| l.to_string()));
                Ok(InstalledApp {
                    filename: filename.to_string(),
                    bundle_id: lines.next().flatten(),
                    executable: lines.next().flatten(),
                    version: lines.next().flatten(),
                })
            }
            Platform::Android => {
                if !filename.ends_with(".apk") {
                    bail!("Android apps must be an .apk");
                }
                let spec = self.adb(id)?.args(["install", "-r", "-g"]).arg(&file);
                self.run(&ctx.account, spec, INSTALL, "adb install").await?;
                Ok(InstalledApp { filename: filename.to_string(), ..Default::default() })
            }
        }
    }

    async fn screenshot(&self, ctx: &DeviceCtx) -> Result<Vec<u8>> {
        let id = ctx.device_id()?;
        let png = ctx.dir().join("screen.png");
        let capture = match ctx.platform {
            Platform::Ios => ios::screenshot(id, &png),
            Platform::Android => {
                let adb = self.android()?.adb();
                self.android_env(CmdSpec::sh(
                    r#""$1" -s "$2" exec-out screencap -p > "$3""#,
                    [adb.as_os_str(), id.as_ref(), png.as_os_str()],
                ))?
            }
        };
        self.sips_jpeg(ctx, capture).await
    }

    async fn cleanup_orphans(&self, account: &Account, keep: &[String]) {
        if self.config.ios.is_none() {
            return;
        }
        let list = CmdSpec::new("/usr/bin/xcrun").args(["simctl", "list", "devices", "-j"]);
        let json = match self.run(account, list, SHORT, "simctl list").await {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(user = %account.name, "orphan scan: {e:#}");
                return;
            }
        };
        let orphans: Vec<(String, String)> = {
            let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&json) else { return };
            let Some(runtimes) = parsed.get("devices").and_then(|d| d.as_object()) else { return };
            runtimes
                .values()
                .filter_map(|v| v.as_array())
                .flatten()
                .filter_map(|d| Some((d["name"].as_str()?.to_string(), d["udid"].as_str()?.to_string())))
                .filter(|(name, udid)| name.starts_with(SIM_PREFIX) && !keep.contains(udid))
                .collect()
        };
        for (name, udid) in &orphans {
            tracing::info!(user = %account.name, udid, "deleting orphaned simulator {name}");
            for verb in ["shutdown", "delete"] {
                let spec = CmdSpec::new("/usr/bin/xcrun").args(["simctl", verb, udid]);
                let _ = self.runner.run(account, spec, SHORT, &CancellationToken::new()).await;
            }
        }
    }

    async fn ui(&self, ctx: &DeviceCtx, input: &UiInput) -> Result<()> {
        self.ios_only(ctx, "screen input")?;
        let what = match input {
            UiInput::Tap(_) => "tap",
            UiInput::Type { .. } => "type",
            UiInput::Swipe(_) => "swipe",
            UiInput::Scroll(_) => "scroll",
            UiInput::Button(_) => "button",
            UiInput::Key(_) => "key",
        };
        self.axe_run(ctx, ios::axe_input(&self.config.axe, ctx.device_id()?, input), what).await.map(|_| ())
    }

    async fn describe_ui(&self, ctx: &DeviceCtx) -> Result<serde_json::Value> {
        self.ios_only(ctx, "describe_ui")?;
        self.axe_run(ctx, ios::axe_describe(&self.config.axe, ctx.device_id()?), "describe-ui").await
    }

    async fn save_screenshot(&self, ctx: &DeviceCtx, dest: &Path) -> Result<()> {
        self.ios_only(ctx, "screenshots in runs")?;
        self.run(&ctx.account, ios::screenshot(ctx.device_id()?, dest), SHORT, "simctl screenshot").await.map(|_| ())
    }

    async fn start_capture(&self, ctx: &DeviceCtx, capture: &Capture) -> Result<String> {
        self.ios_only(ctx, "recording")?;
        let udid = ctx.device_id()?;
        let handle = format!("cap-{}", uuid::Uuid::new_v4().simple());
        let (job, is_video) = match capture {
            Capture::Video { dest } => {
                let log = self.config.data_dir.join("run").join(format!("{handle}.log"));
                let mut job = self.runner.spawn(&ctx.account, ios::record_video(udid, dest), &log)?;
                let ready = tokio::time::timeout(SHORT, async {
                    let mut reader = BufReader::new(tokio::fs::File::open(&log).await?);
                    let mut line = String::new();
                    loop {
                        if reader.read_line(&mut line).await? > 0 {
                            if line.trim() == "Recording started" {
                                return Ok::<(), anyhow::Error>(());
                            }
                            if line.ends_with('\n') {
                                line.clear();
                            }
                        } else {
                            if job.try_exited() {
                                bail!("simctl recordVideo exited before recording started");
                            }
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                    }
                })
                .await
                .context("simctl recordVideo did not start within 60 seconds")
                .and_then(|result| result);
                if let Err(error) = ready {
                    self.runner.interrupt(&ctx.account, &mut job, Duration::from_secs(30)).await;
                    let output = std::fs::read_to_string(&log).unwrap_or_default();
                    let _ = std::fs::remove_file(&log);
                    return Err(error.context(format!("recorder output: {}", tail(&output, 2000))));
                }
                (job, true)
            }
            Capture::Log { predicate, out } => {
                (self.runner.spawn(&ctx.account, ios::log_stream(udid, predicate), out)?, false)
            }
        };
        self.captures.lock().await.insert(handle.clone(), (job, is_video));
        Ok(handle)
    }

    async fn stop_capture(&self, ctx: &DeviceCtx, handle: &str) {
        let Some((mut job, is_video)) = self.captures.lock().await.remove(handle) else { return };
        if is_video {
            // recordVideo writes the movie index on SIGINT; a SIGTERM leaves an unplayable file.
            self.runner.interrupt(&ctx.account, &mut job, Duration::from_secs(30)).await;
            let _ = std::fs::remove_file(self.config.data_dir.join("run").join(format!("{handle}.log")));
        } else {
            self.runner.kill(&ctx.account, &mut job).await;
        }
    }

    async fn device_op(&self, ctx: &DeviceCtx, op: &DeviceOp) -> Result<String> {
        self.ios_only(ctx, op.name())?;
        let spec = ios::device_op(&self.config.axe, ctx.device_id()?, op)?;
        self.run(&ctx.account, spec, SHORT, &format!("simctl {}", op.name())).await
    }

    async fn run_xctest(
        &self,
        ctx: &DeviceCtx,
        bundle: &Path,
        filename: &str,
        opts: &XctestOptions,
        out_dir: &Path,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<XctestOutcome> {
        self.ios_only(ctx, "run_xctest")?;
        let file = self.copy_in(ctx, bundle, filename).await?;
        let mut args: Vec<std::ffi::OsString> = vec![
            file.into_os_string(),
            out_dir.as_os_str().to_owned(),
            ctx.device_id()?.into(),
            opts.xctestrun.clone().unwrap_or_default().into(),
        ];
        args.extend(opts.only_testing.iter().map(|t| format!("-only-testing:{t}").into()));
        args.extend(opts.skip_testing.iter().map(|t| format!("-skip-testing:{t}").into()));
        if let Some(c) = &opts.test_configuration {
            args.push("-only-test-configuration".into());
            args.push(c.into());
        }
        let out = self.runner.run(&ctx.account, CmdSpec::sh(ios::XCTEST_SCRIPT, args), timeout, cancel).await?;
        Ok(XctestOutcome {
            exit_code: out.code,
            timed_out: out.timed_out,
            cancelled: out.cancelled,
            output_tail: tail(&out.text(), 8000).to_string(),
        })
    }

    async fn collect_crashes(&self, ctx: &DeviceCtx, run_dir: &Path, executable: &str) -> Result<usize> {
        self.ios_only(ctx, "crash reports")?;
        if !ios::valid_executable(executable) {
            bail!("executable name {executable:?} is not safe to search for");
        }
        let spec = CmdSpec::sh(ios::CRASHES_SCRIPT, [run_dir.as_os_str(), executable.as_ref()]);
        Ok(self.run(&ctx.account, spec, SHORT, "collecting crash reports").await?.trim().parse().unwrap_or(0))
    }

    async fn screen(
        &self,
        ctx: &DeviceCtx,
        opts: StreamOpts,
        mut commands: tokio::sync::mpsc::Receiver<ScreenCommand>,
        events: tokio::sync::mpsc::Sender<ScreenEvent>,
        cancel: &CancellationToken,
    ) -> Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        self.ios_only(ctx, "the screen viewer")?;
        let spec = ios::sim_session(&self.config.viewer.sim_bin, ctx.device_id()?, &opts);
        let (mut job, mut stdin, mut stdout, mut stderr) = self.runner.spawn_duplex(&ctx.account, spec)?;
        // The last of stderr, for the error when the helper ends on its own.
        let stderr = tokio::spawn(async move {
            let (mut buf, mut chunk) = (Vec::new(), [0u8; 4096]);
            while let Ok(n @ 1..) = stderr.read(&mut chunk).await {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > 8192 {
                    buf.drain(..buf.len() - 4096);
                }
            }
            String::from_utf8_lossy(&buf).trim().to_string()
        });
        let write = async {
            while let Some(cmd) = commands.recv().await {
                match (cmd.to_line(), cmd) {
                    (Some(line), _) => stdin.write_all(line.as_bytes()).await?,
                    (None, ScreenCommand::Sync(done)) => {
                        let _ = done.send(());
                    }
                    (None, _) => {}
                }
            }
            anyhow::Ok(())
        };
        let read = async {
            let mut head = [0u8; 5];
            loop {
                match stdout.read_exact(&mut head).await {
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return anyhow::Ok(true),
                    Err(e) => return Err(e.into()),
                }
                let len = u32::from_be_bytes(head[1..5].try_into().expect("4 bytes")) as usize;
                if len > MAX_MESSAGE {
                    bail!("seiji-sim sent a {len}-byte message");
                }
                let mut payload = vec![0u8; len];
                stdout.read_exact(&mut payload).await?;
                let event = parse_message(head[0], payload).context("seiji-sim sent a bad message")?;
                if let Some(event) = event
                    && events.send(event).await.is_err()
                {
                    return Ok(false);
                }
            }
        };
        // `true`: the helper closed its output by itself.
        let result = tokio::select! {
            r = read => r,
            r = write => r.map(|_| false),
            _ = cancel.cancelled() => Ok(false),
        };
        self.runner.kill(&ctx.account, &mut job).await;
        let stderr = stderr.await.unwrap_or_default();
        match result {
            Ok(true) => bail!("seiji-sim ended: {}", if stderr.is_empty() { "no output" } else { tail(&stderr, 2000) }),
            Ok(false) => Ok(()),
            Err(e) if stderr.is_empty() => Err(e),
            Err(e) => Err(e.context(format!("seiji-sim: {}", tail(&stderr, 2000)))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_messages_reach_viewers_only_when_well_formed() {
        let good = br#"{"codec":"avc1.64001F","width":602,"height":1310,"description":"AWQAH/8=","screen":{}}"#;
        let Some(ScreenEvent::Format(f)) = parse_message(MSG_FORMAT, good.to_vec()).unwrap() else { panic!() };
        assert_eq!((f.codec.as_str(), f.width, f.height), ("avc1.64001F", 602, 1310));

        // The helper runs as the tenant: nothing else may reach a viewer's page.
        for bad in [
            r#"{"codec":"avc1.64001F\"<script>","width":602,"height":1310,"description":"AQ=="}"#,
            r#"{"codec":"vp09.00","width":602,"height":1310,"description":"AQ=="}"#,
            r#"{"codec":"avc1.64001F","width":0,"height":1310,"description":"AQ=="}"#,
            r#"{"codec":"avc1.64001F","width":602,"height":99999,"description":"AQ=="}"#,
            r#"{"codec":"avc1.64001F","width":602,"height":1310,"description":"<img>"}"#,
            "not json",
        ] {
            assert!(parse_message(MSG_FORMAT, bad.as_bytes().to_vec()).is_err(), "{bad}");
        }

        let mut frame = vec![1u8];
        frame.extend_from_slice(&1_795_548_363_000_000i64.to_be_bytes());
        frame.extend_from_slice(&[0, 0, 0, 1, 0x65]);
        let Some(ScreenEvent::Frame(f)) = parse_message(MSG_FRAME, frame).unwrap() else { panic!() };
        assert!(f.key);
        assert_eq!(f.captured_us, 1_795_548_363_000_000);
        assert_eq!(&f.data[..], &[0, 0, 0, 1, 0x65]);
        let mut timed = vec![3u8];
        timed.extend_from_slice(&1_795_548_363_000_000i64.to_be_bytes());
        timed.extend_from_slice(&1_795_548_363_004_000i64.to_be_bytes());
        timed.extend_from_slice(&[0, 0, 0, 1, 0x65]);
        let Some(ScreenEvent::Frame(f)) = parse_message(MSG_FRAME, timed).unwrap() else { panic!() };
        assert_eq!((f.key, f.encoded_us), (true, Some(1_795_548_363_004_000)));
        assert_eq!(&f.data[..], &[0, 0, 0, 1, 0x65]);
        assert!(parse_message(MSG_FRAME, vec![3; 17]).is_err(), "a timed frame needs a sample");
        // A refresh frame (bit 3) with an LTR token (bit 2) after the times.
        let mut ltr = vec![2u8 | 4 | 8];
        ltr.extend_from_slice(&1_795_548_363_000_000i64.to_be_bytes());
        ltr.extend_from_slice(&1_795_548_363_004_000i64.to_be_bytes());
        ltr.extend_from_slice(&42u64.to_be_bytes());
        ltr.extend_from_slice(&[0, 0, 0, 1, 0x41]);
        let Some(ScreenEvent::Frame(f)) = parse_message(MSG_FRAME, ltr).unwrap() else { panic!() };
        assert_eq!((f.key, f.refresh, f.ltr, f.encoded_us), (false, true, Some(42), Some(1_795_548_363_004_000)));
        assert_eq!(&f.data[..], &[0, 0, 0, 1, 0x41]);
        assert!(parse_message(MSG_FRAME, vec![6; 25]).is_err(), "a frame with a token needs a sample");
        assert!(parse_message(MSG_FRAME, vec![1; 9]).is_err(), "a frame needs a sample");
        assert!(parse_message(9, vec![]).is_err(), "unknown kinds end the stream");

        assert!(matches!(parse_message(MSG_STATUS, br#"{"ready":true}"#.to_vec()), Ok(Some(ScreenEvent::Ready))));
        assert!(matches!(
            parse_message(MSG_STATUS, br#"{"error":"bad line"}"#.to_vec()),
            Ok(Some(ScreenEvent::Warning(w))) if w == "bad line"
        ));
    }
}
