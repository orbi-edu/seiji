//! Recorded runs (`run_actions`, `run_flow`, `run_xctest`) and native device operations.
//!
//! A run happens under the lease's op lock. Around the engine it starts a screen recording and a
//! log stream, collects crash reports afterwards, copies the tenant's files into the daemon's
//! evidence directory, and writes a manifest with a SHA-256 for every file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::actions::{self, Action, Plan, RunDirs, StepStatus, valid_bundle_id, valid_name};
use crate::config::Platform;
use crate::devices::{Capture, DeviceCtx, DeviceOp, XctestOptions};
use crate::evidence::{self, DeviceInfo, Manifest};
use crate::exec::CmdSpec;
use crate::ios;
use crate::pool::{LeaseRuntime, Pool};
use crate::store::{App, Lease, RunRow, User, now};

const MAX_TAGS: usize = 10;
const MAX_LOG_TEXT: usize = 200_000;

/// What every run records besides its engine's own output.
#[derive(Debug, Clone)]
pub struct RunOptions {
    pub name: Option<String>,
    pub tags: Vec<String>,
    pub record_video: bool,
    pub capture_logs: bool,
    /// Unified-log predicate; defaults to the installed app's process and subsystem.
    pub log_predicate: Option<String>,
}

impl RunOptions {
    fn validate(&self) -> Result<()> {
        if let Some(n) = &self.name
            && (n.is_empty() || n.len() > 200)
        {
            bail!("name must be 1-200 characters");
        }
        if self.tags.len() > MAX_TAGS {
            bail!("at most {MAX_TAGS} tags");
        }
        for t in &self.tags {
            valid_name(t).context("tags")?;
        }
        if let Some(p) = &self.log_predicate
            && (p.is_empty() || p.len() > 2000)
        {
            bail!("log_predicate must be 1-2000 characters");
        }
        Ok(())
    }
}

pub struct RunResult {
    pub manifest: Manifest,
    pub manifest_sha256: String,
    /// JPEG of the screen when the run did not pass.
    pub screenshot: Option<Vec<u8>>,
}

enum Engine {
    Actions { actions: Vec<Action>, screenshot_each_step: bool, continue_on_error: bool },
    Flow { yaml_file: PathBuf, env: BTreeMap<String, String> },
    Xctest { app: App, opts: XctestOptions },
}

impl Engine {
    fn kind(&self) -> &'static str {
        match self {
            Engine::Actions { .. } => "actions",
            Engine::Flow { .. } => "flow",
            Engine::Xctest { .. } => "xctest",
        }
    }

    fn tool(&self) -> &'static str {
        match self {
            Engine::Actions { .. } => "run_actions",
            Engine::Flow { .. } => "run_flow",
            Engine::Xctest { .. } => "run_xctest",
        }
    }
}

/// One run in progress: its row, its two directories and the lease it runs on.
struct Run<'a> {
    row: RunRow,
    daemon_dir: PathBuf,
    tenant_dir: PathBuf,
    user_name: &'a str,
    ctx: &'a DeviceCtx,
    lease: &'a Lease,
    cancel: &'a CancellationToken,
    rt: &'a LeaseRuntime,
    budget: u64,
}

/// Drops `null` members so stored actions list only what the agent set.
fn strip_nulls(v: serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(o) => serde_json::Value::Object(
            o.into_iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k, strip_nulls(v))).collect(),
        ),
        serde_json::Value::Array(a) => serde_json::Value::Array(a.into_iter().map(strip_nulls).collect()),
        other => other,
    }
}

/// Where a run's per-step records come from.
enum Steps {
    Inline(serde_json::Value),
    /// Maestro's `commands-*.json` debug files.
    Maestro,
    /// `xcresulttool get test-results tests`.
    Xctest,
}

struct Outcome {
    status: &'static str,
    /// Empty means "derive it from the test results".
    verdict: String,
    steps: Steps,
}

/// A run's summary row for `list_runs`.
#[derive(Debug, Serialize)]
pub struct RunView {
    pub run_id: String,
    pub kind: String,
    pub name: Option<String>,
    pub tags: Vec<String>,
    pub status: String,
    pub verdict: Option<String>,
    pub lease_id: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub bytes: i64,
}

impl From<RunRow> for RunView {
    fn from(r: RunRow) -> Self {
        Self {
            run_id: r.id,
            kind: r.kind,
            name: r.name,
            tags: r.tags,
            status: r.status,
            verdict: r.summary,
            lease_id: r.lease_id,
            started_at: r.started_at,
            ended_at: r.ended_at,
            bytes: r.bytes,
        }
    }
}

fn dir_size(dir: &Path) -> u64 {
    walk_size(dir).unwrap_or(0)
}

fn walk_size(dir: &Path) -> std::io::Result<u64> {
    let mut total = 0;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        total += if meta.is_dir() { walk_size(&entry.path())? } else { meta.len() };
    }
    Ok(total)
}

fn ios_lease(lease: &Lease, what: &str) -> Result<()> {
    if lease.platform != Platform::Ios {
        bail!("{what} supports iOS simulators only for now");
    }
    Ok(())
}

/// Returns a run's reserved evidence budget when the run ends, however it ends.
struct Reservation<'a> {
    pool: &'a Pool,
    user_id: i64,
    bytes: u64,
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        let mut reserved = self.pool.evidence_reserved.lock().expect("reservation mutex");
        if let Some(r) = reserved.get_mut(&self.user_id) {
            *r = r.saturating_sub(self.bytes);
        }
    }
}

impl Pool {
    fn default_app(rt: &LeaseRuntime) -> Option<String> {
        rt.app.lock().expect("app mutex").as_ref().and_then(|a| a.bundle_id.clone())
    }

    fn device_info(&self, lease: &Lease) -> DeviceInfo {
        let model = self.config.ios.as_ref().and_then(|i| i.models.get(&lease.model));
        DeviceInfo {
            platform: lease.platform,
            model: lease.model.clone(),
            udid: lease.device_id.clone(),
            device_type: model.map(|m| m.device_type.clone()),
            runtime: model.map(|m| m.runtime.clone()),
        }
    }

    /// Runs a native operation on the caller's iOS simulator.
    pub async fn device_op(&self, user: &User, lease_id: Option<&str>, op: DeviceOp) -> Result<String> {
        self.with_lease(user, lease_id, |ctx, lease, _, rt| async move {
            ios_lease(&lease, op.name())?;
            let out = self.devices.device_op(&ctx, &op).await?;
            if let DeviceOp::Uninstall { bundle_id } = &op {
                let mut app = rt.app.lock().expect("app mutex");
                if app.as_ref().and_then(|a| a.bundle_id.as_deref()) == Some(bundle_id) {
                    *app = None;
                }
            }
            Ok(out)
        })
        .await
    }

    /// The bundle id to act on: the given one, or the app installed last on the lease.
    pub fn app_or_default(&self, user: &User, lease_id: Option<&str>, given: Option<String>) -> Result<String> {
        if let Some(b) = given {
            valid_bundle_id(&b)?;
            return Ok(b);
        }
        let lease = self.peek_lease(user, lease_id)?;
        let rt = self.runtime_of(&lease.id).context("lease is being released")?;
        Self::default_app(&rt).context("no bundle_id given and no app installed on this lease")
    }

    /// Recent unified-log lines of the simulator; defaults to the installed app's lines.
    pub async fn logs(
        &self,
        user: &User,
        lease_id: Option<&str>,
        last_seconds: u64,
        predicate: Option<String>,
    ) -> Result<String> {
        if !(1..=3600).contains(&last_seconds) {
            bail!("last_seconds must be 1-3600");
        }
        self.with_lease(user, lease_id, |ctx, lease, _, rt| async move {
            ios_lease(&lease, "get_logs")?;
            let predicate = predicate
                .or_else(|| {
                    let app = rt.app.lock().expect("app mutex").clone()?;
                    Some(ios::app_log_predicate(app.bundle_id.as_deref()?, app.executable.as_deref()))
                })
                .context("no app is installed on this lease; pass predicate")?;
            let op = DeviceOp::LogShow { last_seconds, predicate: Some(predicate) };
            let text = self.devices.device_op(&ctx, &op).await?;
            Ok(crate::exec::tail(&text, MAX_LOG_TEXT).to_string())
        })
        .await
    }

    pub async fn run_actions(
        &self,
        user: &User,
        lease_id: Option<&str>,
        options: RunOptions,
        actions: Vec<Action>,
        screenshot_each_step: bool,
        continue_on_error: bool,
    ) -> Result<RunResult> {
        if actions.is_empty() || actions.len() > self.config.evidence.max_steps {
            bail!("run_actions takes 1-{} actions", self.config.evidence.max_steps);
        }
        for (i, a) in actions.iter().enumerate() {
            a.validate().with_context(|| format!("action {} ({})", i + 1, a.kind()))?;
        }
        let stored: Vec<Action> = actions.iter().map(Action::redacted).collect();
        let input = serde_json::json!({ "actions": stored, "screenshot_each_step": screenshot_each_step, "continue_on_error": continue_on_error });
        self.record(
            user,
            lease_id,
            options,
            input,
            Engine::Actions { actions, screenshot_each_step, continue_on_error },
        )
        .await
    }

    pub async fn run_flow(
        &self,
        user: &User,
        lease_id: Option<&str>,
        options: RunOptions,
        yaml: &str,
        env: BTreeMap<String, String>,
    ) -> Result<RunResult> {
        self.config.maestro.as_ref().context("Maestro is not configured on this host (`maestro` in seiji.json)")?;
        for k in env.keys() {
            if k.is_empty() || k.len() > 128 || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                bail!("env key {k:?} must match [A-Za-z0-9_]+");
            }
        }
        let yaml_file = self.config.data_dir.join("run").join(format!("{}.yaml", uuid::Uuid::new_v4().simple()));
        std::fs::write(&yaml_file, yaml)?;
        // Values can be secrets: the manifest keeps only the keys.
        let input = serde_json::json!({ "yaml": yaml, "env_keys": env.keys().collect::<Vec<_>>() });
        let result =
            self.record(user, lease_id, options, input, Engine::Flow { yaml_file: yaml_file.clone(), env }).await;
        let _ = std::fs::remove_file(&yaml_file);
        result
    }

    pub async fn run_xctest(
        &self,
        user: &User,
        lease_id: Option<&str>,
        options: RunOptions,
        app_id: &str,
        opts: XctestOptions,
    ) -> Result<RunResult> {
        if !self.config.xctest.enabled {
            bail!("run_xctest is turned off on this host (xctest.enabled)");
        }
        let app = self.store.app(app_id, user.id)?.with_context(|| format!("no uploaded app {app_id:?}"))?;
        if let Some(name) = &opts.xctestrun
            && (!name.ends_with(".xctestrun")
                || name.contains('/')
                || evidence::clean_relative(name).as_deref() != Some(name))
        {
            bail!("xctestrun must be a file name ending in .xctestrun");
        }
        for t in opts.only_testing.iter().chain(&opts.skip_testing) {
            ios::valid_test_id(t)?;
        }
        if let Some(c) = &opts.test_configuration
            && (c.is_empty() || c.len() > 200 || c.contains('\n'))
        {
            bail!("test_configuration must be 1-200 characters on one line");
        }
        let input = serde_json::json!({
            "app_id": app.id, "filename": app.filename, "xctestrun": opts.xctestrun,
            "only_testing": opts.only_testing, "skip_testing": opts.skip_testing,
            "test_configuration": opts.test_configuration,
        });
        self.record(user, lease_id, options, input, Engine::Xctest { app, opts }).await
    }

    async fn record(
        &self,
        user: &User,
        lease_id: Option<&str>,
        options: RunOptions,
        input: serde_json::Value,
        engine: Engine,
    ) -> Result<RunResult> {
        options.validate()?;
        let user = self.fresh_user(user)?;
        let ev = &self.config.evidence;
        let budget = {
            let mut reserved = self.evidence_reserved.lock().expect("reservation mutex");
            let held = self.store.evidence_bytes(user.id)?.max(0) as u64 + reserved.get(&user.id).copied().unwrap_or(0);
            if held >= ev.max_bytes_per_user {
                bail!(
                    "evidence quota full: {held} of {} bytes kept or reserved by running runs; runs expire {}h after they end",
                    ev.max_bytes_per_user,
                    ev.retention_hours
                );
            }
            let budget = ev.max_run_bytes.min(ev.max_bytes_per_user - held);
            *reserved.entry(user.id).or_default() += budget;
            budget
        };
        let _reservation = Reservation { pool: self, user_id: user.id, bytes: budget };
        let lease = self.peek_lease(&user, lease_id)?;
        if !matches!(engine, Engine::Flow { .. }) {
            ios_lease(&lease, engine.tool())?;
        }
        let user_name = user.name.clone();
        self.with_lease_keep(&user, lease_id, |ctx, lease, cancel, rt| async move {
            self.record_locked(&user_name, &ctx, &lease, &cancel, &rt, options, input, engine, budget).await
        })
        .await
    }

    /// Writes an `error` manifest for a run that did not finish normally, so its partial evidence
    /// stays readable and counts against the quota.
    pub(crate) fn close_run_with_error(&self, run: &RunRow, verdict: &str, warning: &str) -> Result<()> {
        let dir = evidence::run_dir(&self.config, run.user_id, &run.id);
        std::fs::create_dir_all(&dir)?;
        let lease = self.store.lease(&run.lease_id)?;
        let user = self.store.user_by_id(run.user_id)?.map(|u| u.name).unwrap_or_default();
        let device = match &lease {
            Some(l) => self.device_info(l),
            None => DeviceInfo {
                platform: Platform::Ios,
                model: String::new(),
                udid: None,
                device_type: None,
                runtime: None,
            },
        };
        let ended_at = run.ended_at.unwrap_or_else(now);
        let manifest = Manifest {
            schema: evidence::SCHEMA.into(),
            run_id: run.id.clone(),
            kind: run.kind.clone(),
            name: run.name.clone(),
            tags: run.tags.clone(),
            status: "error".into(),
            verdict: verdict.to_string(),
            user,
            lease_id: run.lease_id.clone(),
            device,
            app: None,
            started_at: run.started_at,
            ended_at,
            seiji_version: env!("CARGO_PKG_VERSION").into(),
            input: serde_json::Value::Null,
            steps: serde_json::json!([]),
            files: evidence::hash_tree(&dir)?,
            dropped: Vec::new(),
            warnings: vec![warning.to_string()],
        };
        evidence::write_manifest(&dir, &manifest)?;
        self.store.finish_run(&run.id, "error", ended_at, dir_size(&dir) as i64, verdict)
    }

    #[allow(clippy::too_many_arguments)]
    async fn record_locked(
        &self,
        user_name: &str,
        ctx: &DeviceCtx,
        lease: &Lease,
        cancel: &CancellationToken,
        rt: &LeaseRuntime,
        options: RunOptions,
        input: serde_json::Value,
        engine: Engine,
        budget: u64,
    ) -> Result<RunResult> {
        let row = RunRow {
            id: format!("r_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]),
            user_id: lease.user_id,
            lease_id: lease.id.clone(),
            kind: engine.kind().to_string(),
            name: options.name.clone(),
            tags: options.tags.clone(),
            status: "running".into(),
            started_at: now(),
            ended_at: None,
            bytes: 0,
            summary: None,
        };
        let run = Run {
            daemon_dir: evidence::run_dir(&self.config, lease.user_id, &row.id),
            tenant_dir: ctx.dir().join("runs").join(&row.id),
            row,
            user_name,
            ctx,
            lease,
            cancel,
            rt,
            budget,
        };
        std::fs::create_dir_all(run.daemon_dir.join("steps"))?;
        let prepare = CmdSpec::sh(r#"set -e; mkdir -p "$1/steps"; : > "$1/.started""#, [run.tenant_dir.as_os_str()]);
        self.runner.run(&ctx.account, prepare, Duration::from_secs(30), cancel).await?.check("preparing the run")?;
        self.store.insert_run(&run.row)?;
        match self.record_body(&run, options, input, engine).await {
            Ok(r) => Ok(r),
            Err(e) => {
                let verdict = format!("{e:#}");
                if let Err(close) = self.close_run_with_error(&run.row, &verdict, "recording the run failed") {
                    tracing::warn!(run = %run.row.id, "closing a failed run: {close:#}");
                }
                Err(e.context(format!("run {} failed while recording its evidence", run.row.id)))
            }
        }
    }

    async fn record_body(
        &self,
        run: &Run<'_>,
        options: RunOptions,
        input: serde_json::Value,
        engine: Engine,
    ) -> Result<RunResult> {
        let (ctx, lease, cancel, rt, budget) = (run.ctx, run.lease, run.cancel, run.rt, run.budget);
        let (daemon_dir, tenant_dir) = (&run.daemon_dir, &run.tenant_dir);
        let app = rt.app.lock().expect("app mutex").clone();
        let ios = lease.platform == Platform::Ios;
        let mut warnings = Vec::new();
        let mut captures = Vec::new();
        if options.record_video && ios {
            match self.devices.start_capture(ctx, &Capture::Video { dest: tenant_dir.join("video.mp4") }).await {
                Ok(h) => captures.push(h),
                Err(e) => warnings.push(format!("video: {e:#}")),
            }
        }
        if options.capture_logs && ios {
            let predicate = options.log_predicate.clone().or_else(|| {
                let a = app.as_ref()?;
                Some(ios::app_log_predicate(a.bundle_id.as_deref()?, a.executable.as_deref()))
            });
            match predicate {
                Some(predicate) => {
                    let out = daemon_dir.join("app.log");
                    match self.devices.start_capture(ctx, &Capture::Log { predicate, out }).await {
                        Ok(h) => captures.push(h),
                        Err(e) => warnings.push(format!("logs: {e:#}")),
                    }
                }
                None => warnings.push("logs: no app is installed on this lease; pass log_predicate".into()),
            }
        }

        let scratch = self.config.data_dir.join("run");
        let dirs = RunDirs { tenant: tenant_dir, daemon: daemon_dir, scratch: &scratch };
        let outcome = match self.run_engine(ctx, lease, rt, &dirs, cancel, engine).await {
            Ok(o) => o,
            Err(e) => {
                Outcome { status: "error", verdict: format!("{e:#}"), steps: Steps::Inline(serde_json::json!([])) }
            }
        };
        let outcome = if cancel.is_cancelled() && outcome.status != "passed" {
            let verdict = if outcome.verdict.starts_with("the lease was released") {
                outcome.verdict
            } else {
                format!("the lease was released during the run: {}", outcome.verdict)
            };
            Outcome { status: "cancelled", verdict, ..outcome }
        } else {
            outcome
        };

        // Evidence collection: never cancelled, bounded by its own timeouts.
        for h in &captures {
            self.devices.stop_capture(ctx, h).await;
        }
        if ios
            && let Some(exe) = app.as_ref().and_then(|a| a.executable.as_deref())
            && let Err(e) = self.devices.collect_crashes(ctx, tenant_dir, exe).await
        {
            warnings.push(format!("crash reports: {e:#}"));
        }
        let screenshot = if outcome.status != "passed" && outcome.status != "cancelled" {
            self.devices.screenshot(ctx).await.ok()
        } else {
            None
        };
        let log = daemon_dir.join("app.log");
        if let Ok(meta) = std::fs::metadata(&log)
            && meta.len() > budget / 2
        {
            std::fs::OpenOptions::new().write(true).open(&log)?.set_len(budget / 2)?;
            warnings.push(format!("app.log was cut to {} bytes", budget / 2));
        }
        let room = budget.saturating_sub(dir_size(daemon_dir));
        let report = evidence::copy_out(&self.runner, &ctx.account, tenant_dir, daemon_dir, room).await?;
        warnings.extend(report.warnings);
        let cleanup = CmdSpec::new("/bin/rm").arg("-rf").arg(tenant_dir);
        if let Err(e) = self.runner.run(&ctx.account, cleanup, Duration::from_secs(60), &CancellationToken::new()).await
        {
            warnings.push(format!("removing the tenant's copy: {e:#}"));
        }

        // Engines whose steps come from files read them from the daemon's copy.
        let steps = match outcome.steps {
            Steps::Inline(v) => v,
            Steps::Maestro => serde_json::Value::Array(evidence::maestro_steps(&daemon_dir.join("maestro/debug"))),
            Steps::Xctest => {
                serde_json::Value::Array(evidence::xctest_steps(&daemon_dir.join("xctest/xcresult-tests.json")))
            }
        };
        let verdict = if outcome.verdict.is_empty() { test_verdict(&steps) } else { outcome.verdict };
        let mut manifest = Manifest {
            schema: evidence::SCHEMA.into(),
            run_id: run.row.id.clone(),
            kind: run.row.kind.clone(),
            name: options.name,
            tags: options.tags,
            status: outcome.status.to_string(),
            verdict,
            user: run.user_name.to_string(),
            lease_id: lease.id.clone(),
            device: self.device_info(lease),
            app: app.map(|a| serde_json::to_value(a).unwrap_or_default()),
            started_at: run.row.started_at,
            ended_at: now(),
            seiji_version: env!("CARGO_PKG_VERSION").into(),
            input: strip_nulls(input),
            steps: strip_nulls(steps),
            files: Vec::new(),
            dropped: report.dropped,
            warnings,
        };
        manifest.files = evidence::hash_tree(daemon_dir)?;
        evidence::write_manifest(daemon_dir, &manifest)?;
        let (_, manifest_sha256) = evidence::sha256_file(&daemon_dir.join(evidence::MANIFEST))?;
        let bytes = dir_size(daemon_dir) as i64;
        self.store.finish_run(&run.row.id, &manifest.status, manifest.ended_at, bytes, &manifest.verdict)?;
        tracing::info!(run = %run.row.id, lease = %lease.id, status = %manifest.status, bytes, "run recorded");
        Ok(RunResult { manifest, manifest_sha256, screenshot })
    }

    async fn run_engine(
        &self,
        ctx: &DeviceCtx,
        lease: &Lease,
        rt: &LeaseRuntime,
        dirs: &RunDirs<'_>,
        cancel: &CancellationToken,
        engine: Engine,
    ) -> Result<Outcome> {
        match engine {
            Engine::Actions { actions, screenshot_each_step, continue_on_error } => {
                let plan = Plan {
                    actions: &actions,
                    screenshot_each_step,
                    continue_on_error,
                    deadline: Instant::now() + Duration::from_secs(self.config.evidence.max_run_seconds),
                    default_app: Self::default_app(rt),
                };
                let records = actions::execute(self.devices.as_ref(), ctx, dirs, &plan, cancel).await;
                let failed: Vec<_> = records.iter().filter(|r| r.status == StepStatus::Failed).collect();
                let passed = records.iter().filter(|r| r.status == StepStatus::Passed).count();
                let (status, verdict) = match failed.first() {
                    None if passed == records.len() => {
                        ("passed", format!("{passed} of {} steps passed", records.len()))
                    }
                    None if cancel.is_cancelled() => ("cancelled", format!("{passed} of {} steps ran", records.len())),
                    None => {
                        let why = records.iter().find_map(|r| r.error.clone()).unwrap_or_default();
                        ("failed", format!("{passed} of {} steps ran: {why}", records.len()))
                    }
                    Some(f) => (
                        "failed",
                        format!(
                            "step {} ({}) failed: {}{}",
                            f.index + 1,
                            f.action.kind(),
                            f.error.as_deref().unwrap_or("?"),
                            if failed.len() > 1 { format!(" (+{} more)", failed.len() - 1) } else { String::new() }
                        ),
                    ),
                };
                Ok(Outcome { status, verdict, steps: Steps::Inline(serde_json::to_value(&records)?) })
            }
            Engine::Flow { yaml_file, env } => {
                let flow = self.maestro.run_flow(ctx, lease.driver_port, &yaml_file, &env, dirs.tenant, cancel).await?;
                std::fs::create_dir_all(dirs.daemon.join("maestro"))?;
                std::fs::write(dirs.daemon.join("maestro/output.txt"), flow.output.text())?;
                let last =
                    flow.output.text().lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
                let (status, verdict) = if flow.passed {
                    ("passed", "the flow passed".to_string())
                } else if flow.output.timed_out {
                    ("failed", "the flow timed out (maestro.flowTimeoutSeconds)".to_string())
                } else {
                    (
                        "failed",
                        format!(
                            "the flow failed (exit {}): {last}",
                            flow.output.code.map_or("signal".into(), |c| c.to_string())
                        ),
                    )
                };
                Ok(Outcome { status, verdict, steps: Steps::Maestro })
            }
            Engine::Xctest { app, opts } => {
                let out_dir = dirs.tenant.join("xctest");
                let timeout = Duration::from_secs(self.config.xctest.timeout_seconds);
                let r = self
                    .devices
                    .run_xctest(ctx, Path::new(&app.path), &app.filename, &opts, &out_dir, timeout, cancel)
                    .await?;
                std::fs::create_dir_all(dirs.daemon.join("xctest"))?;
                std::fs::write(dirs.daemon.join("xctest/output-tail.txt"), &r.output_tail)?;
                let (status, verdict) = match r.exit_code {
                    _ if r.cancelled => ("cancelled", "the lease was released".to_string()),
                    _ if r.timed_out => ("failed", "xcodebuild timed out (xctest.timeoutSeconds)".to_string()),
                    Some(0) => ("passed", String::new()),
                    // xcodebuild exits 65 when tests fail.
                    Some(65) => ("failed", String::new()),
                    code => (
                        "error",
                        format!(
                            "xcodebuild exited {}: {}",
                            code.map_or("by signal".into(), |c| c.to_string()),
                            crate::exec::tail(r.output_tail.trim(), 400)
                        ),
                    ),
                };
                Ok(Outcome { status, verdict, steps: Steps::Xctest })
            }
        }
    }

    pub fn list_runs(&self, user: &User, status: Option<&str>, tag: Option<&str>, limit: i64) -> Result<Vec<RunView>> {
        if let Some(s) = status
            && !["running", "passed", "failed", "error", "cancelled"].contains(&s)
        {
            bail!("status must be running, passed, failed, error or cancelled");
        }
        Ok(self.store.runs(user.id, status, tag, limit.clamp(1, 200))?.into_iter().map(RunView::from).collect())
    }

    /// The manifest of one of the caller's runs.
    pub fn run_manifest(&self, user: &User, run_id: &str) -> Result<(Manifest, PathBuf)> {
        let run = self.store.run(run_id, user.id)?.with_context(|| format!("no run {run_id:?} for you"))?;
        if run.status == "running" {
            bail!("run {run_id} is still running");
        }
        let dir = evidence::run_dir(&self.config, user.id, run_id);
        Ok((evidence::read_manifest(&dir)?, dir))
    }

    /// Absolute path of a file that the run's manifest lists.
    pub fn run_file(&self, user: &User, run_id: &str, rel: &str) -> Result<(PathBuf, u64)> {
        let (manifest, dir) = self.run_manifest(user, run_id)?;
        if rel == evidence::MANIFEST {
            let path = dir.join(evidence::MANIFEST);
            let len = std::fs::metadata(&path)?.len();
            return Ok((path, len));
        }
        let entry = evidence::listed(&manifest, rel)?;
        Ok((dir.join(&entry.path), entry.bytes))
    }

    /// Base URL of a run's HTTP files (absolute with `listen.publicUrl`).
    pub fn run_url(&self, run_id: &str) -> String {
        let base = self.config.listen.public_url.as_deref().unwrap_or("").trim_end_matches('/');
        format!("{base}/v1/runs/{run_id}")
    }
}

fn test_verdict(steps: &serde_json::Value) -> String {
    let Some(steps) = steps.as_array() else { return String::new() };
    let count = |s: &str| steps.iter().filter(|t| t.get("status").and_then(|v| v.as_str()) == Some(s)).count();
    format!(
        "{} tests: {} passed, {} failed, {} skipped",
        steps.len(),
        count("passed"),
        count("failed"),
        count("skipped")
    )
}
