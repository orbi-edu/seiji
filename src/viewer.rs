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
//! connection; it stops when the last viewer leaves or the lease ends. With `viewer.webrtc` on, a
//! page can take the stream and send its input over WebRTC instead (`rtc`), and keeps the
//! WebSocket for the rest.

mod pipeline;
mod rtc;

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering};
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
/// A page that reads nothing for this long is gone: its socket closes.
const SEND_TIMEOUT: Duration = Duration::from_secs(5);
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
/// Format and frames a slow viewer may fall behind before it resyncs at a keyframe. With
/// helper messages of at most 4 MiB, one hub holds at most 128 MiB of video.
const VIDEO_BUFFER: usize = 32;
/// Adaptive bitrate: a congested viewer cuts the bitrate to 70 %, at most every 2 s and never
/// below 20 % of the preset; after 4 s without congestion it grows 15 % every 2 s.
const RATE_DOWN: f64 = 0.7;
const RATE_UP: f64 = 1.15;
const RATE_FLOOR: f64 = 0.2;
const RATE_STEP: Duration = Duration::from_secs(2);
const RATE_CALM: Duration = Duration::from_secs(4);
/// A WebRTC estimate never takes the stream below this many bits per second.
const CAP_FLOOR: u32 = 150_000;
/// A rising WebRTC estimate raises the stream at most this often.
const CAP_STEP: Duration = Duration::from_secs(1);
/// WebSocket close codes the page acts on: open a new link, or the device is gone.
const CLOSE_EXPIRED: u16 = 4401;
const CLOSE_GONE: u16 = 4410;
const CLOSE_INTERNAL: u16 = 1011;
/// The binary video message: flags, frame number, the capture, encode and server-receipt times,
/// then the AVCC sample.
const FRAME_HEADER: usize = 29;

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
    /// Unix microseconds when the daemon read the frame from the helper.
    received_us: i64,
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
    /// The bandwidth estimate of each WebRTC viewer: the stream never goes above the lowest.
    caps: HashMap<u64, u32>,
    /// What the helper was last told, and when a cap last changed it.
    sent: u32,
    capped_at: Instant,
}

impl Rate {
    fn new(preset: StreamPreset, target: StreamOpts, caps: HashMap<u64, u32>) -> Self {
        let now = Instant::now();
        Self {
            preset,
            target,
            bitrate: target.bitrate,
            // The first congestion may cut the bitrate at once.
            changed_at: now.checked_sub(RATE_STEP).unwrap_or(now),
            congested_at: None,
            caps,
            sent: target.bitrate,
            capped_at: now,
        }
    }

    /// The adapted bitrate, held under the lowest WebRTC estimate.
    fn effective(&self) -> u32 {
        let cap = self.caps.values().min().map_or(u32::MAX, |c| (*c).max(CAP_FLOOR));
        self.bitrate.min(cap)
    }

    /// The options to send to the helper, noted as sent.
    fn send(&mut self) -> StreamOpts {
        self.sent = self.effective();
        StreamOpts { bitrate: self.sent, ..self.target }
    }
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
    /// Names each WebRTC viewer's bandwidth cap.
    next_cap: AtomicU64,
    rate: StdMutex<Rate>,
    keyframe_at: StdMutex<Option<Instant>>,
    /// A keyframe was asked for inside `KEYFRAME_GAP`, or the helper was not there to take it.
    keyframe_pending: AtomicBool,
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
            next_cap: AtomicU64::new(0),
            rate: StdMutex::new(Rate::new(StreamPreset::Balanced, target, HashMap::new())),
            keyframe_at: StdMutex::new(None),
            keyframe_pending: AtomicBool::new(false),
            stats: StreamStats {
                started_at_ms: now_ms(),
                frame_count: AtomicU64::new(0),
                frame_bytes: AtomicU64::new(0),
                last_frame_at_ms: AtomicI64::new(0),
            },
        }
    }

    /// Hands a command to the helper; `false` when no helper runs (it is restarting) or its
    /// queue is full.
    fn send(&self, cmd: ScreenCommand) -> bool {
        match &*self.commands.lock().expect("commands mutex") {
            Some(tx) => tx.try_send(cmd).is_ok(),
            None => false,
        }
    }

    /// Hands every command to the helper, or none of them (a key press must not arrive in part).
    fn send_all(&self, cmds: Vec<ScreenCommand>) -> bool {
        let guard = self.commands.lock().expect("commands mutex");
        let Some(tx) = &*guard else { return false };
        if tx.capacity() < cmds.len() {
            return false;
        }
        cmds.into_iter().all(|c| tx.try_send(c).is_ok())
    }

    /// Waits until the helper has every command sent so far (at most `INPUT_WAIT`).
    async fn sync(&self) -> Result<(), ViewError> {
        let (done, wait) = tokio::sync::oneshot::channel();
        if self.send(ScreenCommand::Sync(done)) && matches!(tokio::time::timeout(INPUT_WAIT, wait).await, Ok(Ok(()))) {
            return Ok(());
        }
        Err(helper_missing())
    }

    fn opts(&self) -> StreamOpts {
        let rate = self.rate.lock().expect("rate mutex");
        StreamOpts { bitrate: rate.effective(), ..rate.target }
    }

    /// A WebRTC viewer's bandwidth estimate (`None` when it leaves). The stream follows a falling
    /// estimate at once and a rising one at most every `CAP_STEP`, by 10 % or more, so the
    /// sender's pacer never holds frames back.
    fn cap(&self, id: u64, estimate: Option<u32>) {
        let opts = {
            let mut rate = self.rate.lock().expect("rate mutex");
            match estimate {
                Some(e) => rate.caps.insert(id, e),
                None => rate.caps.remove(&id),
            };
            let next = rate.effective();
            let change = (f64::from(next) - f64::from(rate.sent)).abs() / f64::from(rate.sent.max(1));
            if change < 0.1 || (next > rate.sent && rate.capped_at.elapsed() < CAP_STEP) {
                return;
            }
            rate.capped_at = Instant::now();
            rate.send()
        };
        self.send(ScreenCommand::Encode(opts));
    }

    /// Asks for a keyframe. A request within `KEYFRAME_GAP` of the last one stays pending and
    /// goes out when the gap has passed (`flush_keyframe`), so a still screen still gets it.
    fn keyframe(&self, force: bool) {
        let mut at = self.keyframe_at.lock().expect("keyframe mutex");
        if force || at.is_none_or(|t| t.elapsed() >= KEYFRAME_GAP) {
            *at = Some(Instant::now());
            self.keyframe_pending.store(!self.send(ScreenCommand::Keyframe), Ordering::SeqCst);
        } else {
            self.keyframe_pending.store(true, Ordering::SeqCst);
        }
    }

    fn flush_keyframe(&self) {
        if self.keyframe_pending.load(Ordering::SeqCst) {
            self.keyframe(false);
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
            rate.send()
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
            rate.send()
        };
        self.send(ScreenCommand::Encode(opts));
    }

    fn set_preset(&self, preset: StreamPreset, cfg: &crate::config::Viewer) -> StreamOpts {
        let opts = {
            let mut rate = self.rate.lock().expect("rate mutex");
            let caps = std::mem::take(&mut rate.caps);
            *rate = Rate::new(preset, preset.opts(cfg), caps);
            rate.send()
        };
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
                let _ = self.video.send(Video::Frame(Arc::new(frame_message(seq, &f, now_us()))));
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

/// Flags (bit 0: keyframe), frame number, then the capture, encode and receipt times in Unix
/// microseconds (the encode time is the capture time for an older helper), then the sample.
fn frame_message(seq: u32, f: &VideoFrame, received_us: i64) -> Encoded {
    let mut out = Vec::with_capacity(FRAME_HEADER + f.data.len());
    out.push(u8::from(f.key));
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(&f.captured_us.to_be_bytes());
    out.extend_from_slice(&f.encoded_us.unwrap_or(f.captured_us).to_be_bytes());
    out.extend_from_slice(&received_us.to_be_bytes());
    out.extend_from_slice(&f.data);
    Encoded { seq, key: f.key, received_us, message: Bytes::from(out) }
}

/// Tickets, sessions, the one helper per watched lease, and the settings an admin can change
/// while Seiji runs. In memory: a restart ends every lease, and with it every viewer, and sets
/// the settings back to `seiji.json`.
pub struct Viewers {
    tickets: StdMutex<HashMap<String, Ticket>>,
    sessions: StdMutex<HashMap<String, Session>>,
    hubs: StdMutex<HashMap<String, Arc<Hub>>>,
    /// `viewer.paceMaxMs`, as an admin last set it.
    pace_max_ms: AtomicU32,
    /// `viewer.webrtc.enabled`, as an admin last set it.
    webrtc: AtomicBool,
}

/// The viewer settings an admin can change while Seiji runs (`/v1/admin/viewer`).
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ViewerSettings {
    pub pace_max_ms: u32,
    pub webrtc: bool,
}

impl Viewers {
    pub fn new(cfg: &crate::config::Viewer) -> Self {
        Self {
            tickets: StdMutex::default(),
            sessions: StdMutex::default(),
            hubs: StdMutex::default(),
            pace_max_ms: AtomicU32::new(cfg.pace_max_ms),
            webrtc: AtomicBool::new(cfg.webrtc.enabled),
        }
    }

    /// Drops expired tickets and sessions (the reaper calls it).
    pub fn prune(&self, at: i64) {
        self.tickets.lock().expect("tickets mutex").retain(|_, t| t.expires > at);
        self.sessions.lock().expect("sessions mutex").retain(|_, s| s.expires > at);
    }

    pub fn settings(&self) -> ViewerSettings {
        ViewerSettings {
            pace_max_ms: self.pace_max_ms.load(Ordering::Relaxed),
            webrtc: self.webrtc.load(Ordering::Relaxed),
        }
    }

    /// Stores the settings given and leaves the others. Applies to new gestures and new pages at
    /// once; a page already on WebRTC stays on it.
    pub fn set_settings(&self, pace_max_ms: Option<u32>, webrtc: Option<bool>) {
        if let Some(ms) = pace_max_ms {
            self.pace_max_ms.store(ms, Ordering::Relaxed);
        }
        if let Some(on) = webrtc {
            self.webrtc.store(on, Ordering::Relaxed);
        }
    }

    fn pace_max(&self) -> Duration {
        Duration::from_millis(u64::from(self.pace_max_ms.load(Ordering::Relaxed)))
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

fn now_us() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_micros() as i64
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
    /// WebRTC is off, or no port is free: the page stays on the WebSocket.
    Rtc(String),
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
            ViewError::Rtc(why) => (StatusCode::SERVICE_UNAVAILABLE, "rtc_unavailable", why.clone()),
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
            let mut keyframes = tokio::time::interval(KEYFRAME_GAP / 2);
            touch.tick().await;
            log.tick().await;
            loop {
                tokio::select! {
                    _ = hub.cancel.cancelled() => break,
                    _ = touch.tick() => { let _ = store.touch_lease(&lease_id, now()); }
                    _ = rate.tick() => hub.recover(),
                    _ = keyframes.tick() => hub.flush_keyframe(),
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
    /// The page's decoder dropped frames (it fell behind, or was reset): start over at a keyframe.
    Keyframe,
    Preset {
        name: StreamPreset,
    },
    Touch {
        phase: TouchPhase,
        x: f64,
        y: f64,
        #[serde(default)]
        edge: Option<Edge>,
        /// When the page saw it, in milliseconds on the page's own clock (`performance.now()`).
        #[serde(default)]
        ts: Option<f64>,
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
    /// The page's WebRTC offer (SDP); answered with `rtc_answer`.
    RtcOffer {
        sdp: String,
    },
    /// Where the page shows the stream from: `rtc` stops the frames on the WebSocket.
    VideoVia {
        via: Via,
    },
    /// The page's timings of the last frames, every 5 s; see `pipeline`.
    Telemetry {
        report: pipeline::PageReport,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Via {
    Ws,
    Rtc,
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

/// Sends one message; `false` when the socket is gone or the page has not read for
/// `SEND_TIMEOUT`, so a stalled page cannot hold off the session checks for long.
async fn put(sink: &mut futures_util::stream::SplitSink<WebSocket, Message>, m: Message) -> bool {
    matches!(tokio::time::timeout(SEND_TIMEOUT, sink.send(m)).await, Ok(Ok(())))
}

/// What the page's reader hands to the session loop, which owns the flow state.
enum Ctl {
    Ack(u32),
    Keyframe,
    /// The page shows the WebRTC stream (`true`) or the WebSocket's.
    Rtc(bool),
    /// The page's timings.
    Report(Box<pipeline::PageReport>),
}

/// An input message and when the daemon read it.
type Queued = (ClientMsg, Instant);

/// How often the page gets this side's timings, and the log gets a viewer's pipeline.
const PIPELINE_TO_PAGE: Duration = Duration::from_secs(5);
const PIPELINE_TO_LOG: Duration = Duration::from_secs(30);

async fn session(live: Live, mut video: broadcast::Receiver<Video>, socket: WebSocket) {
    let (mut sink, stream) = socket.split();
    let hub = live.joined.hub.clone();
    let (out_tx, mut out_rx) = mpsc::channel::<Message>(64);
    let (in_tx, in_rx) = mpsc::channel::<Queued>(1024);
    let pipe = Arc::new(StdMutex::new(pipeline::Pipeline::default()));
    let (ctl_tx, mut ctl_rx) = mpsc::channel::<Ctl>(256);
    let input = Input {
        pool: live.pool.clone(),
        ctx: live.ctx.clone(),
        rt: live.rt.clone(),
        hub: hub.clone(),
        gesture: None,
        touched: None,
        pipe: pipe.clone(),
    };
    let stop = CancellationToken::new();
    // Once the page is gone its queued input is dropped, even while a frame write still waits.
    let gone = stop.clone().drop_guard();
    let reader = tokio::spawn({
        let reading = read_page(stream, live.pool.clone(), hub.clone(), live.control, in_tx, out_tx.clone(), ctl_tx);
        async move {
            let _gone = gone;
            reading.await;
        }
    });
    let input = tokio::spawn(run_input(input, in_rx, out_tx, stop.clone()));
    let mut flow = Flow::new();
    // The page shows the WebRTC stream: the WebSocket carries no frames.
    let mut rtc_video = false;
    // The format may be in the cache and in the channel both: send each one once.
    let mut sent_format = hub.format.lock().expect("format mutex").clone();
    let mut close = None;
    if let Some(f) = &sent_format
        && !put(&mut sink, Message::Text(f.to_string().into())).await
    {
        close = Some(None);
    }
    let mut recheck = tokio::time::interval(RECHECK_EVERY);
    recheck.tick().await;
    let mut to_page = tokio::time::interval(PIPELINE_TO_PAGE);
    to_page.tick().await;
    let mut to_log = tokio::time::interval(PIPELINE_TO_LOG);
    to_log.tick().await;
    let viewer = &live.sid[..6];
    while close.is_none() {
        tokio::select! {
            v = video.recv() => match v {
                Ok(Video::Format(f)) => {
                    if !sent_format.as_ref().is_some_and(|s| Arc::ptr_eq(s, &f)) {
                        flow.need_key = true;
                        if !put(&mut sink, Message::Text(f.to_string().into())).await {
                            close = Some(None);
                        }
                        sent_format = Some(f);
                    }
                }
                Ok(Video::Frame(f)) => {
                    if !rtc_video && flow.admit(&f, &hub) {
                        if put(&mut sink, Message::Binary(f.message.clone())).await {
                            // In server: read from the helper to written to this socket.
                            let ms = (now_us() - f.received_us) as f64 / 1000.0;
                            pipe.lock().expect("pipeline mutex").server.push(ms);
                        } else {
                            close = Some(None);
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // A format may have been among the messages it missed.
                    let current = hub.format.lock().expect("format mutex").clone();
                    if let Some(f) = current
                        && !sent_format.as_ref().is_some_and(|s| Arc::ptr_eq(s, &f))
                    {
                        if !put(&mut sink, Message::Text(f.to_string().into())).await {
                            close = Some(None);
                        }
                        sent_format = Some(f);
                    }
                    flow.need_key = true;
                    hub.keyframe(false);
                }
                Err(broadcast::error::RecvError::Closed) => close = Some(Some(live.ended())),
            },
            c = ctl_rx.recv() => match c {
                Some(Ctl::Ack(seq)) => flow.ack(seq, &hub),
                Some(Ctl::Keyframe) => {
                    flow.need_key = true;
                    hub.keyframe(false);
                }
                Some(Ctl::Rtc(on)) => {
                    rtc_video = on;
                    if !on {
                        // Back on the WebSocket: start over at a keyframe.
                        flow = Flow::new();
                        hub.keyframe(false);
                    }
                }
                Some(Ctl::Report(r)) => pipe.lock().expect("pipeline mutex").page = Some(*r),
                // The page closed its socket.
                None => close = Some(None),
            },
            Some(m) = out_rx.recv() => {
                if !put(&mut sink, m).await {
                    close = Some(None);
                }
            }
            _ = recheck.tick() => {
                if let Err(e) = live.pool.check_session(&live.sid, &live.secret) {
                    close = Some(Some(e.close()));
                }
            }
            _ = to_page.tick() => {
                let m = text(pipe.lock().expect("pipeline mutex").message());
                if !put(&mut sink, m).await {
                    close = Some(None);
                }
            }
            _ = to_log.tick() => pipe.lock().expect("pipeline mutex").log(&live.ctx.lease_id, viewer),
            _ = hub.cancel.cancelled() => close = Some(Some(live.ended())),
        }
    }
    // Queued input is dropped: nothing more reaches the device once the page is gone or its
    // session is revoked. The input task still lifts a finger that is down.
    stop.cancel();
    reader.abort();
    let _ = reader.await;
    drop(out_rx);
    if let Some(Some(frame)) = close {
        let _ = put(&mut sink, Message::Close(Some(frame))).await;
    }
    let _ = input.await;
}

/// Reads the page on its own task, so input reaches the device while a video frame is still
/// being written to a slow link. Replies go through `out`; acks, keyframe requests and the
/// page's choice of video transport go to the session loop. The page's WebRTC session, if any,
/// lives here. Returns, closing `ctl`, when the page closes its socket.
async fn read_page(
    mut stream: futures_util::stream::SplitStream<WebSocket>,
    pool: Arc<Pool>,
    hub: Arc<Hub>,
    control: bool,
    input: mpsc::Sender<Queued>,
    out: mpsc::Sender<Message>,
    ctl: mpsc::Sender<Ctl>,
) {
    let mut rtc: Option<rtc::RtcSession> = None;
    while let Some(Ok(m)) = stream.next().await {
        let t = match m {
            Message::Text(t) => t,
            Message::Close(_) => return,
            _ => continue,
        };
        let reply = match serde_json::from_str::<ClientMsg>(&t) {
            Ok(ClientMsg::Ack { seq }) => {
                if ctl.send(Ctl::Ack(seq)).await.is_err() {
                    return;
                }
                None
            }
            Ok(ClientMsg::Keyframe) => {
                if ctl.send(Ctl::Keyframe).await.is_err() {
                    return;
                }
                None
            }
            Ok(ClientMsg::Ping { at }) => {
                Some(text(serde_json::json!({ "t": "pong", "at": at, "server_ms": now_ms() })))
            }
            Ok(
                ClientMsg::Preset { .. }
                | ClientMsg::Touch { .. }
                | ClientMsg::Key { .. }
                | ClientMsg::Button { .. }
                | ClientMsg::Type { .. }
                | ClientMsg::Paste { .. }
                | ClientMsg::PasteAndType { .. },
            ) if !control => view_only(&t),
            Ok(ClientMsg::Preset { name }) => {
                let opts = hub.set_preset(name, &pool.config.viewer);
                Some(text(serde_json::json!({ "t": "preset", "preset": name, "opts": opts })))
            }
            Ok(ClientMsg::RtcOffer { sdp }) => {
                let cfg = &pool.config.viewer.webrtc;
                if pool.viewers.settings().webrtc {
                    // A new offer replaces the old session and frees its port first.
                    if let Some(old) = rtc.take() {
                        old.close().await;
                    }
                    let peer = rtc::Peer { hub: hub.clone(), control, input: input.clone(), out: out.clone() };
                    match rtc::start(cfg, &sdp, peer).await {
                        Ok((answer, session)) => {
                            rtc = Some(session);
                            Some(text(serde_json::json!({ "t": "rtc_answer", "sdp": answer })))
                        }
                        Err(e) => Some(e.message(None)),
                    }
                } else {
                    Some(
                        ViewError::Rtc("WebRTC is off on this host: the video stays on the WebSocket.".into())
                            .message(None),
                    )
                }
            }
            Ok(ClientMsg::VideoVia { via }) => {
                if ctl.send(Ctl::Rtc(via == Via::Rtc && rtc.is_some())).await.is_err() {
                    return;
                }
                None
            }
            Ok(ClientMsg::Telemetry { report }) => {
                if ctl.send(Ctl::Report(Box::new(report.checked()))).await.is_err() {
                    return;
                }
                None
            }
            Ok(msg) => input.try_send((msg, Instant::now())).err().map(|_| ViewError::Busy.message(None)),
            Err(e) => Some(ViewError::Bad(format!("invalid message: {e}")).message(None)),
        };
        if let Some(r) = reply
            && out.send(r).await.is_err()
        {
            return;
        }
    }
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
    /// The page's time of one point of the gesture and when it goes to the helper.
    clock: Option<(f64, Instant)>,
}

impl Gesture {
    /// How long to hold a point so the helper gets the gesture with the page's own spacing,
    /// though the link delivered its points late or in a burst. A late point starts a new anchor;
    /// no point waits longer than `max` (`viewer.paceMaxMs`, or what an admin set).
    fn pace(&mut self, ts: Option<f64>, max: Duration) -> Option<Duration> {
        let ts = ts?;
        let now = Instant::now();
        let (c0, s0) = self.clock?;
        let target = s0 + Duration::from_secs_f64((ts - c0).max(0.0) / 1000.0);
        if target <= now {
            self.clock = Some((ts, now));
            return None;
        }
        let wait = (target - now).min(max);
        self.clock = Some((ts, now + wait));
        Some(wait)
    }
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
    /// Where "input in server" is recorded.
    pipe: Arc<StdMutex<pipeline::Pipeline>>,
}

impl Input {
    /// Lifts a finger that is still down, and frees the device once the helper has the lift, so
    /// a run that takes the device next never finds the finger down.
    async fn lift(&mut self) {
        if let Some(g) = self.gesture.take() {
            self.hub.send(ScreenCommand::Touch { phase: TouchPhase::Up, x: g.x, y: g.y, edge: None });
            let _ = self.hub.sync().await;
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
            self.lift().await;
        }
        let hub = self.hub.clone();
        match msg {
            ClientMsg::Touch { phase: TouchPhase::Down, x, y, edge, ts } => {
                point(x, y)?;
                let held = hold(&self.rt, &hub).await?;
                delivered(hub.send(ScreenCommand::Touch { phase: TouchPhase::Down, x, y, edge }))?;
                let clock = ts.map(|t| (t, Instant::now()));
                self.gesture = Some(Gesture { _held: held, x, y, at: Instant::now(), clock });
            }
            ClientMsg::Touch { phase, x, y, ts, .. } => {
                point(x, y)?;
                // Without a finger down (it was refused as busy, or timed out) there is nothing to move.
                if let Some(g) = self.gesture.as_mut() {
                    if let Some(wait) = g.pace(ts, self.pool.viewers.pace_max()) {
                        tokio::time::sleep(wait).await;
                    }
                    hub.send(ScreenCommand::Touch { phase, x, y, edge: None });
                    (g.x, g.y, g.at) = (x, y, Instant::now());
                    if phase == TouchPhase::Up {
                        // The device stays held until the helper has the lift.
                        let _ = hub.sync().await;
                        self.gesture = None;
                    }
                }
            }
            ClientMsg::Key { usage, mods } => {
                if mods.len() > 4 || mods.iter().any(|m| !(224..=231).contains(m)) {
                    return Err(ViewError::Bad("mods must be up to 4 modifier usages (224-231)".into()));
                }
                let _held = hold(&self.rt, &hub).await?;
                delivered(press(&hub, usage, &mods))?;
                hub.sync().await?;
            }
            ClientMsg::Button { name } => {
                let _held = hold(&self.rt, &hub).await?;
                delivered(hub.send(ScreenCommand::Button { button: name, down: true }))?;
                tokio::time::sleep(BUTTON_HOLD).await;
                hub.send(ScreenCommand::Button { button: name, down: false });
                hub.sync().await?;
            }
            ClientMsg::Type { id, text } | ClientMsg::PasteAndType { id, text } => {
                let _held = hold(&self.rt, &hub).await?;
                hub.sync().await?;
                self.typed(&text).await?;
                return Ok(Some(done(id, None)));
            }
            ClientMsg::Paste { id, text } => {
                let _held = hold(&self.rt, &hub).await?;
                hub.sync().await?;
                let via = self.pasted(&text).await?;
                return Ok(Some(done(id, Some(via))));
            }
            // The reader handles these; they never reach the input queue.
            ClientMsg::Ack { .. }
            | ClientMsg::Ping { .. }
            | ClientMsg::Keyframe
            | ClientMsg::Preset { .. }
            | ClientMsg::RtcOffer { .. }
            | ClientMsg::VideoVia { .. }
            | ClientMsg::Telemetry { .. } => {}
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
                    // The clipboard alone is not a paste: report it only once Cmd+V is with the helper.
                    delivered(press(&self.hub, KEY_V, &[CMD]))?;
                    self.hub.sync().await?;
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
/// in reverse. All of it goes to the helper, or none; `false` for none.
fn press(hub: &Hub, usage: u8, mods: &[u8]) -> bool {
    let mut keys: Vec<ScreenCommand> = mods.iter().map(|m| ScreenCommand::Key { usage: *m, down: true }).collect();
    keys.push(ScreenCommand::Key { usage, down: true });
    keys.push(ScreenCommand::Key { usage, down: false });
    keys.extend(mods.iter().rev().map(|m| ScreenCommand::Key { usage: *m, down: false }));
    hub.send_all(keys)
}

fn delivered(sent: bool) -> Result<(), ViewError> {
    if sent { Ok(()) } else { Err(helper_missing()) }
}

fn helper_missing() -> ViewError {
    ViewError::Internal(anyhow::anyhow!("the screen helper did not take the input; try again"))
}

fn done(id: u32, via: Option<&str>) -> Message {
    let mut v = serde_json::json!({ "t": "done", "id": id });
    if let Some(via) = via {
        v["via"] = via.into();
    }
    text(v)
}

/// Runs until the page goes away (`stop`): what is still queued then is dropped, not applied.
async fn run_input(
    mut input: Input,
    mut rx: mpsc::Receiver<Queued>,
    out: mpsc::Sender<Message>,
    stop: CancellationToken,
) {
    loop {
        let idle = input.gesture.as_ref().map(|g| tokio::time::Instant::from_std(g.at + GESTURE_IDLE));
        let msg = tokio::select! {
            biased;
            _ = stop.cancelled() => break,
            _ = async { tokio::time::sleep_until(idle.expect("checked")).await }, if idle.is_some() => {
                input.lift().await;
                continue;
            }
            m = rx.recv() => m,
        };
        let Some((msg, read_at)) = msg else { break };
        let reply = input.apply(msg).await;
        // Input in server: read from the page to applied (the helper has it, or AXe is done).
        input.pipe.lock().expect("pipeline mutex").input.push(read_at.elapsed().as_secs_f64() * 1000.0);
        if let Some(reply) = reply
            && out.send(reply).await.is_err()
        {
            break;
        }
    }
    input.lift().await;
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
        "webrtc": pool.viewers.settings().webrtc,
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
