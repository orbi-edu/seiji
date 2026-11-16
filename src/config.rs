use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rmcp::schemars;
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Ios,
    Android,
}

impl Platform {
    pub fn as_str(self) -> &'static str {
        match self {
            Platform::Ios => "ios",
            Platform::Android => "android",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "ios" => Ok(Platform::Ios),
            "android" => Ok(Platform::Android),
            other => bail!("unknown platform {other:?}"),
        }
    }
}

/// How tenant commands (simctl, emulator, maestro) are executed on the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationMode {
    /// Every tenant is a dedicated macOS account; commands run as that account through
    /// `sudo seiji exec-as`, so simulators, files and processes are separated by the OS.
    OsUser,
    /// Every command runs as the daemon's own account; separation is enforced only by Seiji.
    Shared,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Isolation {
    pub mode: IsolationMode,
    /// macOS account name = prefix + Seiji user name.
    #[serde(default = "default_user_prefix")]
    pub user_prefix: String,
    /// Absolute path of the seiji binary that sudoers allows (`exec-as`, `kill-as`).
    #[serde(default = "default_helper")]
    pub helper: PathBuf,
}

fn default_user_prefix() -> String {
    "seiji-".into()
}
fn default_helper() -> PathBuf {
    PathBuf::from("/usr/local/bin/seiji")
}

#[derive(Debug, Clone, Copy, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Quota {
    /// Device-minutes per calendar month (UTC). Boot time counts: the device holds host resources while booting.
    pub monthly_minutes: i64,
    /// Devices one user may hold at the same time.
    pub max_concurrent: i64,
    /// Hard cap on one lease's wall-clock length.
    pub max_lease_minutes: i64,
    /// A lease with no tool call for this long is released.
    pub idle_minutes: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Listen {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// `Host` header values the MCP endpoint accepts (DNS-rebinding guard). Add your public hostname.
    #[serde(default = "default_allowed_hosts")]
    pub allowed_hosts: Vec<String>,
    /// Base URL agents use to reach Seiji, e.g. `https://seiji.example.com`. Download links in
    /// tool results are absolute when it is set and server-relative otherwise.
    pub public_url: Option<String>,
}

fn default_host() -> String {
    "127.0.0.1".into()
}
fn default_port() -> u16 {
    4780
}
fn default_allowed_hosts() -> Vec<String> {
    vec!["localhost".into(), "127.0.0.1".into()]
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Capacity {
    pub ios: i64,
    pub android: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IosModel {
    /// e.g. `com.apple.CoreSimulator.SimDeviceType.iPhone-16`
    pub device_type: String,
    /// e.g. `com.apple.CoreSimulator.SimRuntime.iOS-18-5`
    pub runtime: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Ios {
    pub models: BTreeMap<String, IosModel>,
    pub default_model: String,
    #[serde(default = "default_ios_boot_timeout")]
    pub boot_timeout_seconds: u64,
}

fn default_ios_boot_timeout() -> u64 {
    180
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AndroidModel {
    pub avd: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Android {
    pub sdk_root: PathBuf,
    /// AVD directory readable by every tenant (emulators run with `-read-only`, so it is never written).
    pub avd_home: PathBuf,
    pub models: BTreeMap<String, AndroidModel>,
    pub default_model: String,
    /// Even console ports reserved for Seiji's emulators.
    #[serde(default = "default_console_ports")]
    pub console_port_range: (u16, u16),
    #[serde(default = "default_android_boot_timeout")]
    pub boot_timeout_seconds: u64,
    /// Extra emulator flags. The default renders on the CPU, which works on a headless host.
    #[serde(default = "default_emulator_args")]
    pub emulator_args: Vec<String>,
}

fn default_emulator_args() -> Vec<String> {
    ["-gpu", "swiftshader_indirect", "-no-audio", "-no-boot-anim"].map(String::from).to_vec()
}

fn default_console_ports() -> (u16, u16) {
    (5600, 5640)
}
fn default_android_boot_timeout() -> u64 {
    240
}

impl Android {
    pub fn adb(&self) -> PathBuf {
        self.sdk_root.join("platform-tools/adb")
    }
    pub fn emulator(&self) -> PathBuf {
        self.sdk_root.join("emulator/emulator")
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Maestro {
    #[serde(default = "default_maestro_bin")]
    pub bin: PathBuf,
    /// JAVA_HOME passed to Maestro; tenant accounts have no shell profile.
    pub java_home: Option<PathBuf>,
    #[serde(default = "default_flow_timeout")]
    pub flow_timeout_seconds: u64,
}

fn default_maestro_bin() -> PathBuf {
    PathBuf::from("/opt/maestro/bin/maestro")
}
fn default_flow_timeout() -> u64 {
    600
}

/// AXe drives iOS simulators through the accessibility tree (taps, typing, gestures).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Axe {
    pub bin: PathBuf,
    /// Longest single AXe command (one action).
    pub step_timeout_seconds: u64,
}

impl Default for Axe {
    fn default() -> Self {
        Self { bin: PathBuf::from("/opt/homebrew/bin/axe"), step_timeout_seconds: 60 }
    }
}

/// Recorded runs: what is kept, how much, and for how long.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Evidence {
    /// Runs and their files are deleted this long after they end.
    pub retention_hours: i64,
    /// Total evidence one user may keep; a new run is refused above it.
    pub max_bytes_per_user: u64,
    /// Files one run may keep; files past it are listed as truncated in the manifest.
    pub max_run_bytes: u64,
    /// Longest `run_actions` run, all steps included.
    pub max_run_seconds: u64,
    /// Most steps in one `run_actions` call.
    pub max_steps: usize,
}

impl Default for Evidence {
    fn default() -> Self {
        Self {
            retention_hours: 168,
            max_bytes_per_user: 5 * 1024 * 1024 * 1024,
            max_run_bytes: 2 * 1024 * 1024 * 1024,
            max_run_seconds: 1800,
            max_steps: 200,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Xctest {
    /// `run_xctest` executes the uploaded test code as the tenant; set false to refuse it.
    pub enabled: bool,
    /// Longest `xcodebuild test-without-building` run.
    pub timeout_seconds: u64,
}

impl Default for Xctest {
    fn default() -> Self {
        Self { enabled: true, timeout_seconds: 3600 }
    }
}

/// The live screen viewer (`/view/…`), opened from Tern or from a `share_screen` link.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Viewer {
    /// Frames per second of the stream, 1-30.
    pub fps: u8,
    /// Frame scale, 0.1-1 (0.5 halves each side of the simulator's pixels).
    pub scale: f64,
    /// JPEG quality, 1-100.
    pub quality: u8,
    /// Open streams per lease.
    pub max_viewers_per_lease: usize,
    /// A viewer session lasts at most this long (and never past its lease).
    pub session_hours: i64,
}

impl Default for Viewer {
    fn default() -> Self {
        Self { fps: 10, scale: 0.5, quality: 70, max_viewers_per_lease: 4, session_hours: 12 }
    }
}

/// `seiji forward`: tunnels from a lease's host loopback to a port on the developer's machine
/// (a Metro bundler, for one).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Forward {
    /// Off: `/v1/leases/{id}/forward` answers 403.
    pub enabled: bool,
    /// Open tunnels per lease.
    pub max_per_lease: usize,
}

impl Default for Forward {
    fn default() -> Self {
        Self { enabled: true, max_per_lease: 2 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_listen")]
    pub listen: Listen,
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    /// Maximum number of Seiji users (seats).
    pub seats: i64,
    /// Price of one device-hour, used to estimate spend.
    pub cost_per_device_hour_usd: Option<f64>,
    /// Host-wide simultaneous devices per platform. Rule of thumb: ~1.5 GiB RAM per iOS sim, ~4 GiB per emulator.
    pub capacity: Capacity,
    /// Quota given to `seiji user add` when flags are omitted.
    pub default_quota: Quota,
    pub isolation: Isolation,
    pub ios: Option<Ios>,
    pub android: Option<Android>,
    /// Optional: only `run_flow` and Android screen inspection need Maestro.
    pub maestro: Option<Maestro>,
    #[serde(default)]
    pub axe: Axe,
    #[serde(default)]
    pub evidence: Evidence,
    #[serde(default)]
    pub xctest: Xctest,
    #[serde(default)]
    pub viewer: Viewer,
    #[serde(default)]
    pub forward: Forward,
    #[serde(default = "default_max_upload")]
    pub max_upload_bytes: u64,
    /// Uploaded and downloaded builds are deleted this long after they arrive.
    #[serde(default = "default_app_retention")]
    pub app_retention_hours: i64,
    /// Builds one user may keep at the same time; older ones expire with `appRetentionHours`.
    #[serde(default = "default_max_builds")]
    pub max_builds_per_user: i64,
}

fn default_listen() -> Listen {
    Listen { host: default_host(), port: default_port(), allowed_hosts: default_allowed_hosts(), public_url: None }
}
fn default_data_dir() -> PathBuf {
    PathBuf::from("./data")
}
fn default_max_upload() -> u64 {
    1024 * 1024 * 1024
}
fn default_app_retention() -> i64 {
    72
}
fn default_max_builds() -> i64 {
    10
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let base = path.parent().unwrap_or(Path::new("."));
        Self::parse(&text, base)
    }

    pub fn parse(text: &str, base_dir: &Path) -> Result<Self> {
        let mut config: Config = serde_json::from_str(text).context("parsing config")?;
        if let Some(ios) = &config.ios
            && !ios.models.contains_key(&ios.default_model)
        {
            bail!("ios.defaultModel {:?} is not in ios.models", ios.default_model);
        }
        if let Some(android) = &config.android {
            if !android.models.contains_key(&android.default_model) {
                bail!("android.defaultModel {:?} is not in android.models", android.default_model);
            }
            let (lo, hi) = android.console_port_range;
            // The emulator accepts even console ports 5554-5682; the adb port is console + 1.
            if lo % 2 != 0 || hi <= lo || lo < 5554 || hi > 5682 {
                bail!("android.consolePortRange must be [even, larger] within 5554-5682");
            }
            if android.boot_timeout_seconds == 0 {
                bail!("android.bootTimeoutSeconds must be > 0");
            }
        }
        if config.ios.as_ref().is_some_and(|ios| ios.boot_timeout_seconds == 0) {
            bail!("ios.bootTimeoutSeconds must be > 0");
        }
        if config.seats < 1 {
            bail!("seats must be >= 1");
        }
        if config.capacity.ios < 0 || config.capacity.android < 0 {
            bail!("capacity values must be >= 0");
        }
        let q = config.default_quota;
        if q.monthly_minutes < 0 || q.max_concurrent < 1 || q.max_lease_minutes < 1 || q.idle_minutes < 1 {
            bail!("defaultQuota: monthlyMinutes must be >= 0 and the other limits >= 1");
        }
        if config.listen.allowed_hosts.is_empty() {
            // An empty list would switch the DNS-rebinding check off entirely.
            bail!("listen.allowedHosts must not be empty");
        }
        if config.maestro.as_ref().is_some_and(|m| m.flow_timeout_seconds == 0)
            || config.app_retention_hours < 1
            || config.max_builds_per_user < 1
        {
            bail!("maestro.flowTimeoutSeconds, appRetentionHours and maxBuildsPerUser must be > 0");
        }
        let ev = &config.evidence;
        if ev.retention_hours < 1
            || ev.max_run_seconds == 0
            || ev.max_steps == 0
            || ev.max_run_bytes == 0
            || ev.max_bytes_per_user == 0
        {
            bail!("evidence.retentionHours, maxRunSeconds, maxSteps, maxRunBytes and maxBytesPerUser must be > 0");
        }
        if config.axe.step_timeout_seconds == 0 || config.xctest.timeout_seconds == 0 {
            bail!("axe.stepTimeoutSeconds and xctest.timeoutSeconds must be > 0");
        }
        let v = &config.viewer;
        if !(1..=30).contains(&v.fps)
            || !(0.1..=1.0).contains(&v.scale)
            || !(1..=100).contains(&v.quality)
            || v.max_viewers_per_lease == 0
            || v.session_hours < 1
        {
            bail!("viewer: fps 1-30, scale 0.1-1, quality 1-100, maxViewersPerLease and sessionHours must be > 0");
        }
        if config.forward.max_per_lease == 0 {
            bail!("forward.maxPerLease must be > 0");
        }
        if let Some(url) = &config.listen.public_url
            && !(url.starts_with("https://") || url.starts_with("http://"))
        {
            bail!("listen.publicUrl must start with https:// or http://");
        }
        if config.data_dir.is_relative() {
            config.data_dir = base_dir.join(&config.data_dir);
        }
        Ok(config)
    }

    pub fn capacity(&self, platform: Platform) -> i64 {
        match platform {
            Platform::Ios if self.ios.is_some() => self.capacity.ios,
            Platform::Android if self.android.is_some() => self.capacity.android,
            _ => 0,
        }
    }

    pub fn models(&self, platform: Platform) -> Vec<String> {
        match platform {
            Platform::Ios => self.ios.iter().flat_map(|c| c.models.keys().cloned()).collect(),
            Platform::Android => self.android.iter().flat_map(|c| c.models.keys().cloned()).collect(),
        }
    }

    pub fn default_model(&self, platform: Platform) -> Option<&str> {
        match platform {
            Platform::Ios => self.ios.as_ref().map(|c| c.default_model.as_str()),
            Platform::Android => self.android.as_ref().map(|c| c.default_model.as_str()),
        }
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("seiji.db")
    }
}
