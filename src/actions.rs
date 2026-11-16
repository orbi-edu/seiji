//! `run_actions`: a validated list of UI steps, executed one by one against an iOS simulator,
//! each recorded with its timing, outcome and a screenshot.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::devices::{DeviceCtx, DeviceOp, Devices, UiInput};

/// AXe refuses selector waits above 30 s.
const MAX_TAP_WAIT: f64 = 30.0;
const MAX_WAIT: f64 = 120.0;
const MAX_TEXT: usize = 10_000;
const POLL: Duration = Duration::from_millis(500);
const ASSERT_SETTLE: Duration = Duration::from_secs(2);

/// An element on screen, matched against the accessibility tree.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Selector {
    /// Accessibility identifier (exact).
    pub id: Option<String>,
    /// Accessibility label (exact).
    pub label: Option<String>,
    /// Accessibility value (exact).
    pub value: Option<String>,
    /// Case-insensitive substring of the label or the value. Not accepted by `tap`.
    pub contains: Option<String>,
    /// Narrow to one element type, e.g. `Button`, `TextField`, `Switch`.
    pub element_type: Option<String>,
}

impl Selector {
    fn criteria(&self) -> usize {
        [&self.id, &self.label, &self.value, &self.contains].iter().filter(|c| c.is_some()).count()
    }

    fn validate(&self) -> Result<()> {
        if self.criteria() == 0 {
            bail!("a selector needs one of id, label, value or contains");
        }
        Ok(())
    }

    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        for (k, v) in [
            ("id", &self.id),
            ("label", &self.label),
            ("value", &self.value),
            ("contains", &self.contains),
            ("type", &self.element_type),
        ] {
            if let Some(v) = v {
                parts.push(format!("{k}={v:?}"));
            }
        }
        parts.join(" ")
    }

    /// True when `node` itself (not its children) matches every given criterion.
    pub fn matches(&self, node: &Value) -> bool {
        let s = |k: &str| node.get(k).and_then(scalar);
        let eq = |want: &Option<String>, got: Option<String>| want.as_ref().is_none_or(|w| got.as_deref() == Some(w));
        if !eq(&self.label, s("AXLabel")) || !eq(&self.value, s("AXValue")) {
            return false;
        }
        if let Some(id) = &self.id
            && s("AXUniqueId").as_deref() != Some(id)
            && s("AXIdentifier").as_deref() != Some(id)
        {
            return false;
        }
        if let Some(c) = &self.contains {
            let c = c.to_lowercase();
            let hit =
                [s("AXLabel"), s("AXValue"), s("title")].into_iter().flatten().any(|t| t.to_lowercase().contains(&c));
            if !hit {
                return false;
            }
        }
        if let Some(t) = &self.element_type {
            let hit = [s("type"), s("role"), s("role_description")]
                .into_iter()
                .flatten()
                .any(|v| v.eq_ignore_ascii_case(t) || v.eq_ignore_ascii_case(&format!("AX{t}")));
            if !hit {
                return false;
            }
        }
        true
    }
}

fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Every node in an AXe tree (an array of roots, each with `children`), depth first.
pub fn nodes(tree: &Value) -> Vec<&Value> {
    fn walk<'a>(v: &'a Value, out: &mut Vec<&'a Value>) {
        match v {
            Value::Array(items) => items.iter().for_each(|i| walk(i, out)),
            Value::Object(o) => {
                out.push(v);
                if let Some(c) = o.get("children") {
                    walk(c, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(tree, &mut out);
    out
}

pub fn find<'a>(tree: &'a Value, selector: &Selector) -> Vec<&'a Value> {
    nodes(tree).into_iter().filter(|n| selector.matches(n)).collect()
}

/// One line per node that carries something to target: indent, type, label, id, value, frame.
/// Far fewer tokens than the raw JSON and enough to write selectors.
pub fn render_tree(tree: &Value) -> String {
    fn walk(v: &Value, depth: usize, out: &mut String) {
        match v {
            Value::Array(items) => items.iter().for_each(|i| walk(i, depth, out)),
            Value::Object(o) => {
                let s = |k: &str| o.get(k).and_then(scalar);
                let kind = s("type").or_else(|| s("role")).unwrap_or_else(|| "?".into());
                let mut line = format!("{}{kind}", "  ".repeat(depth));
                let mut useful = false;
                for (key, name) in
                    [("AXLabel", "label"), ("AXUniqueId", "id"), ("AXIdentifier", "id"), ("AXValue", "value")]
                {
                    if let Some(v) = s(key) {
                        line.push_str(&format!(" {name}={v:?}"));
                        useful = true;
                    }
                }
                if let Some(f) = o.get("frame") {
                    let n = |k: &str| f.get(k).and_then(Value::as_f64).unwrap_or(0.0);
                    line.push_str(&format!(" @({:.0},{:.0} {:.0}x{:.0})", n("x"), n("y"), n("width"), n("height")));
                }
                if o.get("enabled").and_then(Value::as_bool) == Some(false) {
                    line.push_str(" disabled");
                }
                if useful || depth == 0 {
                    out.push_str(&line);
                    out.push('\n');
                }
                if let Some(c) = o.get("children") {
                    walk(c, depth + usize::from(useful || depth == 0), out);
                }
            }
            _ => {}
        }
    }
    let mut out = String::new();
    walk(tree, 0, &mut out);
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum HwButton {
    Home,
    Lock,
    SideButton,
    Siri,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TapArgs {
    /// Accessibility identifier (exact).
    pub id: Option<String>,
    /// Accessibility label (exact).
    pub label: Option<String>,
    /// Accessibility value (exact).
    pub value: Option<String>,
    pub element_type: Option<String>,
    /// Point in logical points; give `x` and `y` instead of a selector.
    pub x: Option<f64>,
    pub y: Option<f64>,
    /// 2 for a double tap.
    pub count: Option<u8>,
    /// Hold for a long press (0 < s <= 10).
    pub hold_seconds: Option<f64>,
    /// Wait this long for the element to appear (default 5, max 30).
    pub timeout_seconds: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TypeArgs {
    /// Text typed into the focused field, on the simulator's keyboard layout.
    pub text: String,
    /// Press Return afterwards.
    #[serde(default)]
    pub submit: bool,
    /// Keep the text out of the evidence (passwords, tokens): recorded as `<redacted>`.
    #[serde(default)]
    pub secret: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SwipeArgs {
    /// `[x, y]` in logical points.
    pub from: [f64; 2],
    pub to: [f64; 2],
    /// Seconds (default 1).
    pub duration_seconds: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScrollArgs {
    /// Content direction, as AXe's `scroll-up|down|left|right` presets.
    pub direction: Direction,
    /// Keep scrolling until this element is on screen (needs `id` or `label`).
    pub until: Option<Selector>,
    /// At most this many swipes while looking for `until` (default 10, max 50).
    pub max_swipes: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WaitArgs {
    pub id: Option<String>,
    pub label: Option<String>,
    pub value: Option<String>,
    pub contains: Option<String>,
    pub element_type: Option<String>,
    /// Default 10, max 120.
    pub timeout_seconds: Option<f64>,
    /// Wait for the element to disappear instead.
    #[serde(default)]
    pub gone: bool,
}

impl WaitArgs {
    fn selector(&self) -> Selector {
        Selector {
            id: self.id.clone(),
            label: self.label.clone(),
            value: self.value.clone(),
            contains: self.contains.clone(),
            element_type: self.element_type.clone(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NameArgs {
    /// `[A-Za-z0-9_-]`, at most 64 characters; becomes the file name.
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SleepArgs {
    /// At most 60.
    pub seconds: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UrlArgs {
    /// Deep link or web URL, opened with `simctl openurl`.
    pub url: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LaunchArgs {
    /// Defaults to the app installed last on this lease.
    pub bundle_id: Option<String>,
    /// Launch arguments passed to the app.
    #[serde(default)]
    pub arguments: Vec<String>,
    /// Kill a running instance first (default true).
    pub restart: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AppArgs {
    /// Defaults to the app installed last on this lease.
    pub bundle_id: Option<String>,
}

/// One step. Written as an object with one key, e.g. `{"tap": {"label": "Sign In"}}`.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    /// Tap an element (id, label or value) or a point; long press with `hold_seconds`.
    Tap(TapArgs),
    /// Type into the focused field.
    Type(TypeArgs),
    Swipe(SwipeArgs),
    Scroll(ScrollArgs),
    /// Press a hardware button: `home`, `lock`, `side-button`, `siri`.
    Button(HwButton),
    /// Press a key by HID usage code (40 Return, 41 Escape, 42 Backspace, 43 Tab).
    Key(u8),
    /// Wait until an element appears (or disappears with `gone: true`).
    WaitFor(WaitArgs),
    /// Fail unless an element is on screen (2 s to settle).
    AssertVisible(Selector),
    /// Fail if an element is still on screen after 2 s.
    AssertNotVisible(Selector),
    /// Save a named screenshot in the evidence.
    Screenshot(NameArgs),
    /// Save the accessibility tree in the evidence.
    CaptureTree(NameArgs),
    Sleep(SleepArgs),
    OpenUrl(UrlArgs),
    LaunchApp(LaunchArgs),
    TerminateApp(AppArgs),
}

pub fn valid_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        bail!("name {name:?} must be 1-64 characters of [A-Za-z0-9_-]");
    }
    Ok(())
}

pub fn valid_bundle_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 255 || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-') {
        bail!("bundle id {id:?} must be [A-Za-z0-9.-]+");
    }
    Ok(())
}

pub fn valid_url(url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url).with_context(|| format!("invalid url {url:?}"))?;
    if parsed.scheme().is_empty() {
        bail!("url {url:?} has no scheme");
    }
    Ok(())
}

fn positive_at_most(v: Option<f64>, max: f64, what: &str) -> Result<()> {
    if let Some(v) = v
        && !(v > 0.0 && v <= max)
    {
        bail!("{what} must be > 0 and <= {max}");
    }
    Ok(())
}

impl Action {
    pub fn validate(&self) -> Result<()> {
        match self {
            Action::Tap(t) => {
                let selectors = [&t.id, &t.label, &t.value].iter().filter(|s| s.is_some()).count();
                let point = t.x.is_some() || t.y.is_some();
                match (selectors, point) {
                    (1, false) => {}
                    (0, true) if t.x.is_some() && t.y.is_some() => {
                        if t.element_type.is_some() || t.timeout_seconds.is_some() {
                            bail!("tap: element_type and timeout_seconds need a selector, not a point");
                        }
                    }
                    _ => bail!("tap needs exactly one of id, label or value, or both x and y"),
                }
                if t.count.is_some_and(|c| c != 1 && c != 2) {
                    bail!("tap: count must be 1 or 2");
                }
                if t.count == Some(2) && t.hold_seconds.is_some() {
                    bail!("tap: a double tap cannot also hold");
                }
                positive_at_most(t.hold_seconds, 10.0, "tap: hold_seconds")?;
                positive_at_most(t.timeout_seconds, MAX_TAP_WAIT, "tap: timeout_seconds")?;
            }
            Action::Type(t) => {
                if t.text.is_empty() || t.text.len() > MAX_TEXT {
                    bail!("type: text must be 1-{MAX_TEXT} bytes");
                }
            }
            Action::Swipe(s) => positive_at_most(s.duration_seconds, 10.0, "swipe: duration_seconds")?,
            Action::Scroll(s) => {
                if let Some(u) = &s.until
                    && (u.id.is_none() == u.label.is_none()
                        || u.value.is_some()
                        || u.contains.is_some()
                        || u.element_type.is_some())
                {
                    bail!("scroll: until needs exactly one of id or label, and nothing else");
                }
                if s.max_swipes.is_some() && s.until.is_none() {
                    bail!("scroll: max_swipes needs until");
                }
                if s.max_swipes.is_some_and(|m| m == 0 || m > 50) {
                    bail!("scroll: max_swipes must be 1-50");
                }
            }
            Action::Button(_) | Action::Key(_) => {}
            Action::WaitFor(w) => {
                w.selector().validate()?;
                positive_at_most(w.timeout_seconds, MAX_WAIT, "wait_for: timeout_seconds")?;
            }
            Action::AssertVisible(s) | Action::AssertNotVisible(s) => s.validate()?,
            Action::Screenshot(n) | Action::CaptureTree(n) => valid_name(&n.name)?,
            Action::Sleep(s) => positive_at_most(Some(s.seconds), 60.0, "sleep: seconds")?,
            Action::OpenUrl(u) => valid_url(&u.url)?,
            Action::LaunchApp(l) => {
                if let Some(b) = &l.bundle_id {
                    valid_bundle_id(b)?;
                }
                if l.arguments.iter().map(String::len).sum::<usize>() > 8192 {
                    bail!("launch_app: arguments are too long");
                }
            }
            Action::TerminateApp(a) => {
                if let Some(b) = &a.bundle_id {
                    valid_bundle_id(b)?;
                }
            }
        }
        Ok(())
    }

    /// The action as it is stored in evidence: secret text replaced.
    pub fn redacted(&self) -> Action {
        match self {
            Action::Type(t) if t.secret => Action::Type(TypeArgs { text: "<redacted>".into(), ..t.clone() }),
            other => other.clone(),
        }
    }

    /// Short name used in file names and summaries.
    pub fn kind(&self) -> &'static str {
        match self {
            Action::Tap(_) => "tap",
            Action::Type(_) => "type",
            Action::Swipe(_) => "swipe",
            Action::Scroll(_) => "scroll",
            Action::Button(_) => "button",
            Action::Key(_) => "key",
            Action::WaitFor(_) => "wait_for",
            Action::AssertVisible(_) => "assert_visible",
            Action::AssertNotVisible(_) => "assert_not_visible",
            Action::Screenshot(_) => "screenshot",
            Action::CaptureTree(_) => "capture_tree",
            Action::Sleep(_) => "sleep",
            Action::OpenUrl(_) => "open_url",
            Action::LaunchApp(_) => "launch_app",
            Action::TerminateApp(_) => "terminate_app",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Passed,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Serialize)]
pub struct StepRecord {
    pub index: usize,
    pub action: Action,
    pub status: StepStatus,
    /// Milliseconds from the start of the run.
    pub started_ms: u64,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Paths relative to the run, e.g. `steps/003-tap.png`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
}

pub struct Plan<'a> {
    pub actions: &'a [Action],
    pub screenshot_each_step: bool,
    pub continue_on_error: bool,
    pub deadline: Instant,
    /// Bundle id used when `launch_app`/`terminate_app` omit theirs.
    pub default_app: Option<String>,
}

/// Directories of one run: `tenant` is written by device tools (as the tenant), `daemon` by Seiji.
/// Both use the same relative layout; the tenant's files are copied into `daemon` at the end.
pub struct RunDirs<'a> {
    pub tenant: &'a Path,
    pub daemon: &'a Path,
    /// Daemon-owned scratch for stdin payloads.
    pub scratch: &'a Path,
}

pub async fn execute(
    devices: &dyn Devices,
    ctx: &DeviceCtx,
    dirs: &RunDirs<'_>,
    plan: &Plan<'_>,
    cancel: &CancellationToken,
) -> Vec<StepRecord> {
    let start = Instant::now();
    let mut records = Vec::with_capacity(plan.actions.len());
    let mut stop: Option<&'static str> = None;
    for (index, action) in plan.actions.iter().enumerate() {
        let started_ms = start.elapsed().as_millis() as u64;
        if stop.is_none() {
            if cancel.is_cancelled() {
                stop = Some("the lease was released");
            } else if Instant::now() >= plan.deadline {
                stop = Some("the run reached evidence.maxRunSeconds");
            }
        }
        if let Some(why) = stop {
            records.push(StepRecord {
                index,
                action: action.redacted(),
                status: StepStatus::Skipped,
                started_ms,
                duration_ms: 0,
                error: Some(why.to_string()),
                files: Vec::new(),
            });
            continue;
        }
        let prefix = format!("steps/{index:03}-{}", action.kind());
        let mut files = Vec::new();
        let remaining = plan.deadline.saturating_duration_since(Instant::now());
        let outcome = tokio::select! {
            r = step(devices, ctx, dirs, plan, action, &prefix, &mut files) => r,
            _ = tokio::time::sleep(remaining) => Err(anyhow::anyhow!("the run reached evidence.maxRunSeconds")),
            _ = cancel.cancelled() => Err(anyhow::anyhow!("the lease was released")),
        };
        let failed = outcome.is_err();
        if cancel.is_cancelled() {
            stop = Some("the lease was released");
        } else {
            // A `screenshot` step already saved its own picture.
            if (plan.screenshot_each_step && !matches!(action, Action::Screenshot(_))) || failed {
                let rel = format!("{prefix}{}.png", if failed { "-failure" } else { "" });
                if !files.contains(&rel) && devices.save_screenshot(ctx, &dirs.tenant.join(&rel)).await.is_ok() {
                    files.push(rel);
                }
            }
            if failed && let Ok(tree) = devices.describe_ui(ctx).await {
                let rel = format!("{prefix}-tree.txt");
                if std::fs::write(dirs.daemon.join(&rel), render_tree(&tree)).is_ok() {
                    files.push(rel);
                }
            }
        }
        records.push(StepRecord {
            index,
            action: action.redacted(),
            status: if failed { StepStatus::Failed } else { StepStatus::Passed },
            started_ms,
            duration_ms: (start.elapsed().as_millis() as u64).saturating_sub(started_ms),
            error: outcome.err().map(|e| format!("{e:#}")),
            files,
        });
        if failed && !plan.continue_on_error && stop.is_none() {
            stop = Some("an earlier step failed");
        }
    }
    records
}

async fn wait_until(
    devices: &dyn Devices,
    ctx: &DeviceCtx,
    selector: &Selector,
    gone: bool,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let tree = devices.describe_ui(ctx).await?;
        let present = !find(&tree, selector).is_empty();
        if present != gone {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let state = if gone { "still on screen" } else { "not on screen" };
            bail!("{} is {state} after {:.1}s", selector.describe(), timeout.as_secs_f64());
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn step(
    devices: &dyn Devices,
    ctx: &DeviceCtx,
    dirs: &RunDirs<'_>,
    plan: &Plan<'_>,
    action: &Action,
    prefix: &str,
    files: &mut Vec<String>,
) -> Result<()> {
    let app = |given: &Option<String>| -> Result<String> {
        given
            .clone()
            .or_else(|| plan.default_app.clone())
            .context("no bundle_id given and no app installed on this lease")
    };
    match action {
        Action::Tap(t) => {
            devices.ui(ctx, &UiInput::Tap(t.clone())).await?;
        }
        Action::Type(t) => {
            let file = dirs.scratch.join(format!("{}.txt", uuid::Uuid::new_v4().simple()));
            std::fs::write(&file, &t.text)?;
            let typed = devices.ui(ctx, &UiInput::Type { text_file: file.clone() }).await;
            let _ = std::fs::remove_file(&file);
            typed?;
            if t.submit {
                devices.ui(ctx, &UiInput::Key(40)).await?;
            }
        }
        Action::Swipe(s) => devices.ui(ctx, &UiInput::Swipe(s.clone())).await?,
        Action::Scroll(s) => devices.ui(ctx, &UiInput::Scroll(s.clone())).await?,
        Action::Button(b) => devices.ui(ctx, &UiInput::Button(*b)).await?,
        Action::Key(k) => devices.ui(ctx, &UiInput::Key(*k)).await?,
        Action::WaitFor(w) => {
            let timeout = Duration::from_secs_f64(w.timeout_seconds.unwrap_or(10.0));
            wait_until(devices, ctx, &w.selector(), w.gone, timeout).await?;
        }
        Action::AssertVisible(s) => wait_until(devices, ctx, s, false, ASSERT_SETTLE).await?,
        Action::AssertNotVisible(s) => wait_until(devices, ctx, s, true, ASSERT_SETTLE).await?,
        Action::Screenshot(n) => {
            let rel = format!("{prefix}-{}.png", n.name);
            devices.save_screenshot(ctx, &dirs.tenant.join(&rel)).await?;
            files.push(rel);
        }
        Action::CaptureTree(n) => {
            let tree = devices.describe_ui(ctx).await?;
            let rel = format!("{prefix}-{}.json", n.name);
            std::fs::write(dirs.daemon.join(&rel), serde_json::to_vec_pretty(&tree)?)?;
            files.push(rel);
        }
        Action::Sleep(s) => tokio::time::sleep(Duration::from_secs_f64(s.seconds)).await,
        Action::OpenUrl(u) => {
            devices.device_op(ctx, &DeviceOp::OpenUrl(u.url.clone())).await?;
        }
        Action::LaunchApp(l) => {
            let op = DeviceOp::Launch {
                bundle_id: app(&l.bundle_id)?,
                arguments: l.arguments.clone(),
                restart: l.restart.unwrap_or(true),
            };
            devices.device_op(ctx, &op).await?;
        }
        Action::TerminateApp(a) => {
            devices.device_op(ctx, &DeviceOp::Terminate { bundle_id: app(&a.bundle_id)? }).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tree() -> Value {
        json!([{
            "type": "Application", "AXLabel": "Demo", "frame": {"x":0,"y":0,"width":402,"height":874},
            "children": [
                {"type": "Group", "children": [
                    {"type": "Button", "AXLabel": "Sign In", "AXUniqueId": "signin", "frame": {"x":20,"y":700,"width":362,"height":50}},
                    {"type": "TextField", "AXLabel": "Email", "AXValue": "ana@example.com", "enabled": false}
                ]},
                {"type": "StaticText", "AXLabel": "Welcome back, Ana!"}
            ]
        }])
    }

    #[test]
    fn selectors_match_exact_fields_and_types() {
        let t = tree();
        let by = |s: Selector| find(&t, &s).len();
        assert_eq!(by(Selector { label: Some("Sign In".into()), ..Default::default() }), 1);
        assert_eq!(by(Selector { label: Some("Sign".into()), ..Default::default() }), 0);
        assert_eq!(
            by(Selector { id: Some("signin".into()), element_type: Some("button".into()), ..Default::default() }),
            1
        );
        assert_eq!(
            by(Selector { id: Some("signin".into()), element_type: Some("TextField".into()), ..Default::default() }),
            0
        );
        assert_eq!(by(Selector { value: Some("ana@example.com".into()), ..Default::default() }), 1);
        assert_eq!(by(Selector { contains: Some("WELCOME".into()), ..Default::default() }), 1);
    }

    #[test]
    fn rendered_tree_keeps_targets_and_drops_empty_groups() {
        let text = render_tree(&tree());
        assert!(text.contains("Button label=\"Sign In\" id=\"signin\" @(20,700 362x50)"), "{text}");
        assert!(text.contains("TextField label=\"Email\" value=\"ana@example.com\" disabled"), "{text}");
        assert!(!text.contains("Group"), "{text}");
    }

    #[test]
    fn actions_parse_from_one_key_objects_and_validate() {
        let actions: Vec<Action> = serde_json::from_value(json!([
            {"tap": {"label": "Sign In"}},
            {"type": {"text": "ana@example.com", "submit": true}},
            {"button": "home"},
            {"wait_for": {"contains": "welcome", "timeout_seconds": 5}},
            {"screenshot": {"name": "home"}}
        ]))
        .unwrap();
        assert!(actions.iter().all(|a| a.validate().is_ok()));

        let bad = |v: Value| serde_json::from_value::<Action>(v).map(|a| a.validate().is_err()).unwrap_or(true);
        assert!(bad(json!({"tap": {"label": "A", "x": 1, "y": 2}})));
        assert!(bad(json!({"tap": {"x": 1}})));
        assert!(bad(json!({"tap": {"label": "A", "timeout_seconds": 31}})));
        assert!(bad(json!({"screenshot": {"name": "../etc"}})));
        assert!(bad(json!({"wait_for": {"timeout_seconds": 3}})));
        assert!(bad(json!({"open_url": {"url": "not a url"}})));
        assert!(bad(json!({"tap": {"label": "A", "surprise": 1}})));
    }
}
