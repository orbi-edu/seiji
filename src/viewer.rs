//! Live screen viewer: watch and drive a leased simulator from a browser page (Tern's browser
//! block, or any browser).
//!
//! A user with a token asks for a viewer link (`POST /v1/leases/{id}/viewer` or the
//! `share_screen` tool), or an admin for any user's lease (`POST /v1/admin/leases/{id}/viewer`).
//! The link carries a one-time ticket that lives 60 s. Opening it trades the ticket for a
//! session: a random id in the path and a random secret in an `HttpOnly`, `SameSite=Strict`
//! cookie scoped to that path. Every request checks the session, its user (and the admin who
//! asked for a link to another user's lease) and that the lease is still the user's and ready.
//! One AXe stream per lease fans out to all its viewers and stops when the last one leaves or the
//! lease ends.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::Context;
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::actions::{HwButton, SwipeArgs, TapArgs};
use crate::config::Platform;
use crate::devices::{DeviceCtx, DeviceOp, Frame, StreamOpts, UiInput};
use crate::pool::{LeaseRuntime, Pool};
use crate::store::{Lease, LeaseStatus, User, now};

const TICKET_SECONDS: i64 = 60;
const COOKIE: &str = "seiji_view";
const BOUNDARY: &str = "seijiframe";
/// A watched lease counts as used: the stream touches it this often.
const TOUCH_EVERY: Duration = Duration::from_secs(60);
/// How often the stream logs its stats for the operator.
const STATS_LOG_EVERY: Duration = Duration::from_secs(30);
const MAX_TEXT: usize = 10_000;
/// A still screen's last frame is sent again this often (the page flags a stall after 2 s).
const FRAME_REPEAT: Duration = Duration::from_secs(1);
/// HID usage codes for Paste: Left Command + V.
const CMD: u8 = 227;
const KEY_V: u8 = 25;
/// How long input waits for the device before it answers busy.
const INPUT_WAIT: Duration = Duration::from_secs(1);

const PAGE: &str = include_str!("viewer/index.html");
const SCRIPT: &str = include_str!("viewer/viewer.js");
const STYLE: &str = include_str!("viewer/viewer.css");

struct Ticket {
    /// The lease's owner: the session acts as this user's account.
    user_id: i64,
    /// Who asked for the link: the owner, or an admin.
    requester_id: i64,
    lease_id: String,
    control: bool,
    expires: i64,
}

struct Session {
    secret_sha256: String,
    user_id: i64,
    requester_id: i64,
    lease_id: String,
    control: bool,
    expires: i64,
}

/// Per-stream counters updated by a stats subscriber on the watch channel.
struct StreamStats {
    started_at_ms: i64,
    frame_count: AtomicU64,
    frame_bytes: AtomicU64,
    last_frame_at_ms: AtomicI64,
}

/// A quality preset the viewer page can pick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamPreset {
    Smooth,
    Balanced,
    Sharp,
}

impl StreamPreset {
    fn opts(self, cfg: &crate::config::Viewer) -> StreamOpts {
        match self {
            StreamPreset::Smooth => StreamOpts { fps: 15, scale: 0.3, quality: 50 },
            StreamPreset::Balanced => StreamOpts { fps: cfg.fps, scale: cfg.scale, quality: cfg.quality },
            StreamPreset::Sharp => StreamOpts { fps: 5, scale: 0.8, quality: 85 },
        }
    }
}

struct Hub {
    tx: watch::Sender<Option<Frame>>,
    cancel: CancellationToken,
    /// Cancels only the current AXe stream; a child of `cancel`.  Replaced on each restart.
    stream_cancel: StdMutex<CancellationToken>,
    opts: StdMutex<StreamOpts>,
    preset: StdMutex<StreamPreset>,
    stats: Arc<StreamStats>,
}

/// Tickets, sessions and the one stream per lease. In memory: a restart ends every lease, and
/// with it every viewer.
#[derive(Default)]
pub struct Viewers {
    tickets: StdMutex<HashMap<String, Ticket>>,
    sessions: StdMutex<HashMap<String, Session>>,
    hubs: StdMutex<HashMap<String, Arc<Hub>>>,
}

impl Viewers {
    /// Drops expired tickets and sessions (the reaper calls it).
    pub fn prune(&self, at: i64) {
        self.tickets.lock().expect("tickets mutex").retain(|_, t| t.expires > at);
        self.sessions.lock().expect("sessions mutex").retain(|_, s| s.expires > at);
    }
}

#[derive(Debug, Serialize)]
pub struct ViewerLink {
    /// Open it within `expires_in` seconds; absolute when `listen.publicUrl` is set.
    pub url: String,
    pub expires_in: i64,
    /// Whether the page may send input, or only watch.
    pub control: bool,
    pub lease_id: String,
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::fill(&mut buf[..]);
    hex::encode(buf)
}

fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as i64
}

/// Why a viewer request failed, as the page sees it.
#[derive(Debug)]
pub enum ViewError {
    /// No session, a wrong secret or an expired session: open a new link.
    Unauthorized,
    /// The lease ended or is no longer the user's.
    Gone(String),
    /// A run holds the device.
    Busy,
    /// A view-only session tried to send input.
    ViewOnly,
    /// Too many viewers on one lease.
    Full(usize),
    Bad(String),
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for ViewError {
    fn from(e: anyhow::Error) -> Self {
        ViewError::Internal(e)
    }
}

impl IntoResponse for ViewError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            ViewError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "expired",
                "This viewer link expired. Open the device again from Tern.".into(),
            ),
            ViewError::Gone(why) => (StatusCode::GONE, "gone", why),
            ViewError::Busy => {
                (StatusCode::CONFLICT, "busy", "A run is using the device. Input works again when it ends.".into())
            }
            ViewError::ViewOnly => (StatusCode::FORBIDDEN, "view_only", "This viewer is view-only.".into()),
            ViewError::Full(n) => {
                (StatusCode::TOO_MANY_REQUESTS, "full", format!("This device already has {n} viewers open."))
            }
            ViewError::Bad(why) => (StatusCode::BAD_REQUEST, "bad_request", why),
            ViewError::Internal(e) => (StatusCode::INTERNAL_SERVER_ERROR, "internal", format!("{e:#}")),
        };
        let mut resp =
            (status, Json(serde_json::json!({ "error": { "code": code, "message": message } }))).into_response();
        no_store(resp.headers_mut());
        resp
    }
}

fn no_store(h: &mut HeaderMap) {
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
}

/// What a session resolved to.
struct Viewing {
    user: User,
    lease: Lease,
    control: bool,
}

impl Pool {
    /// A one-time link to the caller's ready iOS lease.
    pub fn create_viewer(&self, user: &User, lease_id: Option<&str>, control: bool) -> anyhow::Result<ViewerLink> {
        let lease = self.peek_lease(user, lease_id)?;
        self.issue_ticket(user.id, lease, control)
    }

    /// A one-time link to any user's ready iOS lease, for an admin. The session acts as the
    /// lease's owner. For another user's lease it also ends when the admin is disabled or
    /// removed; on the admin's own lease it is the owner's session like any other.
    pub fn create_admin_viewer(&self, admin: &User, lease_id: &str, control: bool) -> anyhow::Result<ViewerLink> {
        let admin = self.fresh_user(admin)?;
        if !self.config.admin_users.contains(&admin.name) {
            anyhow::bail!("admin access required");
        }
        let (owner, lease) = self.admin_lease(lease_id)?;
        let link = self.issue_ticket(admin.id, lease, control)?;
        tracing::info!(admin = %admin.name, user = %owner.name, lease = %link.lease_id, control, "admin viewer link");
        Ok(link)
    }

    fn issue_ticket(&self, requester_id: i64, lease: Lease, control: bool) -> anyhow::Result<ViewerLink> {
        if lease.platform != Platform::Ios {
            anyhow::bail!("the screen viewer supports iOS simulators only for now");
        }
        let ticket = random_hex(32);
        self.viewers.tickets.lock().expect("tickets mutex").insert(
            ticket.clone(),
            Ticket {
                user_id: lease.user_id,
                requester_id,
                lease_id: lease.id.clone(),
                control,
                expires: now() + TICKET_SECONDS,
            },
        );
        let base = self.config.listen.public_url.as_deref().unwrap_or("").trim_end_matches('/');
        Ok(ViewerLink {
            url: format!("{base}/view/open/{ticket}"),
            expires_in: TICKET_SECONDS,
            control,
            lease_id: lease.id,
        })
    }

    /// Trades a ticket for a session: `(session id, secret, lifetime in seconds)`.
    fn open_ticket(&self, ticket: &str) -> Result<(String, String, i64), ViewError> {
        let t = self.viewers.tickets.lock().expect("tickets mutex").remove(ticket).ok_or(ViewError::Unauthorized)?;
        if t.expires <= now() {
            return Err(ViewError::Unauthorized);
        }
        let lifetime = self.config.viewer.session_hours * 3600;
        let (sid, secret) = (random_hex(16), random_hex(32));
        self.viewers.sessions.lock().expect("sessions mutex").insert(
            sid.clone(),
            Session {
                secret_sha256: sha256_hex(&secret),
                user_id: t.user_id,
                requester_id: t.requester_id,
                lease_id: t.lease_id,
                control: t.control,
                expires: now() + lifetime,
            },
        );
        Ok((sid, secret, lifetime))
    }

    fn viewing(&self, sid: &str, headers: &HeaderMap) -> Result<Viewing, ViewError> {
        let secret = cookie(headers).ok_or(ViewError::Unauthorized)?;
        let (user_id, requester_id, lease_id, control) = {
            let sessions = self.viewers.sessions.lock().expect("sessions mutex");
            let s = sessions.get(sid).ok_or(ViewError::Unauthorized)?;
            if s.expires <= now() || s.secret_sha256 != sha256_hex(secret) {
                return Err(ViewError::Unauthorized);
            }
            (s.user_id, s.requester_id, s.lease_id.clone(), s.control)
        };
        let user = self.store.user_by_id(user_id)?.filter(|u| !u.disabled).ok_or(ViewError::Unauthorized)?;
        if requester_id != user_id {
            self.store
                .user_by_id(requester_id)?
                .filter(|a| !a.disabled && self.config.admin_users.contains(&a.name))
                .ok_or(ViewError::Unauthorized)?;
        }
        let lease = self
            .store
            .lease(&lease_id)?
            .filter(|l| l.user_id == user.id)
            .ok_or_else(|| ViewError::Gone("The device was released.".into()))?;
        match lease.status {
            LeaseStatus::Ready => Ok(Viewing { user, lease, control }),
            LeaseStatus::Booting => Err(ViewError::Gone("The device is still booting.".into())),
            LeaseStatus::Ended => Err(ViewError::Gone(format!(
                "The device was released ({}).",
                lease.end_reason.as_deref().unwrap_or("ended")
            ))),
        }
    }

    fn viewer_ctx(&self, v: &Viewing) -> Result<(DeviceCtx, Arc<LeaseRuntime>), ViewError> {
        let rt = self.runtime_of(&v.lease.id).ok_or_else(|| ViewError::Gone("The device is being released.".into()))?;
        Ok((self.ctx(&v.lease, self.account(&v.user)?), rt))
    }

    /// The screen size in points: cached per lease, read again when the picture's aspect ratio
    /// changes (a rotation).
    async fn screen_points(
        &self,
        ctx: &DeviceCtx,
        rt: &LeaseRuntime,
        aspect: Option<f64>,
    ) -> anyhow::Result<(f64, f64)> {
        let cached = *rt.screen.lock().expect("screen mutex");
        if let Some((w, h)) = cached
            && aspect.is_none_or(|a| ((w / h) - a).abs() / a < 0.03)
        {
            return Ok((w, h));
        }
        let tree = self.devices.describe_ui(ctx).await?;
        let size = crate::actions::nodes(&tree)
            .into_iter()
            .filter_map(|n| {
                let f = n.get("frame")?;
                Some((f.get("width")?.as_f64()?, f.get("height")?.as_f64()?))
            })
            .find(|(w, h)| *w > 0.0 && *h > 0.0)
            .context("the accessibility tree has no screen size")?;
        *rt.screen.lock().expect("screen mutex") = Some(size);
        Ok(size)
    }
}

fn cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| c.trim().strip_prefix(COOKIE)?.strip_prefix('='))
}

// ---- input ----

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ViewerInput {
    /// Normalized coordinates (0-1 of the picture); `hold` seconds makes a long press.
    Tap {
        x: f64,
        y: f64,
        hold: Option<f64>,
    },
    Swipe {
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        duration: Option<f64>,
    },
    Type {
        text: String,
    },
    /// HID usage code (40 Return, 42 Backspace, 43 Tab, 41 Escape).
    Key {
        code: u8,
    },
    Button {
        name: HwButton,
    },
    /// Put text on the simulator clipboard (`axe clipboard copy`) and press Cmd+V into the
    /// focused field; typed instead when the clipboard cannot be set.
    Paste {
        text: String,
    },
    /// Type the text character by character (like `Type`), for fields that refuse paste.
    PasteAndType {
        text: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputRequest {
    pub input: ViewerInput,
    /// Width / height of the picture the page shows, to notice rotations.
    pub aspect: Option<f64>,
}

fn unit(v: f64) -> Result<f64, ViewError> {
    if (0.0..=1.0).contains(&v) { Ok(v) } else { Err(ViewError::Bad("coordinates must be between 0 and 1".into())) }
}

fn tap(x: f64, y: f64, hold: Option<f64>) -> TapArgs {
    TapArgs {
        id: None,
        label: None,
        value: None,
        element_type: None,
        x: Some(x),
        y: Some(y),
        count: None,
        hold_seconds: hold,
        timeout_seconds: None,
    }
}

impl Pool {
    /// Applies one input. `Some(via)` for a paste: `"pasteboard"`, or `"typed"` when the
    /// device pasteboard could not be reached and the text was typed instead.
    async fn viewer_input(&self, v: &Viewing, req: InputRequest) -> Result<Option<&'static str>, ViewError> {
        if !v.control {
            return Err(ViewError::ViewOnly);
        }
        let (ctx, rt) = self.viewer_ctx(v)?;
        // A run (or another viewer's input) holds the device: wait briefly, then give up rather
        // than interleave with a run's steps.
        let _op = tokio::time::timeout(INPUT_WAIT, rt.op.clone().lock_owned()).await.map_err(|_| ViewError::Busy)?;
        let mut scratch = None;
        let mut via = None;
        // Paste puts the text on the device clipboard (AXe) and presses Cmd+V into the focused
        // field, so any character goes through; everything else goes through AXe input.
        let result = match req.input {
            ViewerInput::Paste { text } => {
                if text.is_empty() || text.len() > MAX_TEXT {
                    return Err(ViewError::Bad(format!("text must be 1-{MAX_TEXT} bytes")));
                }
                let file = self.config.data_dir.join("run").join(format!("{}.txt", uuid::Uuid::new_v4().simple()));
                std::fs::write(&file, &text).context("writing pasteboard text")?;
                scratch = Some(file.clone());
                match self.devices.device_op(&ctx, &DeviceOp::Pasteboard { text_file: file.clone() }).await {
                    Ok(_) => {
                        via = Some("pasted");
                        self.devices.ui(&ctx, &UiInput::KeyCombo { modifiers: vec![CMD], key: KEY_V }).await
                    }
                    // Without a clipboard, type it: US keyboard characters only.
                    Err(e) => {
                        tracing::warn!(lease = %v.lease.id, error = %e, "clipboard unavailable, typing instead");
                        via = Some("typed");
                        self.devices.ui(&ctx, &UiInput::Type { text_file: file }).await
                    }
                }
            }
            other => {
                let input = match other {
                    ViewerInput::Tap { x, y, hold } => {
                        let (w, h) = self.screen_points(&ctx, &rt, req.aspect).await?;
                        let hold = hold.filter(|s| *s >= 0.35).map(|s| s.min(10.0));
                        UiInput::Tap(tap(unit(x)? * w, unit(y)? * h, hold))
                    }
                    ViewerInput::Swipe { x1, y1, x2, y2, duration } => {
                        let (w, h) = self.screen_points(&ctx, &rt, req.aspect).await?;
                        UiInput::Swipe(SwipeArgs {
                            from: [unit(x1)? * w, unit(y1)? * h],
                            to: [unit(x2)? * w, unit(y2)? * h],
                            duration_seconds: Some(duration.unwrap_or(0.3).clamp(0.05, 5.0)),
                        })
                    }
                    ViewerInput::Type { text } | ViewerInput::PasteAndType { text } => {
                        if text.is_empty() || text.len() > MAX_TEXT {
                            return Err(ViewError::Bad(format!("text must be 1-{MAX_TEXT} bytes")));
                        }
                        let file =
                            self.config.data_dir.join("run").join(format!("{}.txt", uuid::Uuid::new_v4().simple()));
                        std::fs::write(&file, text).context("writing typed text")?;
                        scratch = Some(file.clone());
                        UiInput::Type { text_file: file }
                    }
                    ViewerInput::Key { code } => UiInput::Key(code),
                    ViewerInput::Button { name } => UiInput::Button(name),
                    ViewerInput::Paste { .. } => unreachable!(),
                };
                self.devices.ui(&ctx, &input).await
            }
        };
        if let Some(f) = scratch {
            let _ = std::fs::remove_file(f);
        }
        result?;
        self.store.touch_lease(&v.lease.id, now())?;
        Ok(via)
    }
}

// ---- stream ----

/// Subscribes to the lease's frames, starting its stream when it is the first viewer.
fn subscribe(pool: &Arc<Pool>, v: &Viewing) -> Result<watch::Receiver<Option<Frame>>, ViewError> {
    let (ctx, rt) = pool.viewer_ctx(v)?;
    let max = pool.config.viewer.max_viewers_per_lease;
    let mut hubs = pool.viewers.hubs.lock().expect("hubs mutex");
    if let Some(hub) = hubs.get(&v.lease.id)
        && !hub.cancel.is_cancelled()
    {
        // Count only external subscribers (exclude the stats watcher).
        if hub.tx.receiver_count() > max {
            return Err(ViewError::Full(max));
        }
        let mut rx = hub.tx.subscribe();
        rx.mark_changed();
        return Ok(rx);
    }
    let (tx, rx) = watch::channel(None);
    let initial_cancel = rt.cancel.child_token();
    let initial_opts = StreamPreset::Balanced.opts(&pool.config.viewer);
    let stats = Arc::new(StreamStats {
        started_at_ms: now_ms(),
        frame_count: AtomicU64::new(0),
        frame_bytes: AtomicU64::new(0),
        last_frame_at_ms: AtomicI64::new(0),
    });
    let hub = Arc::new(Hub {
        tx,
        cancel: initial_cancel,
        stream_cancel: StdMutex::new(rt.cancel.child_token()),
        opts: StdMutex::new(initial_opts),
        preset: StdMutex::new(StreamPreset::Balanced),
        stats: stats.clone(),
    });
    hubs.insert(v.lease.id.clone(), hub.clone());
    drop(hubs);

    // Stats subscriber: counts every frame that reaches the watch channel.
    let mut stats_rx = hub.tx.subscribe();
    let stats_for_watcher = stats.clone();
    tokio::spawn(async move {
        while stats_rx.changed().await.is_ok() {
            let frame = stats_rx.borrow_and_update().clone();
            if let Some(f) = frame {
                stats_for_watcher.frame_count.fetch_add(1, Ordering::Relaxed);
                stats_for_watcher.frame_bytes.fetch_add(f.jpeg.len() as u64, Ordering::Relaxed);
                stats_for_watcher.last_frame_at_ms.store(f.captured_at_ms, Ordering::Relaxed);
            }
        }
    });

    // Ends the stream when the last viewer leaves; keeps a watched lease from idling out;
    // logs stats every 30 s.
    let watcher = hub.clone();
    let (store, lease_id) = (pool.store.clone(), v.lease.id.clone());
    let lease_id2 = lease_id.clone();
    tokio::spawn(async move {
        let mut touch = tokio::time::interval(TOUCH_EVERY);
        let mut log = tokio::time::interval(STATS_LOG_EVERY);
        touch.tick().await;
        log.tick().await;
        loop {
            tokio::select! {
                _ = watcher.tx.closed() => break,
                _ = watcher.cancel.cancelled() => break,
                _ = touch.tick() => {
                    let _ = store.touch_lease(&lease_id, now());
                }
                _ = log.tick() => {
                    let st = &watcher.stats;
                    let count = st.frame_count.load(Ordering::Relaxed);
                    let bytes = st.frame_bytes.load(Ordering::Relaxed);
                    // Subtract 1 for the stats watcher itself.
                    let subscribers = watcher.tx.receiver_count().saturating_sub(1);
                    let elapsed_s = (now_ms() - st.started_at_ms).max(1) as f64 / 1000.0;
                    let fps = count as f64 / elapsed_s;
                    let mean_bytes = bytes.checked_div(count).unwrap_or(0);
                    tracing::info!(
                        lease = %lease_id2,
                        capture_fps = format!("{fps:.1}"),
                        mean_frame_bytes = mean_bytes,
                        total_frames = count,
                        subscribers,
                        "stream stats"
                    );
                }
            }
        }
        watcher.cancel.cancel();
    });

    let pool = pool.clone();
    let lease_id = v.lease.id.clone();
    tokio::spawn(async move {
        while !hub.cancel.is_cancelled() {
            let opts = *hub.opts.lock().expect("opts mutex");
            let sc = hub.cancel.child_token();
            *hub.stream_cancel.lock().expect("stream_cancel mutex") = sc.clone();
            if let Err(e) = pool.devices.stream(&ctx, &opts, &hub.tx, &sc).await {
                tracing::warn!(lease = %lease_id, "screen stream: {e:#}");
            }
            // The stream process ended on its own: start it again while someone watches.
            tokio::select! {
                _ = hub.cancel.cancelled() => {}
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }
        let mut hubs = pool.viewers.hubs.lock().expect("hubs mutex");
        if hubs.get(&lease_id).is_some_and(|h| Arc::ptr_eq(h, &hub)) {
            hubs.remove(&lease_id);
        }
    });
    Ok(rx)
}

fn part(frame: &Frame) -> Bytes {
    let hdr = format!(
        "--{BOUNDARY}\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\nX-Capture-Time: {}\r\n\r\n",
        frame.jpeg.len(),
        frame.captured_at_ms
    );
    let mut out = Vec::with_capacity(hdr.len() + frame.jpeg.len() + 2);
    out.extend_from_slice(hdr.as_bytes());
    out.extend_from_slice(&frame.jpeg);
    out.extend_from_slice(b"\r\n");
    Bytes::from(out)
}

// ---- routes ----

/// The body of `POST /v1/leases/{lease_id}/viewer` (control by default) and
/// `POST /v1/admin/leases/{lease_id}/viewer` (view-only by default).
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ViewerRequest {
    pub control: Option<bool>,
}

async fn open(State(pool): State<Arc<Pool>>, Path(ticket): Path<String>) -> Response {
    match pool.open_ticket(&ticket) {
        Ok((sid, secret, lifetime)) => {
            let secure = pool.config.listen.public_url.as_deref().is_some_and(|u| u.starts_with("https://"));
            let cookie = format!(
                "{COOKIE}={secret}; Path=/view/s/{sid}; Max-Age={lifetime}; HttpOnly; SameSite=Strict{}",
                if secure { "; Secure" } else { "" }
            );
            let mut resp = StatusCode::SEE_OTHER.into_response();
            let h = resp.headers_mut();
            h.insert(header::LOCATION, HeaderValue::from_str(&format!("/view/s/{sid}/")).expect("ascii"));
            h.insert(header::SET_COOKIE, HeaderValue::from_str(&cookie).expect("ascii"));
            no_store(h);
            resp
        }
        Err(e) => page_error(e),
    }
}

/// An HTML page for errors a person meets by opening a link.
fn page_error(e: ViewError) -> Response {
    let mut resp = e.into_response();
    let status = resp.status();
    let message = match status {
        StatusCode::UNAUTHORIZED => "This viewer link expired or was already used. Open the device again from Tern.",
        StatusCode::GONE => "The device was released.",
        _ => "The viewer could not open.",
    };
    resp = (status, axum::response::Html(format!(
        "<!doctype html><meta charset=utf-8><title>Seiji</title><link rel=stylesheet href=/view/assets/viewer.css><main class=gone><p>{message}</p></main>"
    )))
        .into_response();
    no_store(resp.headers_mut());
    resp
}

async fn page(State(pool): State<Arc<Pool>>, Path(sid): Path<String>, headers: HeaderMap) -> Response {
    if let Err(e) = pool.viewing(&sid, &headers) {
        return page_error(e);
    }
    let mut resp = axum::response::Html(PAGE).into_response();
    let h = resp.headers_mut();
    no_store(h);
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; img-src 'self' blob:; script-src 'self'; style-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
        ),
    );
    resp
}

async fn asset(Path(name): Path<String>) -> Response {
    let (body, kind) = match name.as_str() {
        "viewer.js" => (SCRIPT, "text/javascript; charset=utf-8"),
        "viewer.css" => (STYLE, "text/css; charset=utf-8"),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let mut resp = ([(header::CONTENT_TYPE, kind)], body).into_response();
    no_store(resp.headers_mut());
    resp
}

async fn info(
    State(pool): State<Arc<Pool>>,
    Path(sid): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ViewError> {
    let v = pool.viewing(&sid, &headers)?;
    let (_, rt) = pool.viewer_ctx(&v)?;
    let model = pool.config.ios.as_ref().and_then(|i| i.models.get(&v.lease.model));
    let hubs = pool.viewers.hubs.lock().expect("hubs mutex");
    let (viewers, preset, opts) = if let Some(hub) = hubs.get(&v.lease.id) {
        let viewers = hub.tx.receiver_count().saturating_sub(1); // exclude stats watcher
        let preset = *hub.preset.lock().expect("preset mutex");
        let opts = *hub.opts.lock().expect("opts mutex");
        (viewers, Some(preset), Some(opts))
    } else {
        (0, None, None)
    };
    drop(hubs);
    let busy = rt.op.try_lock().is_err();
    let mut resp = Json(serde_json::json!({
        "lease_id": v.lease.id,
        "user": v.user.name,
        "model": v.lease.model,
        "device_type": model.map(|m| m.device_type.clone()),
        "runtime": model.map(|m| m.runtime.clone()),
        "udid": v.lease.device_id,
        "started_at": v.lease.started_at,
        "ends_at_latest": v.lease.started_at + v.user.quota.max_lease_minutes * 60,
        "control": v.control,
        "busy": busy,
        "viewers": viewers,
        "fps": opts.map_or(pool.config.viewer.fps, |o| o.fps),
        "app": rt.app.lock().expect("app mutex").clone(),
        "preset": preset,
        "stream_opts": opts.map(|o| serde_json::json!({"fps": o.fps, "scale": o.scale, "quality": o.quality})),
        // Lets the page correct `X-Capture-Time` for its own clock when it shows a frame's age.
        "server_time_ms": now_ms(),
    }))
    .into_response();
    no_store(resp.headers_mut());
    Ok(resp)
}

async fn stats(
    State(pool): State<Arc<Pool>>,
    Path(sid): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ViewError> {
    let v = pool.viewing(&sid, &headers)?;
    let hubs = pool.viewers.hubs.lock().expect("hubs mutex");
    let hub = hubs.get(&v.lease.id).ok_or_else(|| ViewError::Gone("No active stream.".into()))?;
    let st = &hub.stats;
    let started_at_ms = st.started_at_ms;
    let count = st.frame_count.load(Ordering::Relaxed);
    let bytes = st.frame_bytes.load(Ordering::Relaxed);
    let subscribers = hub.tx.receiver_count().saturating_sub(1);
    let last_frame_ms = st.last_frame_at_ms.load(Ordering::Relaxed);
    let preset = *hub.preset.lock().expect("preset mutex");
    let opts = *hub.opts.lock().expect("opts mutex");
    drop(hubs);
    let elapsed_s = (now_ms() - started_at_ms).max(1) as f64 / 1000.0;
    let fps = count as f64 / elapsed_s;
    let mean_bytes = bytes.checked_div(count).unwrap_or(0);
    let mut resp = Json(serde_json::json!({
        "stream": {
            "started_at_ms": started_at_ms,
            "fps": (fps * 10.0).round() / 10.0,
            "mean_frame_bytes": mean_bytes,
            "total_frames": count,
            "total_bytes": bytes,
            "subscribers": subscribers,
            "last_frame_at_ms": last_frame_ms,
        },
        "preset": preset,
        "opts": {"fps": opts.fps, "scale": opts.scale, "quality": opts.quality},
    }))
    .into_response();
    no_store(resp.headers_mut());
    Ok(resp)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PresetRequest {
    preset: StreamPreset,
}

async fn preset(
    State(pool): State<Arc<Pool>>,
    Path(sid): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, ViewError> {
    if headers.get("x-seiji-view").and_then(|v| v.to_str().ok()) != Some("1") {
        return Err(ViewError::Bad("missing X-Seiji-View header".into()));
    }
    let v = pool.viewing(&sid, &headers)?;
    if !v.control {
        return Err(ViewError::ViewOnly);
    }
    let req: PresetRequest =
        serde_json::from_slice(&body).map_err(|e| ViewError::Bad(format!("invalid preset: {e}")))?;
    let new_opts = req.preset.opts(&pool.config.viewer);
    let hubs = pool.viewers.hubs.lock().expect("hubs mutex");
    let hub = hubs.get(&v.lease.id).ok_or_else(|| ViewError::Gone("No active stream.".into()))?;
    *hub.opts.lock().expect("opts mutex") = new_opts;
    *hub.preset.lock().expect("preset mutex") = req.preset;
    // Cancel only the AXe stream; the spawn loop will restart with the new opts.
    hub.stream_cancel.lock().expect("stream_cancel mutex").cancel();
    drop(hubs);
    let mut resp = Json(serde_json::json!({
        "preset": req.preset,
        "opts": {"fps": new_opts.fps, "scale": new_opts.scale, "quality": new_opts.quality},
    }))
    .into_response();
    no_store(resp.headers_mut());
    Ok(resp)
}

async fn stream(
    State(pool): State<Arc<Pool>>,
    Path(sid): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ViewError> {
    let v = pool.viewing(&sid, &headers)?;
    let mut rx = subscribe(&pool, &v)?;
    // Identical frames are not sent again, so a screen that does not change produces no new
    // frame: a viewer that joins (or reconnects) gets the current one at once, and the last
    // frame is repeated every FRAME_REPEAT so the page and proxies see a live stream.
    rx.mark_changed();
    let frames = futures_util::stream::unfold(rx, |mut rx| async move {
        loop {
            // Ok(Err): the stream ended (the lease went away or the last viewer left).
            if let Ok(changed) = tokio::time::timeout(FRAME_REPEAT, rx.changed()).await {
                changed.ok()?;
            }
            let frame = rx.borrow_and_update().clone();
            if let Some(f) = frame {
                return Some((Ok::<_, Infallible>(part(&f)), rx));
            }
        }
    });
    let mut resp = Body::from_stream(frames).into_response();
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&format!("multipart/x-mixed-replace; boundary={BOUNDARY}")).expect("ascii"),
    );
    no_store(h);
    Ok(resp)
}

async fn input(
    State(pool): State<Arc<Pool>>,
    Path(sid): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, ViewError> {
    // A cross-site form cannot set this header, and `SameSite=Strict` keeps the cookie home.
    if headers.get("x-seiji-view").and_then(|v| v.to_str().ok()) != Some("1") {
        return Err(ViewError::Bad("missing X-Seiji-View header".into()));
    }
    let v = pool.viewing(&sid, &headers)?;
    let req: InputRequest = serde_json::from_slice(&body).map_err(|e| ViewError::Bad(format!("invalid input: {e}")))?;
    let mut resp = match pool.viewer_input(&v, req).await? {
        // A paste says where the text went, so the page can tell the person.
        Some(via) => Json(serde_json::json!({ "via": via })).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    };
    no_store(resp.headers_mut());
    Ok(resp)
}

async fn screenshot(
    State(pool): State<Arc<Pool>>,
    Path(sid): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ViewError> {
    let v = pool.viewing(&sid, &headers)?;
    let (ctx, _) = pool.viewer_ctx(&v)?;
    let jpeg = pool.devices.screenshot(&ctx).await?;
    let name = format!("{}-{}.jpg", v.lease.id, now());
    let mut resp = jpeg.into_response();
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));
    h.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")).expect("ascii"),
    );
    no_store(h);
    Ok(resp)
}

/// The cookie-authenticated viewer routes (no bearer token: a browser page cannot send one).
pub fn routes() -> Router<Arc<Pool>> {
    Router::new()
        .route("/view/open/{ticket}", get(open))
        .route("/view/assets/{name}", get(asset))
        .route("/view/s/{sid}/", get(page))
        .route("/view/s/{sid}/info", get(info))
        .route("/view/s/{sid}/stats", get(stats))
        .route("/view/s/{sid}/stream", get(stream))
        .route("/view/s/{sid}/input", post(input))
        .route("/view/s/{sid}/preset", post(preset))
        .route("/view/s/{sid}/screenshot.jpg", get(screenshot))
}
