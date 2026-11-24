//! Test harness: real HTTP + MCP, the device layer faked (no simulators in CI), a stub `maestro`.

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, bail};
use async_trait::async_trait;
use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolRequestParams, CallToolResult, ClientCapabilities, ClientConfig, Implementation},
    service::RunningService,
    transport::{StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig},
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use seiji::config::{Config, Quota};
use seiji::devices::{
    Capture, DeviceCtx, DeviceOp, Devices, InstalledApp, ScreenCommand, ScreenEvent, ScreenFormat, StreamOpts, UiInput,
    VideoFrame, XctestOptions, XctestOutcome,
};
use seiji::exec::{Account, Runner};
use seiji::pool::Pool;
use seiji::store::Store;

/// A valid 1x1 PNG, so `sips` previews work in tests.
pub const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0xd, 0xa, 0x1a, 0xa, 0x0, 0x0, 0x0, 0xd, 0x49, 0x48, 0x44, 0x52, 0x0, 0x0, 0x0, 0x1, 0x0,
    0x0, 0x0, 0x1, 0x8, 0x2, 0x0, 0x0, 0x0, 0x90, 0x77, 0x53, 0xde, 0x0, 0x0, 0x0, 0xc, 0x49, 0x44, 0x41, 0x54, 0x78,
    0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0x0, 0x0, 0x3, 0x1, 0x1, 0x0, 0xc9, 0xfe, 0x92, 0xef, 0x0, 0x0, 0x0, 0x0, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

/// A simulator stand-in: a login screen that shows "Welcome, Ana!" after "Sign In" is tapped.
#[derive(Default)]
pub struct FakeDevices {
    pub created: AtomicUsize,
    pub destroyed: Mutex<Vec<String>>,
    pub installed: Mutex<Vec<String>>,
    pub fail_boot: AtomicBool,
    /// Boot blocks until the lease is released.
    pub hang_boot: AtomicBool,
    /// Notified (with a stored permit) when `boot()` enters the `hang_boot` wait.
    pub boot_entered: tokio::sync::Notify,
    /// Every input and operation, in order, e.g. `tap label=Sign In`, `type ana`, `op launch_app com.example.demo`.
    pub log: Mutex<Vec<String>>,
    pub signed_in: AtomicBool,
    pub stopped_captures: AtomicUsize,
    pub streams_started: AtomicUsize,
    /// JPEGs `screenshot` returns; a tiny placeholder when empty.
    pub frames: Mutex<Vec<Vec<u8>>>,
    /// A still screen: the helper sends frames only when asked for a keyframe.
    pub still: AtomicBool,
    /// `simctl pbcopy` fails, as under `os_user` isolation (no login session for the tenant).
    pub no_pasteboard: AtomicBool,
}

impl FakeDevices {
    pub fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
    fn note(&self, s: String) {
        self.log.lock().unwrap().push(s);
    }
}

#[async_trait]
impl Devices for FakeDevices {
    async fn create(&self, ctx: &DeviceCtx) -> Result<String> {
        std::fs::create_dir_all(ctx.dir())?;
        self.created.fetch_add(1, Ordering::SeqCst);
        Ok(format!("FAKE-{}", ctx.lease_id))
    }
    async fn boot(&self, _ctx: &DeviceCtx, cancel: &CancellationToken) -> Result<()> {
        if self.hang_boot.load(Ordering::SeqCst) {
            self.boot_entered.notify_one();
            cancel.cancelled().await;
            bail!("cancelled");
        }
        if self.fail_boot.load(Ordering::SeqCst) {
            bail!("runtime missing");
        }
        Ok(())
    }
    async fn destroy(&self, ctx: &DeviceCtx) {
        self.destroyed.lock().unwrap().push(ctx.lease_id.clone());
    }
    async fn install(&self, _ctx: &DeviceCtx, app: &std::path::Path, filename: &str) -> Result<InstalledApp> {
        let bytes = std::fs::read(app)?;
        self.installed.lock().unwrap().push(format!("{filename}:{}", String::from_utf8_lossy(&bytes)));
        Ok(InstalledApp {
            filename: filename.to_string(),
            bundle_id: Some("com.example.demo".into()),
            executable: Some("Demo".into()),
            version: Some("1.2.3".into()),
        })
    }
    async fn screenshot(&self, _ctx: &DeviceCtx) -> Result<Vec<u8>> {
        Ok(self.frames.lock().unwrap().first().cloned().unwrap_or_else(|| vec![0xff, 0xd8, 0xff, 0xd9]))
    }
    async fn cleanup_orphans(&self, _account: &Account, _keep: &[String]) {}

    async fn ui(&self, _ctx: &DeviceCtx, input: &UiInput) -> Result<()> {
        match input {
            UiInput::Tap(t) => {
                let label = t.label.clone().unwrap_or_default();
                if label == "Missing" {
                    bail!("tap failed (elementNotFound): No accessibility element matched --label 'Missing'.");
                }
                if label == "Sign In" {
                    self.signed_in.store(true, Ordering::SeqCst);
                }
                match (t.x, t.y) {
                    (Some(x), Some(y)) if t.label.is_none() => self.note(format!(
                        "tap {x:.0},{y:.0}{}",
                        t.hold_seconds.map(|h| format!(" hold {h:.1}")).unwrap_or_default()
                    )),
                    _ => self.note(format!("tap label={label}")),
                }
            }
            UiInput::Type { text_file } => self.note(format!("type {}", std::fs::read_to_string(text_file)?)),
            UiInput::Key(k) => self.note(format!("key {k}")),
            other => self.note(format!("{other:?}")),
        }
        Ok(())
    }
    async fn describe_ui(&self, _ctx: &DeviceCtx) -> Result<Value> {
        let mut children = vec![json!({"type": "Button", "AXLabel": "Sign In", "AXUniqueId": "signin",
            "frame": {"x": 20, "y": 700, "width": 362, "height": 50}})];
        if self.signed_in.load(Ordering::SeqCst) {
            children.push(json!({"type": "StaticText", "AXLabel": "Welcome, Ana!"}));
        }
        Ok(json!([{"type": "Application", "AXLabel": "Demo",
            "frame": {"x": 0, "y": 0, "width": 402, "height": 874}, "children": children}]))
    }
    async fn save_screenshot(&self, _ctx: &DeviceCtx, dest: &std::path::Path) -> Result<()> {
        std::fs::write(dest, PNG)?;
        Ok(())
    }
    async fn start_capture(&self, _ctx: &DeviceCtx, capture: &Capture) -> Result<String> {
        match capture {
            Capture::Video { dest } => std::fs::write(dest, b"fake mp4")?,
            Capture::Log { predicate, out } => std::fs::write(out, format!("predicate: {predicate}\nDemo: started\n"))?,
        }
        Ok(format!("cap-{}", uuid::Uuid::new_v4().simple()))
    }
    async fn stop_capture(&self, _ctx: &DeviceCtx, _handle: &str) {
        self.stopped_captures.fetch_add(1, Ordering::SeqCst);
    }
    async fn device_op(&self, _ctx: &DeviceCtx, op: &DeviceOp) -> Result<String> {
        let detail = match op {
            DeviceOp::Launch { bundle_id, .. }
            | DeviceOp::Terminate { bundle_id }
            | DeviceOp::Uninstall { bundle_id } => bundle_id.clone(),
            DeviceOp::Push { bundle_id, payload_file } => {
                format!("{bundle_id} {}", std::fs::read_to_string(payload_file)?)
            }
            DeviceOp::Pasteboard { text_file } => {
                if self.no_pasteboard.load(Ordering::SeqCst) {
                    anyhow::bail!("simctl pasteboard killed: ");
                }
                format!("pasteboard:{}", std::fs::read_to_string(text_file)?)
            }
            DeviceOp::Privacy { action, service, bundle_id } => {
                format!("{} {} {bundle_id}", action.as_str(), service.as_str())
            }
            DeviceOp::LogShow { predicate, .. } => {
                return Ok(format!("log for {}\n", predicate.clone().unwrap_or_default()));
            }
            other => format!("{other:?}"),
        };
        self.note(format!("op {} {detail}", op.name()));
        Ok(String::new())
    }
    async fn run_xctest(
        &self,
        _ctx: &DeviceCtx,
        _bundle: &std::path::Path,
        _filename: &str,
        opts: &XctestOptions,
        out_dir: &std::path::Path,
        _timeout: Duration,
        _cancel: &CancellationToken,
    ) -> Result<XctestOutcome> {
        std::fs::create_dir_all(out_dir.join("attachments"))?;
        std::fs::write(out_dir.join("xcodebuild.log"), "** TEST FAILED **\n")?;
        std::fs::write(out_dir.join("result.xcresult.zip"), b"PK fake")?;
        std::fs::write(out_dir.join("attachments/Login.png"), PNG)?;
        std::fs::write(
            out_dir.join("xcresult-tests.json"),
            json!({"testNodes": [{"nodeType": "Test Plan", "name": "Plan", "children": [
                {"nodeType": "UI test bundle", "name": "DemoUITests", "children": [
                    {"nodeType": "Test Suite", "name": "LoginTests", "children": [
                        {"nodeType": "Test Case", "name": "testLogin()", "result": "Passed", "durationInSeconds": 2.0},
                        {"nodeType": "Test Case", "name": "testLogout()", "result": "Failed", "durationInSeconds": 1.0,
                         "children": [{"nodeType": "Failure Message", "name": "XCTAssertTrue failed"}]}
                    ]}
                ]}
            ]}]})
            .to_string(),
        )?;
        self.note(format!("xctest only={:?}", opts.only_testing));
        Ok(XctestOutcome {
            exit_code: Some(65),
            timed_out: false,
            cancelled: false,
            output_tail: "** TEST FAILED **".into(),
        })
    }
    async fn collect_crashes(&self, _ctx: &DeviceCtx, run_dir: &std::path::Path, executable: &str) -> Result<usize> {
        std::fs::create_dir_all(run_dir.join("crashes"))?;
        std::fs::write(run_dir.join(format!("crashes/{executable}-2026-11-16.ips")), "{\"bug_type\":\"309\"}")?;
        Ok(1)
    }
    /// Like `seiji-sim`: ready, a format, then frames at `fps` (every 10th a keyframe), a keyframe at
    /// once when asked. Touches, keys and buttons go to the log, e.g. `touch down 0.50,0.25`.
    async fn screen(
        &self,
        _ctx: &DeviceCtx,
        opts: StreamOpts,
        mut commands: tokio::sync::mpsc::Receiver<ScreenCommand>,
        events: tokio::sync::mpsc::Sender<ScreenEvent>,
        cancel: &CancellationToken,
    ) -> Result<()> {
        self.streams_started.fetch_add(1, Ordering::SeqCst);
        let format =
            ScreenFormat { codec: "avc1.64001f".into(), width: 402, height: 874, description: "AWQAH/8=".into() };
        let frame = |n: u64, key: bool| {
            let captured_us =
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_micros()
                    as i64;
            ScreenEvent::Frame(VideoFrame { key, captured_us, data: bytes::Bytes::from(n.to_be_bytes().to_vec()) })
        };
        events.send(ScreenEvent::Ready).await?;
        events.send(ScreenEvent::Format(format)).await?;
        events.send(frame(0, true)).await?;
        let mut tick = tokio::time::interval(Duration::from_millis(1000 / u64::from(opts.fps.max(1))));
        tick.tick().await;
        let mut n: u64 = 1;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                _ = tick.tick(), if !self.still.load(Ordering::SeqCst) => {
                    events.send(frame(n, n % 10 == 0)).await?;
                    n += 1;
                }
                cmd = commands.recv() => match cmd {
                    None => return Ok(()),
                    Some(ScreenCommand::Keyframe) => {
                        events.send(frame(n, true)).await?;
                        n += 1;
                    }
                    Some(ScreenCommand::Touch { phase, x, y, edge }) => self.note(format!(
                        "touch {} {x:.2},{y:.2}{}",
                        serde_json::to_value(phase)?.as_str().unwrap_or("?"),
                        edge.map(|e| format!(" edge {}", serde_json::to_value(e).unwrap().as_str().unwrap())).unwrap_or_default()
                    )),
                    Some(ScreenCommand::Key { usage, down }) => {
                        self.note(format!("hid key {usage} {}", if down { "down" } else { "up" }))
                    }
                    Some(ScreenCommand::Button { button, down }) => self.note(format!(
                        "hid button {} {}",
                        serde_json::to_value(button)?.as_str().unwrap_or("?"),
                        if down { "down" } else { "up" }
                    )),
                    Some(ScreenCommand::Encode(o)) => self.note(format!("encode {} {} {}", o.fps, o.scale, o.bitrate)),
                    Some(ScreenCommand::Sync(done)) => {
                        let _ = done.send(());
                    }
                },
            }
        }
    }
}

/// Prints its arguments; fails flows containing FAILME; answers `hierarchy`.
pub const MAESTRO_STUB: &str = r#"#!/bin/sh
echo "ARGS: $*"
case " $* " in *" hierarchy "*) echo '{"elements":[{"txt":"Login"}]}'; exit 0 ;; esac
for last; do :; done
if grep -q FAILME "$last"; then echo "Assertion is false: Login visible"; exit 1; fi
echo "Flow passed"
"#;

pub struct Env {
    _dir: tempfile::TempDir,
    pub pool: Arc<Pool>,
    pub fake: Arc<FakeDevices>,
    pub url: String,
    shutdown: CancellationToken,
}

impl Drop for Env {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

pub const QUOTA: Quota =
    Quota { monthly_minutes: 60, monthly_unlimited: false, max_concurrent: 1, max_lease_minutes: 30, idle_minutes: 10 };

pub async fn setup(ios_capacity: i64) -> Env {
    setup_with(ios_capacity, json!({})).await
}

/// `extra` overrides top-level config keys.
pub async fn setup_with(ios_capacity: i64, extra: Value) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let stub = dir.path().join("maestro");
    std::fs::write(&stub, MAESTRO_STUB).unwrap();
    std::fs::set_permissions(&stub, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let mut config = json!({
        "dataDir": dir.path().join("data"),
        "seats": 3,
        "costPerDeviceHourUsd": 1.5,
        "capacity": { "ios": ios_capacity, "android": 0 },
        "defaultQuota": QUOTA,
        "isolation": { "mode": "shared" },
        "ios": {
            "models": { "iphone-16": { "deviceType": "iPhone-16", "runtime": "iOS-18" } },
            "defaultModel": "iphone-16"
        },
        "maestro": { "bin": stub },
        "maxUploadBytes": 16,
        "maxBuildsPerUser": 2
    });
    for (k, v) in extra.as_object().unwrap() {
        config[k] = v.clone();
    }
    let config = Arc::new(Config::parse(&config.to_string(), dir.path()).unwrap());
    let store = Arc::new(Store::open(&config.db_path()).unwrap());
    let runner = Arc::new(Runner::new(&config).unwrap());
    let fake = Arc::new(FakeDevices::default());
    let pool = Arc::new(Pool::new(config, store, runner, fake.clone()));
    let shutdown = CancellationToken::new();
    let app = seiji::http::router(pool.clone(), shutdown.child_token());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let stop = shutdown.clone();
    tokio::spawn(async move {
        axum::serve(listener, app).with_graceful_shutdown(async move { stop.cancelled().await }).await.unwrap();
    });
    Env { _dir: dir, pool, fake, url, shutdown }
}

pub fn add_user(env: &Env, name: &str, quota: Quota) -> String {
    env.pool.store.add_user(name, None, quota, env.pool.config.seats).unwrap().1
}

pub async fn client(env: &Env, token: &str) -> RunningService<RoleClient, ClientConfig> {
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("{}/mcp", env.url)).auth_header(token),
    );
    ClientConfig::new(ClientCapabilities::default(), Implementation::new("test", "0")).serve(transport).await.unwrap()
}

pub async fn call(c: &RunningService<RoleClient, ClientConfig>, tool: &str, args: Value) -> CallToolResult {
    let params = CallToolRequestParams::new(tool.to_string()).with_arguments(args.as_object().unwrap().clone());
    c.call_tool(params).await.unwrap()
}

pub fn text(r: &CallToolResult) -> String {
    r.content.iter().filter_map(|c| c.as_text()).map(|t| t.text.as_str()).collect::<Vec<_>>().join("\n")
}

pub fn ok_json(r: &CallToolResult) -> Value {
    assert_ne!(r.is_error, Some(true), "tool failed: {}", text(r));
    serde_json::from_str(&text(r)).unwrap()
}

/// The JSON text of a tool result whether or not it is an error.
pub fn any_json(r: &CallToolResult) -> Value {
    serde_json::from_str(&text(r)).unwrap_or_else(|_| panic!("not JSON: {}", text(r)))
}

pub fn is_err(r: &CallToolResult) -> bool {
    r.is_error == Some(true)
}

pub fn has_image(r: &CallToolResult) -> bool {
    r.content.iter().any(|c| matches!(c, rmcp::model::ContentBlock::Image(_)))
}

pub async fn upload(env: &Env, token: &str, name: &str, body: &'static str) -> Value {
    reqwest::Client::new()
        .post(format!("{}/v1/apps", env.url))
        .bearer_auth(token)
        .header("X-Filename", name)
        .body(body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}
