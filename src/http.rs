//! HTTP surface: `/mcp` (Streamable HTTP MCP with SSE responses and 15 s keepalives),
//! `POST /v1/apps` (build upload), `DELETE /v1/apps/{app_id}`, run evidence downloads under
//! `/v1/runs/{run_id}/`, admin endpoints under `/v1/admin/`, and `/healthz`.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json, Router,
    body::Body,
    extract::{Extension, Path as UrlPath, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use futures_util::StreamExt;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
};
use tokio::io::AsyncWriteExt;
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;

use crate::mcp::{AuthedUser, SeijiMcp};
use crate::pool::{AdminLimitsOutcome, Pool, sanitize_filename};
use crate::store::{App, User, month_window, now};

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers.get("authorization")?.to_str().ok()?.strip_prefix("Bearer ")
}

async fn auth(State(pool): State<Arc<Pool>>, mut req: Request, next: Next) -> Response {
    let user = match bearer(req.headers()).map(|t| pool.store.user_by_token(t)) {
        Some(Ok(Some(user))) if !user.disabled => user,
        Some(Err(e)) => {
            tracing::error!("token lookup: {e:#}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        _ => return (StatusCode::UNAUTHORIZED, "missing, invalid or disabled bearer token").into_response(),
    };
    req.extensions_mut().insert(AuthedUser(user));
    next.run(req).await
}

fn bad(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": msg.into() }))).into_response()
}

/// Streams the request body to `<dataDir>/apps/<user id>/<app_id>/<filename>`.
async fn upload(State(pool): State<Arc<Pool>>, req: Request) -> Response {
    let Some(AuthedUser(user)) = req.extensions().get::<AuthedUser>().cloned() else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Some(filename) = req.headers().get("x-filename").and_then(|v| v.to_str().ok()).map(sanitize_filename) else {
        return bad(StatusCode::BAD_REQUEST, "X-Filename header is required (e.g. MyApp.zip or app-debug.apk)");
    };
    if let Err(e) = pool.check_build_room(&user) {
        return bad(StatusCode::TOO_MANY_REQUESTS, format!("{e:#}"));
    }
    match store_upload(&pool, &user, &filename, req.into_body()).await {
        Ok(app) => {
            Json(serde_json::json!({ "app_id": app.id, "filename": app.filename, "size": app.size })).into_response()
        }
        Err((status, msg)) => bad(status, msg),
    }
}

async fn store_upload(pool: &Pool, user: &User, filename: &str, body: Body) -> Result<App, (StatusCode, String)> {
    let internal = |e: std::io::Error| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    let id = format!("a_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
    let dir = pool.apps_dir(user.id, &id);
    tokio::fs::create_dir_all(&dir).await.map_err(internal)?;
    let path = dir.join(filename);
    let limit = pool.config.max_upload_bytes;
    let written: Result<u64, (StatusCode, String)> = async {
        let mut file = tokio::fs::File::create(&path).await.map_err(internal)?;
        let mut size: u64 = 0;
        let mut stream = body.into_data_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| (StatusCode::BAD_REQUEST, format!("upload failed: {e}")))?;
            size += chunk.len() as u64;
            if size > limit {
                return Err((StatusCode::PAYLOAD_TOO_LARGE, format!("upload exceeds maxUploadBytes ({limit} bytes)")));
            }
            file.write_all(&chunk).await.map_err(internal)?;
        }
        file.flush().await.map_err(internal)?;
        if size == 0 {
            return Err((StatusCode::BAD_REQUEST, "empty upload".into()));
        }
        Ok(size)
    }
    .await;
    let size = match written {
        Ok(size) => size,
        Err(e) => {
            let _ = tokio::fs::remove_dir_all(&dir).await;
            return Err(e);
        }
    };
    let app = App {
        id,
        user_id: user.id,
        filename: filename.to_string(),
        path: path.to_string_lossy().into_owned(),
        size: size as i64,
    };
    pool.store.insert_app(&app).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok(app)
}

/// `DELETE /v1/apps/{app_id}`: deletes one of the caller's builds and frees its place.
async fn delete_app(
    State(pool): State<Arc<Pool>>,
    Extension(AuthedUser(user)): Extension<AuthedUser>,
    UrlPath(app_id): UrlPath<String>,
) -> Response {
    match pool.delete_build(&user, &app_id) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => bad(StatusCode::NOT_FOUND, format!("no uploaded app {app_id:?}")),
        Err(e) => bad(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

/// Streams a daemon-owned file as the response body.
async fn file_response(path: &std::path::Path, content_type: &str, download_name: Option<&str>) -> Response {
    let file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(e) => return bad(StatusCode::INTERNAL_SERVER_ERROR, format!("opening the file: {e}")),
    };
    let len = file.metadata().await.map(|m| m.len()).ok();
    let mut resp = Body::from_stream(tokio_util::io::ReaderStream::new(file)).into_response();
    let h = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(content_type) {
        h.insert(header::CONTENT_TYPE, v);
    }
    if let Some(len) = len {
        h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    if let Some(name) = download_name
        && let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{name}\""))
    {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    resp
}

/// `GET /v1/runs/{run_id}/files/{*path}`: one file the run's manifest lists (or `manifest.json`).
async fn run_file(
    State(pool): State<Arc<Pool>>,
    Extension(AuthedUser(user)): Extension<AuthedUser>,
    UrlPath((run_id, rel)): UrlPath<(String, String)>,
) -> Response {
    match pool.run_file(&user, &run_id, &rel) {
        Ok((path, _)) => {
            let name = rel.rsplit('/').next().unwrap_or("file");
            let kind = crate::evidence::content_type(&rel);
            let attach = !(kind.starts_with("image/") || kind.starts_with("text/") || kind == "application/json");
            file_response(&path, kind, attach.then_some(name)).await
        }
        Err(e) => bad(StatusCode::NOT_FOUND, format!("{e:#}")),
    }
}

/// `GET /v1/runs/{run_id}/evidence.zip`: the whole run, manifest included.
async fn evidence_zip(
    State(pool): State<Arc<Pool>>,
    Extension(AuthedUser(user)): Extension<AuthedUser>,
    UrlPath(run_id): UrlPath<String>,
) -> Response {
    let dir = match pool.run_manifest(&user, &run_id) {
        Ok((_, dir)) => dir,
        Err(e) => return bad(StatusCode::NOT_FOUND, format!("{e:#}")),
    };
    let zip = pool.config.data_dir.join("run").join(format!("{}.zip", uuid::Uuid::new_v4().simple()));
    if let Err(e) = crate::evidence::zip(&dir, &zip).await {
        let _ = std::fs::remove_file(&zip);
        return bad(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
    }
    let resp = file_response(&zip, "application/zip", Some(&format!("{run_id}-evidence.zip"))).await;
    // The open file keeps streaming after the name is gone.
    let _ = std::fs::remove_file(&zip);
    resp
}

/// `GET /v1/runs/{run_id}/report.md`: the same Markdown as `export_evidence`.
async fn run_report(
    State(pool): State<Arc<Pool>>,
    Extension(AuthedUser(user)): Extension<AuthedUser>,
    UrlPath(run_id): UrlPath<String>,
) -> Response {
    let report = (|| {
        let (manifest, dir) = pool.run_manifest(&user, &run_id)?;
        let (_, sha) = crate::evidence::sha256_file(&dir.join(crate::evidence::MANIFEST))?;
        anyhow::Ok(crate::evidence::markdown(&manifest, &pool.run_url(&run_id), &sha))
    })();
    match report {
        Ok(md) => (
            [
                (header::CONTENT_TYPE, "text/markdown; charset=utf-8"),
                (header::CACHE_CONTROL, "private, no-store"),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            ],
            md,
        )
            .into_response(),
        Err(e) => bad(StatusCode::NOT_FOUND, format!("{e:#}")),
    }
}

/// `GET /v1/me`: who the token belongs to, for clients that sign in (`seiji login`, Tern), and
/// the server's build, which `seiji update` brings the clients to.
async fn me(State(pool): State<Arc<Pool>>, Extension(AuthedUser(user)): Extension<AuthedUser>) -> Response {
    let is_admin = pool.config.admin_users.contains(&user.name);
    let server = serde_json::json!({ "version": env!("CARGO_PKG_VERSION"), "commit": crate::COMMIT });
    Json(serde_json::json!({ "user": user.name, "quota": user.quota, "is_admin": is_admin, "server": server }))
        .into_response()
}

/// A user entry for the admin endpoints.
#[derive(serde::Serialize)]
struct AdminUserEntry {
    id: i64,
    name: String,
    disabled: bool,
    is_admin: bool,
    monthly_minutes: i64,
    monthly_unlimited: bool,
    max_concurrent: i64,
    max_lease_minutes: i64,
    idle_minutes: i64,
    used_minutes: f64,
    active_devices: usize,
}

fn admin_entry(user: &User, is_admin: bool, used_seconds: i64, active_devices: usize) -> AdminUserEntry {
    AdminUserEntry {
        id: user.id,
        name: user.name.clone(),
        disabled: user.disabled,
        is_admin,
        monthly_minutes: user.quota.monthly_minutes,
        monthly_unlimited: user.quota.monthly_unlimited,
        max_concurrent: user.quota.max_concurrent,
        max_lease_minutes: user.quota.max_lease_minutes,
        idle_minutes: user.quota.idle_minutes,
        used_minutes: (used_seconds as f64 / 60.0 * 10.0).round() / 10.0,
        active_devices,
    }
}

/// `GET /v1/admin/users`: list all users with quotas, usage and active devices.
async fn admin_users(State(pool): State<Arc<Pool>>, Extension(AuthedUser(caller)): Extension<AuthedUser>) -> Response {
    if !pool.config.admin_users.contains(&caller.name) {
        return bad(StatusCode::FORBIDDEN, "admin access required");
    }
    let users = match pool.store.users() {
        Ok(u) => u,
        Err(e) => return bad(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    let active = match pool.store.active_leases() {
        Ok(a) => a,
        Err(e) => return bad(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    let t = now();
    let (from, to) = month_window(t);
    let mut entries = Vec::with_capacity(users.len());
    for u in &users {
        let used_secs = match pool.store.used_seconds(u.id, from, to, t) {
            Ok(s) => s,
            Err(e) => return bad(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
        };
        let active_devices = active.iter().filter(|l| l.user_id == u.id).count();
        entries.push(admin_entry(u, pool.config.admin_users.contains(&u.name), used_secs, active_devices));
    }
    let ios_in_use = active.iter().filter(|l| l.platform == crate::config::Platform::Ios).count();
    let android_in_use = active.iter().filter(|l| l.platform == crate::config::Platform::Android).count();
    Json(serde_json::json!({
        "users": entries,
        "capacity": { "ios": pool.config.capacity.ios, "android": pool.config.capacity.android },
        "in_use": { "ios": ios_in_use, "android": android_in_use },
    }))
    .into_response()
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AdminLimitsBody {
    monthly_minutes: i64,
    monthly_unlimited: bool,
    max_concurrent: i64,
}

/// `PUT /v1/admin/users/{id}/limits`: update a user's admin-editable limits.
async fn admin_set_limits(
    State(pool): State<Arc<Pool>>,
    Extension(AuthedUser(caller)): Extension<AuthedUser>,
    UrlPath(user_id): UrlPath<i64>,
    body: axum::body::Bytes,
) -> Response {
    if !pool.config.admin_users.contains(&caller.name) {
        return bad(StatusCode::FORBIDDEN, "admin access required");
    }
    let body: AdminLimitsBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => return bad(StatusCode::BAD_REQUEST, format!("invalid body: {e}")),
    };
    if body.monthly_minutes < 0 {
        return bad(StatusCode::BAD_REQUEST, "monthly_minutes must be >= 0");
    }
    if body.max_concurrent < 1 {
        return bad(StatusCode::BAD_REQUEST, "max_concurrent must be >= 1");
    }
    let outcome = match pool
        .set_admin_limits(caller.id, user_id, body.monthly_minutes, body.monthly_unlimited, body.max_concurrent)
        .await
    {
        Ok(o) => o,
        Err(e) => return bad(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    let user = match outcome {
        AdminLimitsOutcome::Ok(u) => u,
        AdminLimitsOutcome::CallerGone => return bad(StatusCode::UNAUTHORIZED, "caller no longer valid"),
        AdminLimitsOutcome::Forbidden => return bad(StatusCode::FORBIDDEN, "admin access required"),
        AdminLimitsOutcome::NotFound => return bad(StatusCode::NOT_FOUND, "user not found"),
    };
    let t = now();
    let (from, to) = month_window(t);
    let used_secs = match pool.store.used_seconds(user.id, from, to, t) {
        Ok(s) => s,
        Err(e) => return bad(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    let active_devices = match pool.store.active_leases() {
        Ok(a) => a.iter().filter(|l| l.user_id == user.id).count(),
        Err(e) => return bad(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    Json(admin_entry(&user, pool.config.admin_users.contains(&user.name), used_secs, active_devices)).into_response()
}

/// `control` from the optional `{"control": <bool>}` body of a viewer link request.
fn viewer_control(body: &[u8], default: bool) -> Result<bool, String> {
    if body.is_empty() {
        return Ok(default);
    }
    let req: crate::viewer::ViewerRequest = serde_json::from_slice(body).map_err(|e| format!("invalid body: {e}"))?;
    Ok(req.control.unwrap_or(default))
}

fn viewer_response(link: anyhow::Result<crate::viewer::ViewerLink>) -> Response {
    match link {
        Ok(link) => {
            let mut resp = Json(link).into_response();
            resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            resp
        }
        Err(e) => bad(StatusCode::NOT_FOUND, format!("{e:#}")),
    }
}

/// `POST /v1/leases/{lease_id}/viewer`: a one-time link to watch (and by default drive) the lease's
/// screen from a browser.
async fn viewer_link(
    State(pool): State<Arc<Pool>>,
    Extension(AuthedUser(user)): Extension<AuthedUser>,
    UrlPath(lease_id): UrlPath<String>,
    body: axum::body::Bytes,
) -> Response {
    match viewer_control(&body, true) {
        Ok(control) => viewer_response(pool.create_viewer(&user, Some(&lease_id), control)),
        Err(e) => bad(StatusCode::BAD_REQUEST, e),
    }
}

/// `GET /v1/admin/leases`: every active lease with its owner.
async fn admin_leases(State(pool): State<Arc<Pool>>, Extension(AuthedUser(caller)): Extension<AuthedUser>) -> Response {
    if !pool.config.admin_users.contains(&caller.name) {
        return bad(StatusCode::FORBIDDEN, "admin access required");
    }
    match pool.admin_leases() {
        Ok(leases) => Json(serde_json::json!({ "leases": leases })).into_response(),
        Err(e) => bad(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

/// `POST /v1/admin/leases/{lease_id}/viewer`: a one-time link to any user's lease, view-only by
/// default.
async fn admin_viewer_link(
    State(pool): State<Arc<Pool>>,
    Extension(AuthedUser(caller)): Extension<AuthedUser>,
    UrlPath(lease_id): UrlPath<String>,
    body: axum::body::Bytes,
) -> Response {
    if !pool.config.admin_users.contains(&caller.name) {
        return bad(StatusCode::FORBIDDEN, "admin access required");
    }
    match viewer_control(&body, false) {
        Ok(control) => viewer_response(pool.create_admin_viewer(&caller, &lease_id, control)),
        Err(e) => bad(StatusCode::BAD_REQUEST, e),
    }
}

/// The body of `PUT /v1/admin/viewer`: the settings to change.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ViewerSettingsBody {
    pace_max_ms: Option<u32>,
    webrtc: Option<bool>,
    trace: Option<bool>,
    fps: Option<u8>,
}

/// The viewer settings now, what `seiji.json` sets, why WebRTC cannot be turned on, if so, and
/// the size of the last viewer trace.
fn viewer_settings(pool: &Pool) -> Response {
    let v = &pool.config.viewer;
    Json(serde_json::json!({
        "settings": pool.viewers.settings(),
        "defaults": { "pace_max_ms": v.pace_max_ms, "webrtc": v.webrtc.enabled, "trace": false, "fps": v.fps },
        "webrtc_problem": v.webrtc.problem(),
        "trace_bytes": pool.viewers.trace_file().1,
    }))
    .into_response()
}

/// `GET /v1/admin/viewer`: the viewer settings an admin can change while Seiji runs.
async fn admin_viewer(State(pool): State<Arc<Pool>>, Extension(AuthedUser(caller)): Extension<AuthedUser>) -> Response {
    if !pool.config.admin_users.contains(&caller.name) {
        return bad(StatusCode::FORBIDDEN, "admin access required");
    }
    viewer_settings(&pool)
}

/// `PUT /v1/admin/viewer`: changes them until the next restart.
async fn admin_set_viewer(
    State(pool): State<Arc<Pool>>,
    Extension(AuthedUser(caller)): Extension<AuthedUser>,
    body: axum::body::Bytes,
) -> Response {
    if !pool.config.admin_users.contains(&caller.name) {
        return bad(StatusCode::FORBIDDEN, "admin access required");
    }
    let body: ViewerSettingsBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => return bad(StatusCode::BAD_REQUEST, format!("invalid body: {e}")),
    };
    // Validate everything first, then store only what the body names, so two admins who change
    // different settings at once both keep their change.
    if body.pace_max_ms.is_some_and(|ms| ms > 1000) {
        return bad(StatusCode::BAD_REQUEST, "pace_max_ms must be 0-1000");
    }
    if body.fps.is_some_and(|fps| !(1..=60).contains(&fps)) {
        return bad(StatusCode::BAD_REQUEST, "fps must be 1-60");
    }
    if body.webrtc == Some(true)
        && let Some(problem) = pool.config.viewer.webrtc.problem()
    {
        return bad(StatusCode::BAD_REQUEST, format!("WebRTC cannot be turned on: {problem}"));
    }
    // The one change that can fail goes first: a failed trace leaves the other settings as they were.
    if let Some(on) = body.trace
        && let Err(e) = pool.viewers.set_trace(on)
    {
        tracing::error!(error = %e, "cannot start the viewer trace");
        return bad(StatusCode::INTERNAL_SERVER_ERROR, format!("cannot start the viewer trace: {e}"));
    }
    pool.viewers.set_settings(body.pace_max_ms, body.webrtc, body.fps);
    let s = pool.viewers.settings();
    tracing::info!(admin = %caller.name, pace_max_ms = s.pace_max_ms, webrtc = s.webrtc, trace = s.trace, fps = s.fps, "viewer settings changed");
    viewer_settings(&pool)
}

/// `GET /v1/admin/viewer/trace`: the last viewer trace, as JSON lines.
async fn admin_viewer_trace(
    State(pool): State<Arc<Pool>>,
    Extension(AuthedUser(caller)): Extension<AuthedUser>,
) -> Response {
    if !pool.config.admin_users.contains(&caller.name) {
        return bad(StatusCode::FORBIDDEN, "admin access required");
    }
    let file = match tokio::fs::File::open(pool.viewers.trace_file().0).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return bad(StatusCode::NOT_FOUND, "no viewer trace yet: turn it on with PUT /v1/admin/viewer");
        }
        Err(e) => return bad(StatusCode::INTERNAL_SERVER_ERROR, format!("cannot read the viewer trace: {e}")),
    };
    let mut resp = axum::body::Body::from_stream(tokio_util::io::ReaderStream::new(file)).into_response();
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/x-ndjson"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::CONTENT_DISPOSITION, HeaderValue::from_static("attachment; filename=\"viewer-trace.jsonl\""));
    resp
}

/// `GET /v1/leases/{lease_id}/screenshot.jpg`: the screen now, as a JPEG.
async fn lease_screenshot(
    State(pool): State<Arc<Pool>>,
    Extension(AuthedUser(user)): Extension<AuthedUser>,
    UrlPath(lease_id): UrlPath<String>,
) -> Response {
    match pool.screenshot(&user, Some(&lease_id)).await {
        Ok(jpeg) => {
            let mut resp = jpeg.into_response();
            let h = resp.headers_mut();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            resp
        }
        Err(e) => bad(StatusCode::NOT_FOUND, format!("{e:#}")),
    }
}

/// `GET /v1/leases/{lease_id}/forward` (WebSocket): the server end of `seiji forward`.
async fn forward(
    State(pool): State<Arc<Pool>>,
    Extension(AuthedUser(user)): Extension<AuthedUser>,
    UrlPath(lease_id): UrlPath<String>,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> Response {
    use crate::forward::{ForwardError, Wire};
    use axum::extract::ws::Message;
    let tunnel = match pool.open_forward(&user, &lease_id).await {
        Ok(t) => t,
        Err(ForwardError::Disabled) => return bad(StatusCode::FORBIDDEN, "forwarding is off on this server"),
        Err(ForwardError::NotFound(e)) => return bad(StatusCode::NOT_FOUND, format!("{e:#}")),
        Err(ForwardError::Full(n)) => {
            return bad(StatusCode::TOO_MANY_REQUESTS, format!("this lease already has {n} tunnels open"));
        }
        Err(ForwardError::Internal(e)) => return bad(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    ws.on_upgrade(move |socket| async move {
        let (mut sink, stream) = socket.split();
        let (wire, mut outgoing) = tokio::sync::mpsc::channel::<Wire>(64);
        let writer = tokio::spawn(async move {
            use futures_util::SinkExt;
            while let Some(w) = outgoing.recv().await {
                let msg = match w {
                    Wire::Text(t) => Message::Text(t.into()),
                    Wire::Binary(b) => Message::Binary(b.into()),
                    Wire::Closed => {
                        let _ = sink.send(Message::Close(None)).await;
                        break;
                    }
                };
                if sink.send(msg).await.is_err() {
                    break;
                }
            }
        });
        let incoming = stream.filter_map(|m| async move {
            match m {
                Ok(Message::Binary(b)) => Some(Wire::Binary(b.to_vec())),
                Ok(Message::Text(t)) => Some(Wire::Text(t.to_string())),
                Ok(Message::Close(_)) | Err(_) => Some(Wire::Closed),
                Ok(_) => None,
            }
        });
        crate::forward::serve(pool, tunnel, wire, Box::pin(incoming)).await;
        let _ = writer.await;
    })
}

pub fn router(pool: Arc<Pool>, shutdown: CancellationToken) -> Router {
    let cheat_sheet = Arc::new(OnceCell::new());
    let factory_pool = pool.clone();
    let mcp = StreamableHttpService::new(
        move || Ok(SeijiMcp::new(factory_pool.clone(), cheat_sheet.clone())),
        Arc::new(NeverSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_sse_keep_alive(Some(Duration::from_secs(15)))
            .with_legacy_session_mode(false)
            .with_json_response(false)
            .with_allowed_hosts(pool.config.listen.allowed_hosts.clone())
            .with_cancellation_token(shutdown),
    );
    // `store_upload` enforces `maxUploadBytes` while it streams the body.
    let protected = Router::new()
        .nest_service("/mcp", mcp)
        .route("/v1/apps", post(upload))
        .route("/v1/apps/{app_id}", delete(delete_app))
        .route("/v1/runs/{run_id}/files/{*path}", get(run_file))
        .route("/v1/runs/{run_id}/evidence.zip", get(evidence_zip))
        .route("/v1/runs/{run_id}/report.md", get(run_report))
        .route("/v1/me", get(me))
        .route("/v1/admin/users", get(admin_users))
        .route("/v1/admin/users/{id}/limits", put(admin_set_limits))
        .route("/v1/admin/leases", get(admin_leases))
        .route("/v1/admin/leases/{lease_id}/viewer", post(admin_viewer_link))
        .route("/v1/admin/viewer", get(admin_viewer).put(admin_set_viewer))
        .route("/v1/admin/viewer/trace", get(admin_viewer_trace))
        .route("/v1/leases/{lease_id}/viewer", post(viewer_link))
        .route("/v1/leases/{lease_id}/screenshot.jpg", get(lease_screenshot))
        .route("/v1/leases/{lease_id}/forward", get(forward))
        .layer(middleware::from_fn_with_state(pool.clone(), auth));
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(protected)
        .merge(crate::viewer::routes())
        .with_state(pool)
}
