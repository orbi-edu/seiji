//! HTTP surface: `/mcp` (Streamable HTTP MCP with SSE responses and 15 s keepalives),
//! `POST /v1/apps` (build upload), run evidence downloads under `/v1/runs/{run_id}/`,
//! and `/healthz`.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json, Router,
    body::Body,
    extract::{Extension, Path as UrlPath, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::StreamExt;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
};
use tokio::io::AsyncWriteExt;
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;

use crate::mcp::{AuthedUser, SeijiMcp};
use crate::pool::{Pool, sanitize_filename};
use crate::store::{App, User};

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

/// `GET /v1/me`: who the token belongs to, for clients that sign in (`seiji login`, Tern).
async fn me(Extension(AuthedUser(user)): Extension<AuthedUser>) -> Response {
    Json(serde_json::json!({ "user": user.name, "quota": user.quota })).into_response()
}

/// `POST /v1/leases/{lease_id}/viewer`: a one-time link to watch (and by default drive) the lease's
/// screen from a browser.
async fn viewer_link(
    State(pool): State<Arc<Pool>>,
    Extension(AuthedUser(user)): Extension<AuthedUser>,
    UrlPath(lease_id): UrlPath<String>,
    body: axum::body::Bytes,
) -> Response {
    let req: crate::viewer::ViewerRequest = if body.is_empty() {
        Default::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(r) => r,
            Err(e) => return bad(StatusCode::BAD_REQUEST, format!("invalid body: {e}")),
        }
    };
    match pool.create_viewer(&user, Some(&lease_id), req.control.unwrap_or(true)) {
        Ok(link) => {
            let mut resp = Json(link).into_response();
            resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            resp
        }
        Err(e) => bad(StatusCode::NOT_FOUND, format!("{e:#}")),
    }
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
        .route("/v1/runs/{run_id}/files/{*path}", get(run_file))
        .route("/v1/runs/{run_id}/evidence.zip", get(evidence_zip))
        .route("/v1/runs/{run_id}/report.md", get(run_report))
        .route("/v1/me", get(me))
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
