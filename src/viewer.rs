//! Live screen viewer: watch and drive a leased simulator from a browser page (Tern's browser
//! block, or any browser).
//!
//! A user with a token asks for a viewer link (`POST /v1/leases/{id}/viewer` or the
//! `share_screen` tool), or an admin for any user's lease (`POST /v1/admin/leases/{id}/viewer`).
//! The link carries a one-time ticket that lives 60 s. Opening it trades the ticket for a
//! session: a random id in the path and a random secret in an `HttpOnly`, `SameSite=Strict`
//! cookie scoped to that path. Every request checks the session, its user (and the admin who
//! asked for a link to another user's lease) and that the lease is still the user's and ready;
//! the page's WebSocket checks them again every 2 s. One `seiji-sim` helper per lease encodes the
//! screen as H.264 for all its viewers and takes their touches and keys over one persistent HID
//! connection; it stops when the last viewer leaves or the lease ends.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::{
    Json, Router,
    extract::{
        Path, State,
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{OwnedMutexGuard, broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::config::Platform;
use crate::devices::{
    DeviceCtx, DeviceOp, Edge, ScreenButton, ScreenCommand, ScreenEvent, ScreenFormat, StreamOpts, TouchPhase, UiInput,
    VideoFrame,
};
use crate::pool::{LeaseRuntime, Pool};
use crate::store::{Lease, LeaseStatus, User, now};

const TICKET_SECONDS: i64 = 60;
const COOKIE: &str = "seiji_view";
/// A watched lease counts as used: the stream touches it this often.
const TOUCH_EVERY: Duration = Duration::from_secs(60);
/// How often the stream logs its stats for the operator.
const STATS_LOG_EVERY: Duration = Duration::from_secs(30);
const MAX_TEXT: usize = 10_000;
/// HID usage codes for Paste: Left Command + V.
const CMD: u8 = 227;
const KEY_V: u8 = 25;
/// How long input waits for the device before it answers busy.
const INPUT_WAIT: Duration = Duration::from_secs(1);
/// A WebSocket checks its session (user, lease, cookie) this often.
const RECHECK_EVERY: Duration = Duration::from_secs(2);
/// A viewer is congested when its oldest unacknowledged frame is older than its base round trip
/// plus this: frames are queueing on the way. Seiji then skips frames for it and resumes at a
/// keyframe, so the picture stays this close to live on a slow link.
const QUEUE_BUDGET: Duration = Duration::from_millis(200);
/// The base round trip before the first acknowledgement, and the most frames in flight anyway.
const FIRST_RTT: Duration = Duration::from_millis(300);
const MAX_IN_FLIGHT: usize = 60;
/// The base round trip is the lowest seen in this long.
const RTT_WINDOW: Duration = Duration::from_secs(10);
/// Keyframe requests to the helper are at least this far apart.
const KEYFRAME_GAP: Duration = Duration::from_millis(500);
/// A finger left down this long without a move is lifted, and the device is free again.
const GESTURE_IDLE: Duration = Duration::from_secs(10);
/// How long a hardware button is held.
const BUTTON_HOLD: Duration = Duration::from_millis(60);
/// Viewer inputs touch the lease at most this often.
const INPUT_TOUCH_EVERY: Duration = Duration::from_secs(5);
/// The largest message a page may send (a paste of `MAX_TEXT` bytes, JSON-escaped).
const MAX_CLIENT_MESSAGE: usize = 64 * 1024;
/// Format and frames a slow viewer may fall behind before it resyncs at a keyframe.
const VIDEO_BUFFER: usize = 256;
/// Adaptive bitrate: a congested viewer cuts the bitrate to 70 %, at most every 2 s and never
/// below 20 % of the preset; after 4 s without congestion it grows 15 % every 2 s.
const RATE_DOWN: f64 = 0.7;
const RATE_UP: f64 = 1.15;
const RATE_FLOOR: f64 = 0.2;
const RATE_STEP: Duration = Duration::from_secs(2);
const RATE_CALM: Duration = Duration::from_secs(4);
/// WebSocket close codes the page acts on: open a new link, or the device is gone.
const CLOSE_EXPIRED: u16 = 4401;
const CLOSE_GONE: u16 = 4410;
const CLOSE_INTERNAL: u16 = 1011;
/// The binary video message: flags, frame number, capture time, then the AVCC sample.
const FRAME_HEADER: usize = 13;

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

/// Per-stream counters for the stats route and the log.
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
        let bitrate = cfg.bitrate_kbps * 1000;
        match self {
            StreamPreset::Smooth => StreamOpts { fps: cfg.fps, scale: cfg.scale.min(0.35), bitrate: bitrate / 2 },
            StreamPreset::Balanced => StreamOpts { fps: cfg.fps, scale: cfg.scale, bitrate },
            StreamPreset::Sharp => {
                StreamOpts { fps: cfg.fps, scale: cfg.scale.max(0.8), bitrate: (bitrate * 2).min(20_000_000) }
            }
        }
    }
}

/// What a viewer receives: a decoder setup, or one frame. Both are built once for all viewers.
#[derive(Clone)]
enum Video {
    Format(Arc<str>),
    Frame(Arc<Encoded>),
}

struct Encoded {
    /// The hub's frame number; viewers acknowledge it.
    seq: u32,
    key: bool,
    /// The whole binary WebSocket message.
    message: Bytes,
}

/// The bitrate the helper encodes at, adapted to the slowest viewer.
struct Rate {
    preset: StreamPreset,
    target: StreamOpts,
    bitrate: u32,
    changed_at: Instant,
    congested_at: Option<Instant>,
}

/// One helper per watched lease, shared by its viewers.
struct Hub {
    /// Input and control for the running helper; `None` between restarts.
    commands: StdMutex<Option<mpsc::Sender<ScreenCommand>>>,
    video: broadcast::Sender<Video>,
    /// The latest decoder setup, for viewers that join.
    format: StdMutex<Option<Arc<str>>>,
    cancel: CancellationToken,
    viewers: AtomicUsize,
    /// Viewer inputs that hold the lease's op lock: the page does not call that busy.
    holding: AtomicUsize,
    next_seq: AtomicU64,
    rate: StdMutex<Rate>,
    keyframe_at: StdMutex<Option<Instant>>,
    stats: StreamStats,
}

impl Hub {
    fn new(cancel: CancellationToken, cfg: &crate::config::Viewer) -> Self {
        let target = StreamPreset::Balanced.opts(cfg);
        Self {
            commands: StdMutex::new(None),
            video: broadcast::channel(VIDEO_BUFFER).0,
            format: StdMutex::new(None),
            cancel,
            viewers: AtomicUsize::new(0),
            holding: AtomicUsize::new(0),
            next_seq: AtomicU64::new(0),
            rate: StdMutex::new(Rate {
                preset: StreamPreset::Balanced,
                target,
                bitrate: target.bitrate,
                // The first congestion may cut the bitrate at once.
                changed_at: Instant::now().checked_sub(RATE_STEP).unwrap_or_else(Instant::now),
                congested_at: None,
            }),
            keyframe_at: StdMutex::new(None),
            stats: StreamStats {
                started_at_ms: now_ms(),
                frame_count: AtomicU64::new(0),
                frame_bytes: AtomicU64::new(0),
                last_frame_at_ms: AtomicI64::new(0),
            },
        }
    }

    /// Hands a command to the helper. A missing helper (restarting) drops it.
    fn send(&self, cmd: ScreenCommand) {
        if let Some(tx) = &*self.commands.lock().expect("commands mutex") {
            let _ = tx.try_send(cmd);
        }
    }

    /// Waits until the helper has every command sent so far (at most `INPUT_WAIT`).
    async fn sync(&self) {
        let (done, wait) = tokio::sync::oneshot::channel();
        self.send(ScreenCommand::Sync(done));
        let _ = tokio::time::timeout(INPUT_WAIT, wait).await;
    }

    fn opts(&self) -> StreamOpts {
        let rate = self.rate.lock().expect("rate mutex");
        StreamOpts { bitrate: rate.bitrate, ..rate.target }
    }

    fn keyframe(&self, force: bool) {
        let mut at = self.keyframe_at.lock().expect("keyframe mutex");
        if force || at.is_none_or(|t| t.elapsed() >= KEYFRAME_GAP) {
            *at = Some(Instant::now());
            self.send(ScreenCommand::Keyframe);
        }
    }

    /// A viewer could not keep up: lower the bitrate.
    fn congested(&self) {
        let opts = {
            let mut rate = self.rate.lock().expect("rate mutex");
            rate.congested_at = Some(Instant::now());
            let floor = (f64::from(rate.target.bitrate) * RATE_FLOOR) as u32;
            if rate.changed_at.elapsed() < RATE_STEP || rate.bitrate <= floor {
                return;
            }
            rate.bitrate = ((f64::from(rate.bitrate) * RATE_DOWN) as u32).max(floor);
            rate.changed_at = Instant::now();
            StreamOpts { bitrate: rate.bitrate, ..rate.target }
        };
        self.send(ScreenCommand::Encode(opts));
    }

    /// Called every second: grow the bitrate back toward the preset while nobody is congested.
    fn recover(&self) {
        let opts = {
            let mut rate = self.rate.lock().expect("rate mutex");
            let calm = rate.congested_at.is_none_or(|t| t.elapsed() >= RATE_CALM);
            if !calm || rate.changed_at.elapsed() < RATE_STEP || rate.bitrate >= rate.target.bitrate {
                return;
            }
            rate.bitrate = ((f64::from(rate.bitrate) * RATE_UP) as u32).min(rate.target.bitrate);
            rate.changed_at = Instant::now();
            StreamOpts { bitrate: rate.bitrate, ..rate.target }
        };
        self.send(ScreenCommand::Encode(opts));
    }

    fn set_preset(&self, preset: StreamPreset, cfg: &crate::config::Viewer) -> StreamOpts {
        let opts = preset.opts(cfg);
        {
            let mut rate = self.rate.lock().expect("rate mutex");
            *rate =
                Rate { preset, target: opts, bitrate: opts.bitrate, changed_at: Instant::now(), congested_at: None };
        }
        self.send(ScreenCommand::Encode(opts));
        opts
    }

    fn on_event(&self, event: ScreenEvent, lease_id: &str) {
        match event {
            ScreenEvent::Ready => tracing::debug!(lease = %lease_id, "screen helper ready"),
            ScreenEvent::Warning(w) => tracing::warn!(lease = %lease_id, "seiji-sim: {w}"),
            ScreenEvent::Format(f) => {
                let text: Arc<str> = format_message(&f).into();
                *self.format.lock().expect("format mutex") = Some(text.clone());
                let _ = self.video.send(Video::Format(text));
            }
            ScreenEvent::Frame(f) => {
                self.stats.frame_count.fetch_add(1, Ordering::Relaxed);
                self.stats.frame_bytes.fetch_add(f.data.len() as u64, Ordering::Relaxed);
                self.stats.last_frame_at_ms.store(f.captured_us / 1000, Ordering::Relaxed);
                let seq = self.next_seq.fetch_add(1, Ordering::Relaxed) as u32;
                let _ = self.video.send(Video::Frame(Arc::new(frame_message(seq, &f))));
            }
        }
    }
}

fn format_message(f: &ScreenFormat) -> String {
    serde_json::json!({
        "t": "format",
        "codec": f.codec,
        "width": f.width,
        "height": f.height,
        "description": f.description,
    })
    .to_string()
}

fn frame_message(seq: u32, f: &VideoFrame) -> Encoded {
    let mut out = Vec::with_capacity(FRAME_HEADER + f.data.len());
    out.push(u8::from(f.key));
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(&f.captured_us.to_be_bytes());
    out.extend_from_slice(&f.data);
    Encoded { seq, key: f.key, message: Bytes::from(out) }
}

/// Tickets, sessions and the one helper per watched lease. In memory: a restart ends every
/// lease, and with it every viewer.
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
    /// A WebSocket from a page on another site.
    CrossOrigin,
    Bad(String),
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for ViewError {
    fn from(e: anyhow::Error) -> Self {
        ViewError::Internal(e)
    }
}

impl ViewError {
    fn parts(&self) -> (StatusCode, &'static str, String) {
        match self {
            ViewError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "expired",
                "This viewer link expired. Open the device again from Tern.".into(),
            ),
            ViewError::Gone(why) => (StatusCode::GONE, "gone", why.clone()),
            ViewError::Busy => {
                (StatusCode::CONFLICT, "busy", "A run is using the device. Input works again when it ends.".into())
            }
            ViewError::ViewOnly => (StatusCode::FORBIDDEN, "view_only", "This viewer is view-only.".into()),
            ViewError::Full(n) => {
                (StatusCode::TOO_MANY_REQUESTS, "full", format!("This device already has {n} viewers open."))
            }
            ViewError::CrossOrigin => {
                (StatusCode::FORBIDDEN, "cross_origin", "The viewer only accepts its own page.".into())
            }
            ViewError::Bad(why) => (StatusCode::BAD_REQUEST, "bad_request", why.clone()),
            ViewError::Internal(e) => (StatusCode::INTERNAL_SERVER_ERROR, "internal", format!("{e:#}")),
        }
    }

    /// The error as a WebSocket message; `id` names the input it answers.
    fn message(&self, id: Option<u32>) -> Message {
        let (_, code, message) = self.parts();
        let mut v = serde_json::json!({ "t": "error", "code": code, "message": message });
        if let Some(id) = id {
            v["id"] = id.into();
        }
        text(v)
    }

    /// The close frame that ends a WebSocket for this error.
    fn close(&self) -> CloseFrame {
        let (_, _, message) = self.parts();
        let code = match self {
            ViewError::Unauthorized => CLOSE_EXPIRED,
            ViewError::Gone(_) => CLOSE_GONE,
            _ => CLOSE_INTERNAL,
        };
        // A close reason holds at most 123 bytes.
        CloseFrame { code, reason: crate::exec::head(&message, 120).to_string().into() }
    }
}

impl IntoResponse for ViewError {
    fn into_response(self) -> Response {
        let (status, code, message) = self.parts();
        let mut resp =
            (status, Json(serde_json::json!({ "error": { "code": code, "message": message } }))).into_response();
        no_store(resp.headers_mut());
        resp
    }
}

fn text(v: serde_json::Value) -> Message {
    Message::Text(v.to_string().into())
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
        self.check_session(sid, cookie(headers).ok_or(ViewError::Unauthorized)?)
    }

    fn check_session(&self, sid: &str, secret: &str) -> Result<Viewing, ViewError> {
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
}

fn cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| c.trim().strip_prefix(COOKIE)?.strip_prefix('='))
}

/// A browser names the page that opens a WebSocket in `Origin`; it must be this host, so a page
/// on another site cannot drive the device with the person's cookie. A request without `Origin`
/// does not come from a browser page.
fn same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else { return true };
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok());
    let origin = origin.to_str().ok().and_then(|o| o.strip_prefix("https://").or_else(|| o.strip_prefix("http://")));
    matches!((origin, host), (Some(o), Some(h)) if o.eq_ignore_ascii_case(h))
}

// ---- the helper of a lease ----

/// One viewer of a hub. Dropping it ends the hub with its last viewer.
struct Joined {
    pool: Arc<Pool>,
    lease_id: String,
    hub: Arc<Hub>,
}

impl Drop for Joined {
    fn drop(&mut self) {
        // Under the hubs lock, so a viewer that joins now either sees this hub alive or makes a new one.
        let mut hubs = self.pool.viewers.hubs.lock().expect("hubs mutex");
        if self.hub.viewers.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.hub.cancel.cancel();
            if hubs.get(&self.lease_id).is_some_and(|h| Arc::ptr_eq(h, &self.hub)) {
                hubs.remove(&self.lease_id);
            }
        }
    }
}

/// Joins the lease's hub, starting its helper for the first viewer.
fn join(
    pool: &Arc<Pool>,
    ctx: &DeviceCtx,
    rt: &LeaseRuntime,
) -> Result<(Joined, broadcast::Receiver<Video>), ViewError> {
    let max = pool.config.viewer.max_viewers_per_lease;
    let mut hubs = pool.viewers.hubs.lock().expect("hubs mutex");
    if let Some(hub) = hubs.get(&ctx.lease_id)
        && !hub.cancel.is_cancelled()
    {
        if hub.viewers.load(Ordering::SeqCst) >= max {
            return Err(ViewError::Full(max));
        }
        hub.viewers.fetch_add(1, Ordering::SeqCst);
        let rx = hub.video.subscribe();
        hub.keyframe(true);
        let joined = Joined { pool: pool.clone(), lease_id: ctx.lease_id.clone(), hub: hub.clone() };
        return Ok((joined, rx));
    }
    let hub = Arc::new(Hub::new(rt.cancel.child_token(), &pool.config.viewer));
    hub.viewers.store(1, Ordering::SeqCst);
    let rx = hub.video.subscribe();
    hubs.insert(ctx.lease_id.clone(), hub.clone());
    drop(hubs);
    tokio::spawn(run_hub(pool.clone(), ctx.clone(), hub.clone()));
    Ok((Joined { pool: pool.clone(), lease_id: ctx.lease_id.clone(), hub }, rx))
}

/// Runs the helper while anyone watches (again 1 s after it ends), keeps the lease from idling
/// out, adapts the bitrate and logs stats.
async fn run_hub(pool: Arc<Pool>, ctx: DeviceCtx, hub: Arc<Hub>) {
    let lease_id = ctx.lease_id.clone();
    let ticker = {
        let (hub, store, lease_id) = (hub.clone(), pool.store.clone(), lease_id.clone());
        tokio::spawn(async move {
            let mut touch = tokio::time::interval(TOUCH_EVERY);
            let mut log = tokio::time::interval(STATS_LOG_EVERY);
            let mut rate = tokio::time::interval(Duration::from_secs(1));
            touch.tick().await;
            log.tick().await;
            loop {
                tokio::select! {
                    _ = hub.cancel.cancelled() => break,
                    _ = touch.tick() => { let _ = store.touch_lease(&lease_id, now()); }
                    _ = rate.tick() => hub.recover(),
                    _ = log.tick() => {
                        let st = &hub.stats;
                        let count = st.frame_count.load(Ordering::Relaxed);
                        let bytes = st.frame_bytes.load(Ordering::Relaxed);
                        let elapsed_s = (now_ms() - st.started_at_ms).max(1) as f64 / 1000.0;
                        tracing::info!(
                            lease = %lease_id,
                            fps = format!("{:.1}", count as f64 / elapsed_s),
                            kbps = (bytes as f64 * 8.0 / elapsed_s / 1000.0) as u64,
                            bitrate = hub.opts().bitrate,
                            total_frames = count,
                            viewers = hub.viewers.load(Ordering::Relaxed),
                            "stream stats"
                        );
                    }
                }
            }
        })
    };
    while !hub.cancel.is_cancelled() {
        let (cmd_tx, cmd_rx) = mpsc::channel(1024);
        let (ev_tx, mut ev_rx) = mpsc::channel(64);
        *hub.commands.lock().expect("commands mutex") = Some(cmd_tx);
        let pump = async {
            while let Some(event) = ev_rx.recv().await {
                hub.on_event(event, &lease_id);
            }
        };
        let (result, ()) = tokio::join!(pool.devices.screen(&ctx, hub.opts(), cmd_rx, ev_tx, &hub.cancel), pump);
        *hub.commands.lock().expect("commands mutex") = None;
        if let Err(e) = result {
            tracing::warn!(lease = %lease_id, "screen stream: {e:#}");
        }
        tokio::select! {
            _ = hub.cancel.cancelled() => {}
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
    }
    ticker.abort();
    let mut hubs = pool.viewers.hubs.lock().expect("hubs mutex");
    if hubs.get(&lease_id).is_some_and(|h| Arc::ptr_eq(h, &hub)) {
        hubs.remove(&lease_id);
    }
}

// ---- a viewer's WebSocket ----

/// What the page sends. Points are fractions (0-1) of the picture.
#[derive(Debug, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case", deny_unknown_fields)]
enum ClientMsg {
    /// The last frame the page received.
    Ack {
        seq: u32,
    },
    /// Echoed back with the server's clock, for the round trip and the frame age.
    Ping {
        at: f64,
    },
    Preset {
        name: StreamPreset,
    },
    Touch {
        phase: TouchPhase,
        x: f64,
        y: f64,
        #[serde(default)]
        edge: Option<Edge>,
    },
    /// A HID usage pressed with modifier usages (224-231) held.
    Key {
        usage: u8,
        #[serde(default)]
        mods: Vec<u8>,
    },
    Button {
        name: ScreenButton,
    },
    /// Type the text (AXe, US keyboard characters).
    Type {
        id: u32,
        text: String,
    },
    /// Put the text on the simulator clipboard and press Cmd+V; typed when the clipboard
    /// cannot be set.
    Paste {
        id: u32,
        text: String,
    },
    /// Type the text character by character, for fields that refuse paste.
    PasteAndType {
        id: u32,
        text: String,
    },
}

/// Which frames a viewer gets: all of them while it keeps up, then none until a keyframe once
/// frames queue up on the way to it.
struct Flow {
    /// Frames sent and not yet acknowledged, oldest first, with when they were sent.
    in_flight: VecDeque<(u32, Instant)>,
    /// The lowest round trip in the current window, and when the window started.
    base_rtt: Option<(Duration, Instant)>,
    need_key: bool,
    /// Congested: ask for a keyframe once the queue drains.
    resync: bool,
}

impl Flow {
    fn new() -> Self {
        Self { in_flight: VecDeque::new(), base_rtt: None, need_key: true, resync: false }
    }

    /// How old the oldest unacknowledged frame may get.
    fn limit(&self) -> Duration {
        self.base_rtt.map_or(FIRST_RTT, |(rtt, _)| rtt) + QUEUE_BUDGET
    }

    fn queued(&self) -> bool {
        self.in_flight.len() >= MAX_IN_FLIGHT
            || self.in_flight.front().is_some_and(|(_, at)| at.elapsed() > self.limit())
    }

    fn admit(&mut self, f: &Encoded, hub: &Hub) -> bool {
        if self.queued() {
            hub.congested();
            self.need_key = true;
            self.resync = true;
            return false;
        }
        if self.need_key && !f.key {
            if !self.resync {
                hub.keyframe(false);
            }
            return false;
        }
        self.need_key = false;
        self.resync = false;
        self.in_flight.push_back((f.seq, Instant::now()));
        true
    }

    fn ack(&mut self, seq: u32, hub: &Hub) {
        if let Some(pos) = self.in_flight.iter().position(|(s, _)| *s == seq) {
            let rtt = self.in_flight[pos].1.elapsed();
            self.in_flight.drain(..=pos);
            self.base_rtt = match self.base_rtt {
                Some((base, since)) if rtt >= base && since.elapsed() < RTT_WINDOW => Some((base, since)),
                _ => Some((rtt, Instant::now())),
            };
        }
        if self.resync && !self.queued() && self.in_flight.len() <= 1 {
            self.resync = false;
            hub.keyframe(false);
        }
    }
}

/// A viewer's WebSocket and what it holds.
struct Live {
    pool: Arc<Pool>,
    sid: String,
    secret: String,
    control: bool,
    ctx: DeviceCtx,
    rt: Arc<LeaseRuntime>,
    joined: Joined,
}

impl Live {
    /// Why the stream ended for this viewer.
    fn ended(&self) -> CloseFrame {
        match self.pool.check_session(&self.sid, &self.secret) {
            Err(e) => e.close(),
            Ok(_) => ViewError::Gone("The device is being released.".into()).close(),
        }
    }
}

async fn session(live: Live, mut video: broadcast::Receiver<Video>, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let hub = live.joined.hub.clone();
    let (out_tx, mut out_rx) = mpsc::channel::<Message>(64);
    let (in_tx, in_rx) = mpsc::channel::<ClientMsg>(1024);
    let input = Input {
        pool: live.pool.clone(),
        ctx: live.ctx.clone(),
        rt: live.rt.clone(),
        hub: hub.clone(),
        gesture: None,
        touched: None,
    };
    let input = tokio::spawn(run_input(input, in_rx, out_tx));
    let mut flow = Flow::new();
    // The format may be in the cache and in the channel both: send each one once.
    let mut sent_format = hub.format.lock().expect("format mutex").clone();
    let mut close = None;
    if let Some(f) = &sent_format
        && sink.send(Message::Text(f.to_string().into())).await.is_err()
    {
        close = Some(None);
    }
    let mut recheck = tokio::time::interval(RECHECK_EVERY);
    recheck.tick().await;
    while close.is_none() {
        tokio::select! {
            v = video.recv() => match v {
                Ok(Video::Format(f)) => {
                    if !sent_format.as_ref().is_some_and(|s| Arc::ptr_eq(s, &f)) {
                        flow.need_key = true;
                        if sink.send(Message::Text(f.to_string().into())).await.is_err() {
                            close = Some(None);
                        }
                        sent_format = Some(f);
                    }
                }
                Ok(Video::Frame(f)) => {
                    if flow.admit(&f, &hub) && sink.send(Message::Binary(f.message.clone())).await.is_err() {
                        close = Some(None);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    flow.need_key = true;
                    hub.keyframe(false);
                }
                Err(broadcast::error::RecvError::Closed) => close = Some(Some(live.ended())),
            },
            m = stream.next() => match m {
                Some(Ok(Message::Text(t))) => {
                    let reply = match serde_json::from_str::<ClientMsg>(&t) {
                        Ok(ClientMsg::Ack { seq }) => {
                            flow.ack(seq, &hub);
                            None
                        }
                        Ok(ClientMsg::Ping { at }) => Some(text(serde_json::json!({ "t": "pong", "at": at, "server_ms": now_ms() }))),
                        Ok(ClientMsg::Preset { .. } | ClientMsg::Touch { .. } | ClientMsg::Key { .. } | ClientMsg::Button { .. } | ClientMsg::Type { .. } | ClientMsg::Paste { .. } | ClientMsg::PasteAndType { .. })
                            if !live.control => view_only(&t),
                        Ok(ClientMsg::Preset { name }) => {
                            let opts = hub.set_preset(name, &live.pool.config.viewer);
                            Some(text(serde_json::json!({ "t": "preset", "preset": name, "opts": opts })))
                        }
                        Ok(msg) => in_tx.try_send(msg).err().map(|_| ViewError::Busy.message(None)),
                        Err(e) => Some(ViewError::Bad(format!("invalid message: {e}")).message(None)),
                    };
                    if let Some(r) = reply
                        && sink.send(r).await.is_err()
                    {
                        close = Some(None);
                    }
                }
                Some(Ok(Message::Close(_)) | Err(_)) | None => close = Some(None),
                Some(Ok(_)) => {}
            },
            Some(m) = out_rx.recv() => {
                if sink.send(m).await.is_err() {
                    close = Some(None);
                }
            }
            _ = recheck.tick() => {
                if let Err(e) = live.pool.check_session(&live.sid, &live.secret) {
                    close = Some(Some(e.close()));
                }
            }
            _ = hub.cancel.cancelled() => close = Some(Some(live.ended())),
        }
    }
    if let Some(Some(frame)) = close {
        let _ = sink.send(Message::Close(Some(frame))).await;
    }
    // The input task lifts a finger still down before it ends.
    drop(in_tx);
    let _ = input.await;
}

/// A view-only page is told once per gesture, not for every move.
fn view_only(msg: &str) -> Option<Message> {
    let quiet = msg.contains(r#""phase":"move""#) || msg.contains(r#""phase":"up""#);
    (!quiet).then(|| ViewError::ViewOnly.message(None))
}

/// The lease's op lock, held for one input or one gesture.
struct Held {
    _op: OwnedMutexGuard<()>,
    hub: Arc<Hub>,
}

impl Drop for Held {
    fn drop(&mut self) {
        self.hub.holding.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Waits briefly for the device: a run (or another viewer's gesture) holds it, else.
async fn hold(rt: &LeaseRuntime, hub: &Arc<Hub>) -> Result<Held, ViewError> {
    let op = tokio::time::timeout(INPUT_WAIT, rt.op.clone().lock_owned()).await.map_err(|_| ViewError::Busy)?;
    if rt.cancel.is_cancelled() {
        return Err(ViewError::Gone("The device is being released.".into()));
    }
    hub.holding.fetch_add(1, Ordering::SeqCst);
    Ok(Held { _op: op, hub: hub.clone() })
}

/// A finger that is down: it holds the device until it lifts.
struct Gesture {
    _held: Held,
    x: f64,
    y: f64,
    at: Instant,
}

fn point(x: f64, y: f64) -> Result<(), ViewError> {
    if (0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y) {
        Ok(())
    } else {
        Err(ViewError::Bad("coordinates must be between 0 and 1".into()))
    }
}

/// One viewer's input, applied in order: touches, keys and buttons through the helper, text
/// through AXe.
struct Input {
    pool: Arc<Pool>,
    ctx: DeviceCtx,
    rt: Arc<LeaseRuntime>,
    hub: Arc<Hub>,
    gesture: Option<Gesture>,
    touched: Option<Instant>,
}

impl Input {
    /// Lifts a finger that is still down, and frees the device.
    fn lift(&mut self) {
        if let Some(g) = self.gesture.take() {
            self.hub.send(ScreenCommand::Touch { phase: TouchPhase::Up, x: g.x, y: g.y, edge: None });
        }
    }

    async fn apply(&mut self, msg: ClientMsg) -> Option<Message> {
        let id = match &msg {
            ClientMsg::Type { id, .. } | ClientMsg::Paste { id, .. } | ClientMsg::PasteAndType { id, .. } => Some(*id),
            _ => None,
        };
        let reply = match self.apply_inner(msg).await {
            Ok(r) => r,
            Err(e) => Some(e.message(id)),
        };
        if self.touched.is_none_or(|t| t.elapsed() >= INPUT_TOUCH_EVERY) {
            self.touched = Some(Instant::now());
            let _ = self.pool.store.touch_lease(&self.ctx.lease_id, now());
        }
        reply
    }

    async fn apply_inner(&mut self, msg: ClientMsg) -> Result<Option<Message>, ViewError> {
        if !matches!(msg, ClientMsg::Touch { phase: TouchPhase::Move | TouchPhase::Up, .. }) {
            self.lift();
        }
        let hub = self.hub.clone();
        match msg {
            ClientMsg::Touch { phase: TouchPhase::Down, x, y, edge } => {
                point(x, y)?;
                let held = hold(&self.rt, &hub).await?;
                hub.send(ScreenCommand::Touch { phase: TouchPhase::Down, x, y, edge });
                self.gesture = Some(Gesture { _held: held, x, y, at: Instant::now() });
            }
            ClientMsg::Touch { phase, x, y, .. } => {
                point(x, y)?;
                // Without a finger down (it was refused as busy, or timed out) there is nothing to move.
                if let Some(g) = self.gesture.as_mut() {
                    hub.send(ScreenCommand::Touch { phase, x, y, edge: None });
                    (g.x, g.y, g.at) = (x, y, Instant::now());
                    if phase == TouchPhase::Up {
                        self.gesture = None;
                    }
                }
            }
            ClientMsg::Key { usage, mods } => {
                if mods.len() > 4 || mods.iter().any(|m| !(224..=231).contains(m)) {
                    return Err(ViewError::Bad("mods must be up to 4 modifier usages (224-231)".into()));
                }
                let _held = hold(&self.rt, &hub).await?;
                press(&hub, usage, &mods);
            }
            ClientMsg::Button { name } => {
                let _held = hold(&self.rt, &hub).await?;
                hub.send(ScreenCommand::Button { button: name, down: true });
                tokio::time::sleep(BUTTON_HOLD).await;
                hub.send(ScreenCommand::Button { button: name, down: false });
            }
            ClientMsg::Type { id, text } | ClientMsg::PasteAndType { id, text } => {
                let _held = hold(&self.rt, &hub).await?;
                hub.sync().await;
                self.typed(&text).await?;
                return Ok(Some(done(id, None)));
            }
            ClientMsg::Paste { id, text } => {
                let _held = hold(&self.rt, &hub).await?;
                hub.sync().await;
                let via = self.pasted(&text).await?;
                return Ok(Some(done(id, Some(via))));
            }
            ClientMsg::Ack { .. } | ClientMsg::Ping { .. } | ClientMsg::Preset { .. } => {}
        }
        Ok(None)
    }

    /// Writes `text` to a daemon-owned scratch file (never a command line) and runs `f` on it.
    async fn with_text_file<T>(
        &self,
        text: &str,
        f: impl AsyncFnOnce(std::path::PathBuf) -> Result<T, ViewError>,
    ) -> Result<T, ViewError> {
        if text.is_empty() || text.len() > MAX_TEXT {
            return Err(ViewError::Bad(format!("text must be 1-{MAX_TEXT} bytes")));
        }
        let file = self.pool.config.data_dir.join("run").join(format!("{}.txt", uuid::Uuid::new_v4().simple()));
        std::fs::write(&file, text).context("writing the text")?;
        let result = f(file.clone()).await;
        let _ = std::fs::remove_file(&file);
        result
    }

    /// AXe types the text: US keyboard characters only.
    async fn typed(&self, text: &str) -> Result<(), ViewError> {
        self.with_text_file(text, async |file| {
            Ok(self.pool.devices.ui(&self.ctx, &UiInput::Type { text_file: file }).await?)
        })
        .await
    }

    /// The text goes on the simulator clipboard (AXe) and Cmd+V pastes it into the focused field,
    /// so any character goes through. Without a clipboard, it is typed. Returns where it went.
    async fn pasted(&self, text: &str) -> Result<&'static str, ViewError> {
        self.with_text_file(text, async |file| {
            match self.pool.devices.device_op(&self.ctx, &DeviceOp::Pasteboard { text_file: file.clone() }).await {
                Ok(_) => {
                    press(&self.hub, KEY_V, &[CMD]);
                    Ok("pasted")
                }
                Err(e) => {
                    tracing::warn!(lease = %self.ctx.lease_id, error = %e, "clipboard unavailable, typing instead");
                    self.pool.devices.ui(&self.ctx, &UiInput::Type { text_file: file }).await?;
                    Ok("typed")
                }
            }
        })
        .await
    }
}

/// A key press with modifiers held: modifiers down in order, the key down and up, modifiers up
/// in reverse.
fn press(hub: &Hub, usage: u8, mods: &[u8]) {
    for m in mods {
        hub.send(ScreenCommand::Key { usage: *m, down: true });
    }
    hub.send(ScreenCommand::Key { usage, down: true });
    hub.send(ScreenCommand::Key { usage, down: false });
    for m in mods.iter().rev() {
        hub.send(ScreenCommand::Key { usage: *m, down: false });
    }
}

fn done(id: u32, via: Option<&str>) -> Message {
    let mut v = serde_json::json!({ "t": "done", "id": id });
    if let Some(via) = via {
        v["via"] = via.into();
    }
    text(v)
}

async fn run_input(mut input: Input, mut rx: mpsc::Receiver<ClientMsg>, out: mpsc::Sender<Message>) {
    loop {
        let idle_until = input.gesture.as_ref().map(|g| g.at + GESTURE_IDLE);
        let msg = match idle_until {
            Some(t) => tokio::select! {
                m = rx.recv() => m,
                _ = tokio::time::sleep_until(t.into()) => {
                    input.lift();
                    continue;
                }
            },
            None => rx.recv().await,
        };
        let Some(msg) = msg else { break };
        if let Some(reply) = input.apply(msg).await
            && out.send(reply).await.is_err()
        {
            break;
        }
    }
    input.lift();
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
    // `connect-src 'self'` does not cover WebSockets in every browser: name this host's ws URLs.
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .filter(|h| h.bytes().all(|b| b.is_ascii_alphanumeric() || b".-:[]".contains(&b)));
    let connect = host.map_or("'self'".to_string(), |h| format!("'self' wss://{h} ws://{h}"));
    let csp = format!(
        "default-src 'none'; img-src 'self' blob:; script-src 'self'; style-src 'self'; connect-src {connect}; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
    );
    let mut resp = axum::response::Html(PAGE).into_response();
    let h = resp.headers_mut();
    no_store(h);
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_str(&csp).expect("ascii"));
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

fn hub_of(pool: &Pool, lease_id: &str) -> Option<Arc<Hub>> {
    pool.viewers.hubs.lock().expect("hubs mutex").get(lease_id).cloned()
}

async fn info(
    State(pool): State<Arc<Pool>>,
    Path(sid): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ViewError> {
    let v = pool.viewing(&sid, &headers)?;
    let (_, rt) = pool.viewer_ctx(&v)?;
    let model = pool.config.ios.as_ref().and_then(|i| i.models.get(&v.lease.model));
    let hub = hub_of(&pool, &v.lease.id);
    // The viewers' own touches hold the op lock too; only someone else's hold is busy.
    let holding = hub.as_ref().map_or(0, |h| h.holding.load(Ordering::SeqCst));
    let busy = rt.op.try_lock().is_err() && holding == 0;
    let preset = hub.as_ref().map(|h| h.rate.lock().expect("rate mutex").preset);
    let opts = hub.as_ref().map(|h| h.opts());
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
        "viewers": hub.as_ref().map_or(0, |h| h.viewers.load(Ordering::SeqCst)),
        "app": rt.app.lock().expect("app mutex").clone(),
        "preset": preset,
        "stream_opts": opts,
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
    let hub = hub_of(&pool, &v.lease.id).ok_or_else(|| ViewError::Gone("No active stream.".into()))?;
    let st = &hub.stats;
    let count = st.frame_count.load(Ordering::Relaxed);
    let bytes = st.frame_bytes.load(Ordering::Relaxed);
    let elapsed_s = (now_ms() - st.started_at_ms).max(1) as f64 / 1000.0;
    let preset = hub.rate.lock().expect("rate mutex").preset;
    let mut resp = Json(serde_json::json!({
        "stream": {
            "started_at_ms": st.started_at_ms,
            "fps": (count as f64 / elapsed_s * 10.0).round() / 10.0,
            "kbps": (bytes as f64 * 8.0 / elapsed_s / 1000.0).round(),
            "mean_frame_bytes": bytes.checked_div(count).unwrap_or(0),
            "total_frames": count,
            "total_bytes": bytes,
            "viewers": hub.viewers.load(Ordering::SeqCst),
            "last_frame_at_ms": st.last_frame_at_ms.load(Ordering::Relaxed),
        },
        "preset": preset,
        "opts": hub.opts(),
    }))
    .into_response();
    no_store(resp.headers_mut());
    Ok(resp)
}

/// `GET /view/s/{sid}/ws`: the stream (H.264 frames out) and the input (touches, keys, text in).
async fn ws(
    State(pool): State<Arc<Pool>>,
    Path(sid): Path<String>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Result<Response, ViewError> {
    if !same_origin(&headers) {
        return Err(ViewError::CrossOrigin);
    }
    let secret = cookie(&headers).ok_or(ViewError::Unauthorized)?.to_string();
    let v = pool.check_session(&sid, &secret)?;
    let (ctx, rt) = pool.viewer_ctx(&v)?;
    let (joined, video) = join(&pool, &ctx, &rt)?;
    let live = Live { pool: pool.clone(), sid, secret, control: v.control, ctx, rt, joined };
    let mut resp = upgrade.max_message_size(MAX_CLIENT_MESSAGE).on_upgrade(move |socket| session(live, video, socket));
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
        .route("/view/s/{sid}/ws", get(ws))
        .route("/view/s/{sid}/screenshot.jpg", get(screenshot))
}
