//! Live screen viewer: watch and drive a leased simulator from a browser page (Tern's browser
//! block, or any browser).
//!
//! A user with a token asks for a viewer link (`POST /v1/leases/{id}/viewer` or the
//! `share_screen` tool). The link carries a one-time ticket that lives 60 s. Opening it trades the
//! ticket for a session: a random id in the path and a random secret in an `HttpOnly`,
//! `SameSite=Strict` cookie scoped to that path. Every request checks the session, its user and
//! that the lease is still the user's and ready. One AXe stream per lease fans out to all its
//! viewers and stops when the last one leaves or the lease ends.

use std::collections::HashMap;
use std::convert::Infallible;
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
use crate::devices::{DeviceCtx, StreamOpts, UiInput};
use crate::pool::{LeaseRuntime, Pool};
use crate::store::{Lease, LeaseStatus, User, now};

const TICKET_SECONDS: i64 = 60;
const COOKIE: &str = "seiji_view";
const BOUNDARY: &str = "seijiframe";
/// A watched lease counts as used: the stream touches it this often.
const TOUCH_EVERY: Duration = Duration::from_secs(60);
const MAX_TEXT: usize = 10_000;
/// How long input waits for the device before it answers busy.
const INPUT_WAIT: Duration = Duration::from_secs(1);

const PAGE: &str = include_str!("viewer/index.html");
const SCRIPT: &str = include_str!("viewer/viewer.js");
const STYLE: &str = include_str!("viewer/viewer.css");

struct Ticket {
    user_id: i64,
    lease_id: String,
    control: bool,
    expires: i64,
}

struct Session {
    secret_sha256: String,
    user_id: i64,
    lease_id: String,
    control: bool,
    expires: i64,
}

struct Hub {
    tx: watch::Sender<Option<Bytes>>,
    cancel: CancellationToken,
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
        if lease.platform != Platform::Ios {
            anyhow::bail!("the screen viewer supports iOS simulators only for now");
        }
        let ticket = random_hex(32);
        self.viewers.tickets.lock().expect("tickets mutex").insert(
            ticket.clone(),
            Ticket { user_id: user.id, lease_id: lease.id.clone(), control, expires: now() + TICKET_SECONDS },
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
                lease_id: t.lease_id,
                control: t.control,
                expires: now() + lifetime,
            },
        );
        Ok((sid, secret, lifetime))
    }

    fn viewing(&self, sid: &str, headers: &HeaderMap) -> Result<Viewing, ViewError> {
        let secret = cookie(headers).ok_or(ViewError::Unauthorized)?;
        let (user_id, lease_id, control) = {
            let sessions = self.viewers.sessions.lock().expect("sessions mutex");
            let s = sessions.get(sid).ok_or(ViewError::Unauthorized)?;
            if s.expires <= now() || s.secret_sha256 != sha256_hex(secret) {
                return Err(ViewError::Unauthorized);
            }
            (s.user_id, s.lease_id.clone(), s.control)
        };
        let user = self.store.user_by_id(user_id)?.filter(|u| !u.disabled).ok_or(ViewError::Unauthorized)?;
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
    async fn viewer_input(&self, v: &Viewing, req: InputRequest) -> Result<(), ViewError> {
        if !v.control {
            return Err(ViewError::ViewOnly);
        }
        let (ctx, rt) = self.viewer_ctx(v)?;
        // A run (or another viewer's input) holds the device: wait briefly, then give up rather
        // than interleave with a run's steps.
        let _op = tokio::time::timeout(INPUT_WAIT, rt.op.clone().lock_owned()).await.map_err(|_| ViewError::Busy)?;
        let mut scratch = None;
        let input = match req.input {
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
            ViewerInput::Type { text } => {
                if text.is_empty() || text.len() > MAX_TEXT {
                    return Err(ViewError::Bad(format!("text must be 1-{MAX_TEXT} bytes")));
                }
                let file = self.config.data_dir.join("run").join(format!("{}.txt", uuid::Uuid::new_v4().simple()));
                std::fs::write(&file, text).context("writing typed text")?;
                scratch = Some(file.clone());
                UiInput::Type { text_file: file }
            }
            ViewerInput::Key { code } => UiInput::Key(code),
            ViewerInput::Button { name } => UiInput::Button(name),
        };
        let sent = self.devices.ui(&ctx, &input).await;
        if let Some(f) = scratch {
            let _ = std::fs::remove_file(f);
        }
        sent?;
        self.store.touch_lease(&v.lease.id, now())?;
        Ok(())
    }
}

// ---- stream ----

/// Subscribes to the lease's frames, starting its stream when it is the first viewer.
fn subscribe(pool: &Arc<Pool>, v: &Viewing) -> Result<watch::Receiver<Option<Bytes>>, ViewError> {
    let (ctx, rt) = pool.viewer_ctx(v)?;
    let max = pool.config.viewer.max_viewers_per_lease;
    let mut hubs = pool.viewers.hubs.lock().expect("hubs mutex");
    if let Some(hub) = hubs.get(&v.lease.id)
        && !hub.cancel.is_cancelled()
    {
        if hub.tx.receiver_count() >= max {
            return Err(ViewError::Full(max));
        }
        let mut rx = hub.tx.subscribe();
        rx.mark_changed();
        return Ok(rx);
    }
    let (tx, rx) = watch::channel(None);
    let hub = Arc::new(Hub { tx, cancel: rt.cancel.child_token() });
    hubs.insert(v.lease.id.clone(), hub.clone());
    drop(hubs);

    // Ends the stream when the last viewer leaves; keeps a watched lease from idling out.
    let watcher = hub.clone();
    let (store, lease_id) = (pool.store.clone(), v.lease.id.clone());
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = watcher.tx.closed() => break,
                _ = watcher.cancel.cancelled() => break,
                _ = tokio::time::sleep(TOUCH_EVERY) => {
                    let _ = store.touch_lease(&lease_id, now());
                }
            }
        }
        watcher.cancel.cancel();
    });

    let pool = pool.clone();
    let lease_id = v.lease.id.clone();
    tokio::spawn(async move {
        let cfg = &pool.config.viewer;
        let opts = StreamOpts { fps: cfg.fps, scale: cfg.scale, quality: cfg.quality };
        while !hub.cancel.is_cancelled() {
            if let Err(e) = pool.devices.stream(&ctx, &opts, &hub.tx, &hub.cancel).await {
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

fn part(frame: &[u8]) -> Bytes {
    let mut out = Vec::with_capacity(frame.len() + 96);
    out.extend_from_slice(
        format!("--{BOUNDARY}\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n", frame.len()).as_bytes(),
    );
    out.extend_from_slice(frame);
    out.extend_from_slice(b"\r\n");
    Bytes::from(out)
}

// ---- routes ----

/// `POST /v1/leases/{lease_id}/viewer` (bearer): `{"control": true}` by default.
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
    let viewers = pool.viewers.hubs.lock().expect("hubs mutex").get(&v.lease.id).map_or(0, |h| h.tx.receiver_count());
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
        "fps": pool.config.viewer.fps,
        "app": rt.app.lock().expect("app mutex").clone(),
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
    let rx = subscribe(&pool, &v)?;
    let frames = futures_util::stream::unfold(rx, |mut rx| async move {
        loop {
            // Err: the stream ended (the lease went away or the last viewer left).
            rx.changed().await.ok()?;
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
    pool.viewer_input(&v, req).await?;
    let mut resp = StatusCode::NO_CONTENT.into_response();
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
        .route("/view/s/{sid}/stream", get(stream))
        .route("/view/s/{sid}/input", post(input))
        .route("/view/s/{sid}/screenshot.jpg", get(screenshot))
}
