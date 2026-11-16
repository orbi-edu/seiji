//! MCP tool surface. One instance per HTTP request; the caller's identity comes from the bearer
//! token, resolved by the auth middleware into an [`AuthedUser`] request extension.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use base64::Engine;
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    schemars,
    service::RequestContext,
    tool, tool_handler, tool_router,
};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::OnceCell;

use crate::actions::{Action, valid_url};
use crate::config::Platform;
use crate::devices::{Appearance, DeviceOp, PrivacyAction, PrivacyService, StatusBar, XctestOptions};
use crate::evidence;
use crate::exec::{head, tail};
use crate::pool::{AppSource, Pool};
use crate::runs::{RunOptions, RunResult};
use crate::store::User;

const CHEAT_SHEET_URL: &str = "https://api.copilot.mobile.dev/v2/bot/maestro-cheat-sheet";
const MAX_TEXT: usize = 200_000;
/// Text artifacts up to this size come back inline; larger ones as a link.
const MAX_INLINE_ARTIFACT: u64 = 200_000;
/// APNs refuses payloads above 4 KB.
const MAX_PUSH_PAYLOAD: usize = 4096;

#[derive(Clone)]
pub struct AuthedUser(pub User);

const INSTRUCTIONS: &str = "Seiji lends you a fresh iOS simulator (or Android emulator) on a shared Mac and records evidence of what you do on it.

Devices are metered: every minute you hold one counts against your monthly budget, so release it when done.

Workflow:
1. `budget_status` / `list_device_models` to see what you can use.
2. `acquire_device` (platform `ios`) boots a device just for you (30-90 s) and returns a `lease_id`.
3. Upload your build: `curl -H \"Authorization: Bearer $SEIJI_TOKEN\" -H \"X-Filename: MyApp.zip\" --data-binary @MyApp.zip <server>/v1/apps` (iOS: zip/tar.gz of the simulator .app), then `install_app` with the returned `app_id`.
4. Optional: `configure_device` (status bar, dark mode, location, permissions) for clean, reproducible screenshots.
5. `inspect_screen` shows the accessibility tree: use its exact labels and ids in selectors.
6. `run_actions` runs a list of steps (tap, type, scroll, wait_for, assert_visible, screenshot, launch_app, ...) and records a video, the app's logs, crash reports and a screenshot per step. It returns a `run_id`.
   `run_xctest` runs a compiled XCUITest bundle (`build-for-testing`) and keeps the `.xcresult`. `run_flow` runs Maestro YAML when the host has Maestro.
7. Evidence: `get_run` (steps and files), `get_run_artifact` (a screenshot, the log, ...), `export_evidence` (a Markdown summary for the pull request and a download link for evidence.zip).
8. `release_device` when finished. Idle devices are released automatically; runs stay for the retention period.

`lease_id` may be omitted when you hold exactly one device. `bundle_id` defaults to the app installed last on the lease.";

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AcquireArgs {
    /// `ios` or `android`.
    pub platform: Platform,
    /// Device model from `list_device_models`; the host default when omitted.
    pub model: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LeaseArgs {
    /// Lease from `acquire_device`; optional when you hold exactly one device.
    pub lease_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct InstallArgs {
    /// Lease from `acquire_device`; optional when you hold exactly one device.
    pub lease_id: Option<String>,
    /// `app_id` returned by `POST /v1/apps`.
    pub app_id: Option<String>,
    /// https URL of the build (iOS: .zip/.tar.gz of a simulator .app; Android: .apk).
    pub url: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AppArgs {
    pub lease_id: Option<String>,
    /// Defaults to the app installed last on this lease.
    pub bundle_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LaunchArgs {
    pub lease_id: Option<String>,
    /// Defaults to the app installed last on this lease.
    pub bundle_id: Option<String>,
    /// Launch arguments passed to the app.
    #[serde(default)]
    pub arguments: Vec<String>,
    /// Kill a running instance first (default true).
    pub restart: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UrlArgs {
    pub lease_id: Option<String>,
    /// Deep link or web URL.
    pub url: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct Location {
    pub latitude: f64,
    pub longitude: f64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct Permission {
    pub action: PrivacyAction,
    pub service: PrivacyService,
    /// Defaults to the app installed last on this lease.
    pub bundle_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct Bundler {
    /// Defaults to the app installed last on this lease.
    pub bundle_id: Option<String>,
    /// `host:port` of the Metro bundler as the simulator sees it, e.g. the address
    /// `seiji forward` prints (`127.0.0.1:52314`). Omit to go back to the app's default.
    pub location: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfigureArgs {
    pub lease_id: Option<String>,
    /// Fixed status bar values; `clear_status_bar` removes them.
    pub status_bar: Option<StatusBar>,
    #[serde(default)]
    pub clear_status_bar: bool,
    pub appearance: Option<Appearance>,
    /// Simulated GPS position; `clear_location` removes it.
    pub location: Option<Location>,
    #[serde(default)]
    pub clear_location: bool,
    /// Grant, revoke or reset privacy permissions (no system prompt appears for granted ones).
    #[serde(default)]
    pub permissions: Vec<Permission>,
    /// React Native debug builds: where the app loads its JavaScript bundle from (the
    /// `RCT_jsLocation` default). Relaunch the app afterwards.
    pub react_native_bundler: Option<Bundler>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PushArgs {
    pub lease_id: Option<String>,
    /// Defaults to the app installed last on this lease.
    pub bundle_id: Option<String>,
    /// APNs payload, e.g. `{"aps": {"alert": {"title": "Hi", "body": "There"}}}` (max 4 KB).
    pub payload: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LogsArgs {
    pub lease_id: Option<String>,
    /// How far back (default 300, max 3600).
    pub last_seconds: Option<u64>,
    /// Unified-log predicate; defaults to the installed app's process and subsystem.
    pub predicate: Option<String>,
}

/// Options every recorded run takes.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct RunCommon {
    /// Lease from `acquire_device`; optional when you hold exactly one device.
    pub lease_id: Option<String>,
    /// Title shown in the evidence, e.g. "Login with a valid account".
    pub name: Option<String>,
    /// Labels for `list_runs`, e.g. `["smoke", "PR-123"]` ([A-Za-z0-9_-], at most 10).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Record the screen (default true).
    pub record_video: Option<bool>,
    /// Stream the app's unified log into `app.log` (default true).
    pub capture_logs: Option<bool>,
    /// Log predicate; defaults to the installed app's process and subsystem.
    pub log_predicate: Option<String>,
}

impl RunCommon {
    fn options(&self) -> RunOptions {
        RunOptions {
            name: self.name.clone(),
            tags: self.tags.clone(),
            record_video: self.record_video.unwrap_or(true),
            capture_logs: self.capture_logs.unwrap_or(true),
            log_predicate: self.log_predicate.clone(),
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RunActionsArgs {
    #[serde(flatten)]
    pub common: RunCommon,
    /// Steps, each an object with one key, e.g.
    /// `[{"launch_app": {}}, {"tap": {"label": "Sign In"}}, {"type": {"text": "ana@example.com"}},
    /// {"wait_for": {"label": "Welcome"}}, {"screenshot": {"name": "home"}}]`.
    pub actions: Vec<Action>,
    /// A screenshot after every step (default true). Failed steps always get one.
    pub screenshot_each_step: Option<bool>,
    /// Run the remaining steps after a failure (default false).
    #[serde(default)]
    pub continue_on_error: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RunFlowArgs {
    #[serde(flatten)]
    pub common: RunCommon,
    /// Maestro flow YAML: config section (`appId: com.example`), `---`, then commands.
    pub yaml: String,
    /// Variables available to the flow as `${NAME}`. Only the keys are kept in the evidence.
    pub env: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RunXctestArgs {
    #[serde(flatten)]
    pub common: RunCommon,
    /// `app_id` of a .zip/.tar.gz of the `build-for-testing` Products folder (the `.xctestrun`
    /// file and the `Debug-iphonesimulator` folder with the app and its test runner).
    pub app_id: String,
    /// `.xctestrun` file name; the first one in the bundle when omitted.
    pub xctestrun: Option<String>,
    /// `Target[/Class[/method]]` to run only these.
    #[serde(default)]
    pub only_testing: Vec<String>,
    #[serde(default)]
    pub skip_testing: Vec<String>,
    /// Test plan configuration name.
    pub test_configuration: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListRunsArgs {
    /// `passed`, `failed`, `error`, `cancelled` or `running`.
    pub status: Option<String>,
    pub tag: Option<String>,
    /// Default 20, max 200.
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RunArgs {
    pub run_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ShareArgs {
    pub lease_id: Option<String>,
    /// Let the person tap and type too (default false: watch only).
    #[serde(default)]
    pub control: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ArtifactArgs {
    pub run_id: String,
    /// A path from the run's file list, e.g. `steps/003-tap.png`, `app.log`, `video.mp4`.
    pub path: String,
}

#[derive(Clone)]
pub struct SeijiMcp {
    pool: Arc<Pool>,
    cheat_sheet: Arc<OnceCell<String>>,
    tool_router: ToolRouter<SeijiMcp>,
}

fn caller(ctx: &RequestContext<RoleServer>) -> Result<User, McpError> {
    ctx.extensions
        .get::<axum::http::request::Parts>()
        .and_then(|p| p.extensions.get::<AuthedUser>())
        .map(|u| u.0.clone())
        .ok_or_else(|| McpError::invalid_request("unauthenticated", None))
}

fn fail(e: anyhow::Error) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(format!("{e:#}"))])
}

/// `host:port` with a plain host name or address and a port 1-65535.
fn valid_host_port(s: &str) -> bool {
    let Some((host, port)) = s.rsplit_once(':') else { return false };
    !host.is_empty()
        && host.len() <= 253
        && host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        && port.parse::<u16>().is_ok_and(|p| p > 0)
}

fn json(value: &impl serde::Serialize) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(serde_json::to_string_pretty(value).unwrap_or_default())])
}

fn text(s: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(s)])
}

fn jpeg(bytes: &[u8]) -> ContentBlock {
    ContentBlock::image(base64::engine::general_purpose::STANDARD.encode(bytes), "image/jpeg")
}

/// Compact run summary: verdict, one line per step, where the evidence is.
fn run_result(pool: &Pool, r: RunResult) -> CallToolResult {
    let m = &r.manifest;
    let steps: Vec<serde_json::Value> = m
        .steps
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| {
            let what = s.get("action").and_then(|a| a.as_object()).and_then(|o| o.keys().next().cloned());
            json!({
                "index": s.get("index"),
                "step": what.or_else(|| s.get("name").and_then(|v| v.as_str()).map(String::from)),
                "status": s.get("status"),
                "duration_ms": s.get("duration_ms"),
                "error": s.get("error"),
                "files": s.get("files"),
            })
        })
        .collect();
    let url = pool.run_url(&m.run_id);
    let summary = json!({
        "run_id": m.run_id,
        "status": m.status,
        "verdict": m.verdict,
        "steps": steps,
        "files": m.files.len(),
        "video": m.files.iter().any(|f| f.path == "video.mp4"),
        "crash_reports": m.files.iter().filter(|f| f.path.starts_with("crashes/")).count(),
        "dropped_files": m.dropped.len(),
        "warnings": m.warnings,
        "manifest_sha256": r.manifest_sha256,
        "evidence_zip": format!("{url}/evidence.zip"),
        "next": "get_run for every file, get_run_artifact to look at one, export_evidence for a PR summary",
    });
    let mut content = vec![ContentBlock::text(serde_json::to_string_pretty(&summary).unwrap_or_default())];
    content.extend(r.screenshot.as_deref().map(jpeg));
    if m.status == "passed" { CallToolResult::success(content) } else { CallToolResult::error(content) }
}

#[tool_router]
impl SeijiMcp {
    pub fn new(pool: Arc<Pool>, cheat_sheet: Arc<OnceCell<String>>) -> Self {
        Self { pool, cheat_sheet, tool_router: Self::tool_router() }
    }

    #[tool(description = "Your device-minute budget for this month, limits, and the devices you hold.")]
    async fn budget_status(&self, ctx: RequestContext<RoleServer>) -> Result<CallToolResult, McpError> {
        let user = caller(&ctx)?;
        Ok(self.pool.budget(&user).map_or_else(fail, |b| json(&b)))
    }

    #[tool(description = "Platforms and device models this host offers, with current occupancy.")]
    async fn list_device_models(&self, ctx: RequestContext<RoleServer>) -> Result<CallToolResult, McpError> {
        caller(&ctx)?;
        Ok(self.pool.models().map_or_else(fail, |m| json(&m)))
    }

    #[tool(
        description = "Boot a fresh iOS simulator or Android emulator reserved for you. Takes 30-90 s. Metered until released."
    )]
    async fn acquire_device(
        &self,
        Parameters(args): Parameters<AcquireArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let user = caller(&ctx)?;
        Ok(self.pool.acquire(&user, args.platform, args.model).await.map_or_else(fail, |l| json(&l)))
    }

    #[tool(description = "Release a device you hold; it is deleted and stops counting against your budget.")]
    async fn release_device(
        &self,
        Parameters(args): Parameters<LeaseArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let user = caller(&ctx)?;
        Ok(self
            .pool
            .release_for(&user, args.lease_id.as_deref())
            .await
            .map_or_else(fail, |id| text(format!("released {id}"))))
    }

    #[tool(
        description = "Install an app on your device from an uploaded `app_id` (POST /v1/apps) or an https `url`. Returns its bundle id, executable and version."
    )]
    async fn install_app(
        &self,
        Parameters(args): Parameters<InstallArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let user = caller(&ctx)?;
        let source = match (args.app_id, args.url) {
            (Some(id), None) => AppSource::Uploaded(id),
            (None, Some(url)) => AppSource::Url(url),
            _ => return Ok(fail(anyhow::anyhow!("pass exactly one of app_id or url"))),
        };
        Ok(self.pool.install(&user, args.lease_id.as_deref(), source).await.map_or_else(fail, |a| json(&a)))
    }

    #[tool(description = "iOS: remove an app from your simulator.")]
    async fn uninstall_app(
        &self,
        Parameters(args): Parameters<AppArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let user = caller(&ctx)?;
        let lease = args.lease_id.as_deref();
        let r = async {
            let bundle_id = self.pool.app_or_default(&user, lease, args.bundle_id)?;
            self.pool.device_op(&user, lease, DeviceOp::Uninstall { bundle_id: bundle_id.clone() }).await?;
            anyhow::Ok(format!("uninstalled {bundle_id}"))
        }
        .await;
        Ok(r.map_or_else(fail, text))
    }

    #[tool(description = "iOS: launch an app (restarts it by default).")]
    async fn launch_app(
        &self,
        Parameters(args): Parameters<LaunchArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let user = caller(&ctx)?;
        let lease = args.lease_id.as_deref();
        let r = async {
            let bundle_id = self.pool.app_or_default(&user, lease, args.bundle_id)?;
            let op = DeviceOp::Launch { bundle_id, arguments: args.arguments, restart: args.restart.unwrap_or(true) };
            self.pool.device_op(&user, lease, op).await
        }
        .await;
        Ok(r.map_or_else(fail, |out| text(if out.is_empty() { "launched".into() } else { out })))
    }

    #[tool(description = "iOS: terminate a running app.")]
    async fn terminate_app(
        &self,
        Parameters(args): Parameters<AppArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let user = caller(&ctx)?;
        let lease = args.lease_id.as_deref();
        let r = async {
            let bundle_id = self.pool.app_or_default(&user, lease, args.bundle_id)?;
            self.pool.device_op(&user, lease, DeviceOp::Terminate { bundle_id: bundle_id.clone() }).await?;
            anyhow::Ok(format!("terminated {bundle_id}"))
        }
        .await;
        Ok(r.map_or_else(fail, text))
    }

    #[tool(description = "iOS: open a deep link or web URL on your simulator.")]
    async fn open_url(
        &self,
        Parameters(args): Parameters<UrlArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let user = caller(&ctx)?;
        let r = async {
            valid_url(&args.url)?;
            self.pool.device_op(&user, args.lease_id.as_deref(), DeviceOp::OpenUrl(args.url.clone())).await?;
            anyhow::Ok(format!("opened {}", args.url))
        }
        .await;
        Ok(r.map_or_else(fail, text))
    }

    #[tool(
        description = "iOS: prepare the simulator natively (simctl): fixed status bar, light/dark appearance, simulated location, privacy permissions, the React Native bundler location (RCT_jsLocation; relaunch the app after). Applied in that order."
    )]
    async fn configure_device(
        &self,
        Parameters(args): Parameters<ConfigureArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let user = caller(&ctx)?;
        let lease = args.lease_id.as_deref();
        let r = async {
            let mut ops = Vec::new();
            if args.clear_status_bar {
                ops.push(DeviceOp::ClearStatusBar);
            }
            if let Some(sb) = args.status_bar {
                if sb.battery_level.is_some_and(|l| l > 100)
                    || sb.wifi_bars.is_some_and(|b| b > 3)
                    || sb.cellular_bars.is_some_and(|b| b > 4)
                    || sb.time.as_ref().is_some_and(|t| t.is_empty() || t.len() > 32)
                {
                    anyhow::bail!("status_bar: battery_level 0-100, wifi_bars 0-3, cellular_bars 0-4, time 1-32 chars");
                }
                ops.push(DeviceOp::StatusBar(sb));
            }
            if let Some(a) = args.appearance {
                ops.push(DeviceOp::Appearance(a));
            }
            if args.clear_location {
                ops.push(DeviceOp::Location(None));
            }
            if let Some(l) = args.location {
                if !(-90.0..=90.0).contains(&l.latitude) || !(-180.0..=180.0).contains(&l.longitude) {
                    anyhow::bail!("location: latitude -90..90, longitude -180..180");
                }
                ops.push(DeviceOp::Location(Some((l.latitude, l.longitude))));
            }
            for p in args.permissions {
                let bundle_id = self.pool.app_or_default(&user, lease, p.bundle_id)?;
                ops.push(DeviceOp::Privacy { action: p.action, service: p.service, bundle_id });
            }
            if let Some(b) = args.react_native_bundler {
                if let Some(loc) = &b.location
                    && !valid_host_port(loc)
                {
                    anyhow::bail!("react_native_bundler.location must be host:port, e.g. 127.0.0.1:52314");
                }
                let bundle_id = self.pool.app_or_default(&user, lease, b.bundle_id)?;
                ops.push(DeviceOp::Bundler { bundle_id, location: b.location });
            }
            if ops.is_empty() {
                anyhow::bail!("nothing to configure");
            }
            let mut done = Vec::new();
            for op in ops {
                let name = op.name();
                self.pool.device_op(&user, lease, op).await.context(name)?;
                done.push(name);
            }
            anyhow::Ok(format!("applied: {}", done.join(", ")))
        }
        .await;
        Ok(r.map_or_else(fail, text))
    }

    #[tool(description = "iOS: deliver a push notification payload to an app on your simulator.")]
    async fn push_notification(
        &self,
        Parameters(args): Parameters<PushArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let user = caller(&ctx)?;
        let lease = args.lease_id.as_deref();
        let r = async {
            let bundle_id = self.pool.app_or_default(&user, lease, args.bundle_id)?;
            let payload = serde_json::to_vec(&args.payload)?;
            if payload.len() > MAX_PUSH_PAYLOAD {
                anyhow::bail!("payload is {} bytes; APNs allows {MAX_PUSH_PAYLOAD}", payload.len());
            }
            let file = self.pool.config.data_dir.join("run").join(format!("{}.apns", uuid::Uuid::new_v4().simple()));
            std::fs::write(&file, payload)?;
            let sent = self
                .pool
                .device_op(&user, lease, DeviceOp::Push { bundle_id: bundle_id.clone(), payload_file: file.clone() })
                .await;
            let _ = std::fs::remove_file(&file);
            sent?;
            anyhow::Ok(format!("pushed to {bundle_id}"))
        }
        .await;
        Ok(r.map_or_else(fail, text))
    }

    #[tool(description = "iOS: recent unified-log lines from your simulator, by default the installed app's.")]
    async fn get_logs(
        &self,
        Parameters(args): Parameters<LogsArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let user = caller(&ctx)?;
        let r = self.pool.logs(&user, args.lease_id.as_deref(), args.last_seconds.unwrap_or(300), args.predicate).await;
        Ok(r.map_or_else(fail, |t| text(if t.trim().is_empty() { "no matching log lines".into() } else { t })))
    }

    #[tool(
        description = "The current screen. iOS: the accessibility tree, one element per line (type, label, id, value, frame in points); use these exact labels and ids in selectors. Android: Maestro's hierarchy."
    )]
    async fn inspect_screen(
        &self,
        Parameters(args): Parameters<LeaseArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let user = caller(&ctx)?;
        Ok(self.pool.hierarchy(&user, args.lease_id.as_deref()).await.map_or_else(fail, |t| text(head(&t, MAX_TEXT))))
    }

    #[tool(description = "Screenshot of your device (JPEG, longest side <= 2000 px).")]
    async fn take_screenshot(
        &self,
        Parameters(args): Parameters<LeaseArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let user = caller(&ctx)?;
        Ok(match self.pool.screenshot(&user, args.lease_id.as_deref()).await {
            Ok(bytes) => CallToolResult::success(vec![jpeg(&bytes)]),
            Err(e) => fail(e),
        })
    }

    #[tool(
        description = "iOS: a one-time link (valid 60 s) for a person to watch your simulator live in a browser, or in Tern. View-only unless control is true."
    )]
    async fn share_screen(
        &self,
        Parameters(args): Parameters<ShareArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let user = caller(&ctx)?;
        let r = self.pool.create_viewer(&user, args.lease_id.as_deref(), args.control);
        Ok(r.map_or_else(fail, |link| json(&link)))
    }

    #[tool(
        description = "iOS: run a sequence of UI steps and record evidence (video, app log, crash reports, a screenshot per step, the accessibility tree at a failure). Stops at the first failed step unless continue_on_error. Returns a run_id; failed runs are isError with a screenshot."
    )]
    async fn run_actions(
        &self,
        Parameters(args): Parameters<RunActionsArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let user = caller(&ctx)?;
        let r = self
            .pool
            .run_actions(
                &user,
                args.common.lease_id.as_deref(),
                args.common.options(),
                args.actions,
                args.screenshot_each_step.unwrap_or(true),
                args.continue_on_error,
            )
            .await;
        Ok(r.map_or_else(fail, |r| run_result(&self.pool, r)))
    }

    #[tool(
        description = "iOS: run a compiled XCUITest bundle (xcodebuild test-without-building) on your simulator. Keeps the .xcresult, per-test results, attachments, video, logs and crash reports. Returns a run_id."
    )]
    async fn run_xctest(
        &self,
        Parameters(args): Parameters<RunXctestArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let user = caller(&ctx)?;
        let opts = XctestOptions {
            xctestrun: args.xctestrun,
            only_testing: args.only_testing,
            skip_testing: args.skip_testing,
            test_configuration: args.test_configuration,
        };
        let r = self
            .pool
            .run_xctest(&user, args.common.lease_id.as_deref(), args.common.options(), &args.app_id, opts)
            .await;
        Ok(r.map_or_else(fail, |r| run_result(&self.pool, r)))
    }

    #[tool(
        description = "Run a Maestro flow (inline YAML) and record it like run_actions. Needs Maestro on the host. Call cheat_sheet before writing unfamiliar Maestro commands."
    )]
    async fn run_flow(
        &self,
        Parameters(args): Parameters<RunFlowArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let user = caller(&ctx)?;
        let r = self
            .pool
            .run_flow(
                &user,
                args.common.lease_id.as_deref(),
                args.common.options(),
                &args.yaml,
                args.env.unwrap_or_default(),
            )
            .await;
        Ok(r.map_or_else(fail, |r| run_result(&self.pool, r)))
    }

    #[tool(description = "Your recorded runs, newest first.")]
    async fn list_runs(
        &self,
        Parameters(args): Parameters<ListRunsArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let user = caller(&ctx)?;
        let r = self.pool.list_runs(&user, args.status.as_deref(), args.tag.as_deref(), args.limit.unwrap_or(20));
        Ok(r.map_or_else(fail, |runs| json(&runs)))
    }

    #[tool(
        description = "A run's manifest: status, verdict, device, app, every step with its timing and error, and every file with its size and SHA-256."
    )]
    async fn get_run(
        &self,
        Parameters(args): Parameters<RunArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let user = caller(&ctx)?;
        Ok(match self.pool.run_manifest(&user, &args.run_id) {
            Ok((m, _)) => text(head(&serde_json::to_string_pretty(&m).unwrap_or_default(), MAX_TEXT)),
            Err(e) => fail(e),
        })
    }

    #[tool(
        description = "One file of a run. Images come back as a JPEG, small text files (logs, trees, JSON) inline, anything else (video, .xcresult) as a download link."
    )]
    async fn get_run_artifact(
        &self,
        Parameters(args): Parameters<ArtifactArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let user = caller(&ctx)?;
        let r = async {
            let (path, bytes) = self.pool.run_file(&user, &args.run_id, &args.path)?;
            let kind = evidence::content_type(&args.path);
            let url = format!("{}/files/{}", self.pool.run_url(&args.run_id), args.path);
            if kind.starts_with("image/") {
                return anyhow::Ok(CallToolResult::success(vec![jpeg(
                    &evidence::preview_jpeg(&path, &self.pool.config).await?,
                )]));
            }
            let textual = kind.starts_with("text/")
                || kind == "application/json"
                || kind == "application/xml"
                || kind == "application/yaml";
            if textual && bytes <= MAX_INLINE_ARTIFACT {
                return Ok(text(String::from_utf8_lossy(&std::fs::read(&path)?).into_owned()));
            }
            if textual {
                let content = evidence::read_tail(&path, MAX_INLINE_ARTIFACT)?;
                return Ok(text(format!(
                    "{bytes} bytes; last {MAX_INLINE_ARTIFACT} shown. Full file: {url}\n\n{}",
                    tail(&content, MAX_INLINE_ARTIFACT as usize)
                )));
            }
            Ok(json(&json!({ "path": args.path, "bytes": bytes, "content_type": kind, "download": url })))
        }
        .await;
        Ok(r.unwrap_or_else(fail))
    }

    #[tool(
        description = "Evidence for a pull request: a Markdown summary (verdict, device, app, step table, links to the video, log, crash reports and failure screenshots) and the evidence.zip download link."
    )]
    async fn export_evidence(
        &self,
        Parameters(args): Parameters<RunArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let user = caller(&ctx)?;
        let r = (|| {
            let (manifest, dir) = self.pool.run_manifest(&user, &args.run_id)?;
            let (_, sha) = evidence::sha256_file(&dir.join(evidence::MANIFEST))?;
            anyhow::Ok(evidence::markdown(&manifest, &self.pool.run_url(&args.run_id), &sha))
        })();
        Ok(r.map_or_else(fail, text))
    }

    #[tool(
        description = "Maestro cheat sheet: flow syntax, commands and selector rules. Read before writing unfamiliar flows."
    )]
    async fn cheat_sheet(&self, ctx: RequestContext<RoleServer>) -> Result<CallToolResult, McpError> {
        caller(&ctx)?;
        let sheet = self
            .cheat_sheet
            .get_or_try_init(|| async {
                let client = reqwest::Client::builder().timeout(Duration::from_secs(60)).build()?;
                let text = client.get(CHEAT_SHEET_URL).send().await?.error_for_status()?.text().await?;
                anyhow::Ok(text)
            })
            .await;
        Ok(match sheet {
            Ok(t) => text(t.clone()),
            Err(e) => fail(e.context("fetching the Maestro cheat sheet; see https://docs.maestro.dev/llms.txt")),
        })
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for SeijiMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("seiji", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}
