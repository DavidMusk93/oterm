use crate::assets::WebAssets;
use crate::auth::{AuthError, AuthManager, COOKIE_NAME};
use crate::config::ServeArgs;
use crate::protocol::{
    decode_client_frame, encode_json_frame, encode_output, encode_pong, encode_snapshot,
    AuthLockPayload, ClientFrame, ErrorPayload, OP_AUTH_LOCK, OP_ERROR, OP_SESSION,
};
use crate::ringbuf::ReplaySlice;
use crate::session::{SessionManager, TerminalSession};
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

const MAX_IN_FLIGHT_BYTES: u64 = 256 * 1024;
const WS_CLOSE_AUTH_EXPIRED: u16 = 4401;
/// Transport cookie lifetime; actual session idle expiry is strictly enforced in `AuthManager`.
const COOKIE_MAX_AGE_SECS: i64 = 30 * 24 * 60 * 60;

#[derive(Clone)]
pub struct AppState {
    pub auth: Arc<AuthManager>,
    pub sessions: Arc<SessionManager>,
    pub base_path: String,
    pub secure_cookie: bool,
}

pub fn build_router(state: AppState) -> Router {
    let inner = Router::new()
        .route("/", get(serve_index))
        .route("/api/health", get(health))
        .route("/api/auth/status", get(auth_status))
        .route("/api/auth/touch", post(auth_touch))
        .route("/api/auth/verify", post(auth_verify))
        .route("/api/auth/logout", post(auth_logout))
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route("/api/sessions/{id}", patch(rename_session).delete(delete_session))
        .route("/ws", get(ws_handler))
        .route("/{*file}", get(serve_static))
        .with_state(state.clone());

    if state.base_path.is_empty() {
        inner
    } else {
        let bp = state.base_path.clone();
        let bp_slash = format!("{bp}/");
        let bp_redir = bp_slash.clone();
        Router::new()
            .route(&bp, get(move || async move { Redirect::permanent(&bp_redir) }))
            .nest(&bp_slash, inner)
    }
}

pub async fn run_server(args: ServeArgs) -> anyhow::Result<()> {
    let auth = Arc::new(AuthManager::from_secret_file(
        &args.secret_path,
        &args.issuer,
        &args.account,
        args.auth_ttl,
    )?);

    let sessions = Arc::new(SessionManager::new(
        args.resolved_shell(),
        args.resolved_cwd(),
        args.ring_buffer_bytes,
        args.max_sessions,
    ));

    let base_path = args.normalized_base_path();
    let state = AppState {
        auth,
        sessions,
        base_path: base_path.clone(),
        secure_cookie: args.secure_cookie,
    };

    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind(&args.bind).await?;
    info!(
        "oterm listening on http://{}{} (auth_ttl={}s, ring_buf={}B)",
        args.bind,
        if base_path.is_empty() { "/" } else { &base_path },
        args.auth_ttl,
        args.ring_buffer_bytes
    );

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "ok": true, "service": "oterm" }))
}

async fn serve_index() -> Response {
    serve_embedded_file("index.html")
}

async fn serve_static(Path(file): Path<String>) -> Response {
    let clean = file.trim_start_matches('/');
    if clean.is_empty() {
        return serve_embedded_file("index.html");
    }
    serve_embedded_file(clean)
}

fn serve_embedded_file(path: &str) -> Response {
    match WebAssets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, mime.as_ref().to_string()),
                    (
                        header::CACHE_CONTROL,
                        if path == "index.html" {
                            "no-store".to_string()
                        } else {
                            "no-cache".to_string()
                        },
                    ),
                ],
                content.data.into_owned(),
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

fn extract_client_ip(headers: &HeaderMap, addr: SocketAddr) -> String {
    if let Some(cf) = headers.get("cf-connecting-ip").and_then(|v| v.to_str().ok()) {
        if !cf.trim().is_empty() {
            return cf.trim().to_string();
        }
    }
    if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(first) = xff.split(',').next() {
            if !first.trim().is_empty() {
                return first.trim().to_string();
            }
        }
    }
    addr.ip().to_string()
}

fn is_https_request(headers: &HeaderMap, force_secure: bool) -> bool {
    if force_secure {
        return true;
    }
    if let Some(proto) = headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()) {
        if proto.eq_ignore_ascii_case("https") {
            return true;
        }
    }
    if let Some(cf) = headers.get("cf-visitor").and_then(|v| v.to_str().ok()) {
        if cf.contains("https") {
            return true;
        }
    }
    false
}

fn cookie_path(base_path: &str) -> String {
    if base_path.is_empty() {
        "/".to_string()
    } else {
        base_path.to_string()
    }
}

fn build_session_cookie(token: String, base_path: &str, secure: bool, auth_ttl_secs: u64) -> Cookie<'static> {
    let mut cookie = Cookie::new(COOKIE_NAME, token);
    cookie.set_http_only(true);
    cookie.set_same_site(SameSite::Strict);
    cookie.set_path(cookie_path(base_path));
    cookie.set_secure(secure);
    let max_age = (auth_ttl_secs as i64).max(COOKIE_MAX_AGE_SECS);
    cookie.set_max_age(time::Duration::seconds(max_age));
    cookie
}

#[derive(Serialize)]
struct AuthStatusResp {
    authenticated: bool,
    ttl_secs: u64,
    auth_ttl_max: u64,
}

async fn auth_status(State(state): State<AppState>, jar: CookieJar) -> Json<AuthStatusResp> {
    let rem = jar
        .get(COOKIE_NAME)
        .and_then(|c| state.auth.validate(c.value()));
    let ttl_secs = rem.map(|d| d.as_secs().max(1)).unwrap_or(0);
    Json(AuthStatusResp {
        authenticated: rem.is_some(),
        ttl_secs,
        auth_ttl_max: state.auth.auth_ttl_secs(),
    })
}

async fn auth_touch(
    State(state): State<AppState>,
    headers: HeaderMap,
    jar: CookieJar,
) -> Response {
    let Some(token) = jar.get(COOKIE_NAME).map(|c| c.value().to_string()) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "auth_required" })),
        )
            .into_response();
    };
    let Some(rem) = state.auth.touch(&token) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "auth_expired" })),
        )
            .into_response();
    };
    let secure = is_https_request(&headers, state.secure_cookie);
    let cookie = build_session_cookie(token, &state.base_path, secure, state.auth.auth_ttl_secs());
    (
        jar.add(cookie),
        Json(AuthStatusResp {
            authenticated: true,
            ttl_secs: rem.as_secs().max(1),
            auth_ttl_max: state.auth.auth_ttl_secs(),
        }),
    )
        .into_response()
}

#[derive(Deserialize)]
struct VerifyReq {
    code: String,
}

async fn auth_verify(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    jar: CookieJar,
    Json(req): Json<VerifyReq>,
) -> Response {
    let ip = extract_client_ip(&headers, addr);
    match state.auth.authenticate(&req.code, &ip) {
        Ok(session) => {
            let secure = is_https_request(&headers, state.secure_cookie);
            let cookie = build_session_cookie(
                session.token,
                &state.base_path,
                secure,
                state.auth.auth_ttl_secs(),
            );

            info!("OTP authentication succeeded for {ip}");
            (
                jar.add(cookie),
                Json(AuthStatusResp {
                    authenticated: true,
                    ttl_secs: state.auth.auth_ttl_secs(),
                    auth_ttl_max: state.auth.auth_ttl_secs(),
                }),
            )
                .into_response()
        }
        Err(AuthError::RateLimited { retry_after_secs }) => {
            warn!("OTP rate limited for {ip} ({retry_after_secs}s)");
            (
                StatusCode::TOO_MANY_REQUESTS,
                Json(serde_json::json!({
                    "error": "rate_limited",
                    "message": format!("Too many failed attempts. Retry in {retry_after_secs}s."),
                    "retry_after_secs": retry_after_secs,
                })),
            )
                .into_response()
        }
        Err(AuthError::InvalidCode) => {
            warn!("Invalid OTP attempt from {ip}");
            (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({
                    "error": "invalid_otp",
                    "message": "Invalid or already-used 6-digit OTP code.",
                })),
            )
                .into_response()
        }
    }
}

async fn auth_logout(
    State(state): State<AppState>,
    headers: HeaderMap,
    jar: CookieJar,
) -> impl IntoResponse {
    if let Some(c) = jar.get(COOKIE_NAME) {
        state.auth.revoke(c.value());
    }
    let mut removal = Cookie::new(COOKIE_NAME, "");
    removal.set_http_only(true);
    removal.set_same_site(SameSite::Strict);
    removal.set_path(cookie_path(&state.base_path));
    removal.set_secure(is_https_request(&headers, state.secure_cookie));
    removal.set_max_age(time::Duration::seconds(0));
    (
        jar.add(removal),
        Json(AuthStatusResp {
            authenticated: false,
            ttl_secs: 0,
            auth_ttl_max: state.auth.auth_ttl_secs(),
        }),
    )
}

fn require_auth(state: &AppState, jar: &CookieJar) -> Result<String, Response> {
    let token = jar
        .get(COOKIE_NAME)
        .map(|c| c.value().to_string())
        .ok_or_else(|| {
            (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "error": "auth_required" })),
            )
                .into_response()
        })?;
    if state.auth.validate(&token).is_none() {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "auth_expired" })),
        )
            .into_response());
    }
    Ok(token)
}

fn require_auth_touch(state: &AppState, jar: &CookieJar) -> Result<String, Response> {
    let token = jar
        .get(COOKIE_NAME)
        .map(|c| c.value().to_string())
        .ok_or_else(|| {
            (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "error": "auth_required" })),
            )
                .into_response()
        })?;
    if state.auth.touch(&token).is_none() {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "auth_expired" })),
        )
            .into_response());
    }
    Ok(token)
}

async fn list_sessions(State(state): State<AppState>, jar: CookieJar) -> Response {
    if let Err(resp) = require_auth(&state, &jar) {
        return resp;
    }
    match state.sessions.ensure_default() {
        Ok(list) => Json(serde_json::json!({ "sessions": list })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct CreateSessionReq {
    name: Option<String>,
    cols: Option<u16>,
    rows: Option<u16>,
}

async fn create_session(
    State(state): State<AppState>,
    jar: CookieJar,
    Json(req): Json<CreateSessionReq>,
) -> Response {
    if let Err(resp) = require_auth_touch(&state, &jar) {
        return resp;
    }
    match state
        .sessions
        .create(req.name, req.cols.unwrap_or(120), req.rows.unwrap_or(36))
    {
        Ok(sess) => (StatusCode::CREATED, Json(sess.info())).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct RenameSessionReq {
    name: String,
}

async fn rename_session(
    State(state): State<AppState>,
    jar: CookieJar,
    Path(id): Path<String>,
    Json(req): Json<RenameSessionReq>,
) -> Response {
    if let Err(resp) = require_auth_touch(&state, &jar) {
        return resp;
    }
    match state.sessions.get(&id) {
        Some(sess) => {
            sess.rename(req.name);
            Json(sess.info()).into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "session_not_found" })),
        )
            .into_response(),
    }
}

async fn delete_session(
    State(state): State<AppState>,
    jar: CookieJar,
    Path(id): Path<String>,
) -> Response {
    if let Err(resp) = require_auth_touch(&state, &jar) {
        return resp;
    }
    if state.sessions.delete(&id) {
        Json(serde_json::json!({ "deleted": true })).into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "session_not_found" })),
        )
            .into_response()
    }
}

async fn ws_handler(
    State(state): State<AppState>,
    jar: CookieJar,
    ws: WebSocketUpgrade,
) -> Response {
    let token = match require_auth(&state, &jar) {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    ws.on_upgrade(move |socket| handle_ws(socket, state, token))
}

struct ClientGuard {
    session: Arc<TerminalSession>,
}

impl ClientGuard {
    fn new(session: Arc<TerminalSession>) -> Self {
        session.inc_clients();
        Self { session }
    }
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.session.dec_clients();
    }
}

async fn handle_ws(socket: WebSocket, state: AppState, auth_token: String) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    let mut attached: Option<ClientGuard> = None;
    let mut pty_rx: Option<broadcast::Receiver<crate::session::OutputChunk>> = None;
    let mut sent_seq: u64 = 0;
    let mut acked_seq: u64 = 0;
    let mut backpressure_paused = false;

    let mut auth_tick = tokio::time::interval(Duration::from_secs(1));
    auth_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        // Check if we can resume pumping after ACK freed flow-control window
        if backpressure_paused && sent_seq.saturating_sub(acked_seq) < MAX_IN_FLIGHT_BYTES / 2 {
            if let Some(guard) = &attached {
                let slice = guard.session.slice_from(sent_seq);
                match slice {
                    ReplaySlice::UpToDate { end_seq } => {
                        sent_seq = end_seq;
                    }
                    ReplaySlice::Delta { end_seq, data } => {
                        let frame = encode_output(end_seq, &data);
                        if ws_tx.send(Message::Binary(frame)).await.is_err() {
                            break;
                        }
                        sent_seq = end_seq;
                    }
                    ReplaySlice::FullSnapshot { end_seq, data } => {
                        let frame = encode_snapshot(end_seq, true, &data);
                        if ws_tx.send(Message::Binary(frame)).await.is_err() {
                            break;
                        }
                        sent_seq = end_seq;
                    }
                }
            }
            backpressure_paused = false;
        }

        tokio::select! {
            _ = auth_tick.tick() => {
                if state.auth.validate(&auth_token).is_none() {
                    debug!("auth session expired during active WS; locking client without killing PTY");
                    let lock_frame = encode_json_frame(OP_AUTH_LOCK, &AuthLockPayload {
                        reason: "expired".to_string(),
                        ttl: 0,
                    });
                    let _ = ws_tx.send(Message::Binary(lock_frame)).await;
                    let _ = ws_tx.send(Message::Close(Some(CloseFrame {
                        code: WS_CLOSE_AUTH_EXPIRED,
                        reason: "auth_expired".into(),
                    }))).await;
                    break;
                }
            }

            incoming = ws_rx.next() => {
                let Some(msg_res) = incoming else {
                    break;
                };
                let Ok(msg) = msg_res else {
                    break;
                };

                // Re-check auth on every client message
                let Some(rem_ttl) = state.auth.validate(&auth_token) else {
                    let lock_frame = encode_json_frame(OP_AUTH_LOCK, &AuthLockPayload {
                        reason: "expired".to_string(),
                        ttl: 0,
                    });
                    let _ = ws_tx.send(Message::Binary(lock_frame)).await;
                    let _ = ws_tx.send(Message::Close(Some(CloseFrame {
                        code: WS_CLOSE_AUTH_EXPIRED,
                        reason: "auth_expired".into(),
                    }))).await;
                    break;
                };

                match msg {
                    Message::Binary(data) => {
                        match decode_client_frame(data) {
                            Ok(ClientFrame::Attach(req)) => {
                                let Some(sess) = state.sessions.get(&req.session_id) else {
                                    let err = encode_json_frame(OP_ERROR, &ErrorPayload {
                                        code: "session_not_found".into(),
                                        message: format!("Session {} not found", req.session_id),
                                    });
                                    let _ = ws_tx.send(Message::Binary(err)).await;
                                    continue;
                                };

                                // Detach previous if switching tabs on same socket
                                drop(attached.take());
                                let _ = sess.resize(req.cols, req.rows);
                                let (rx, slice) = sess.subscribe_and_slice(req.last_seq);
                                let guard = ClientGuard::new(Arc::clone(&sess));

                                let info_frame = encode_json_frame(OP_SESSION, &sess.info());
                                if ws_tx.send(Message::Binary(info_frame)).await.is_err() {
                                    break;
                                }

                                match slice {
                                    ReplaySlice::UpToDate { end_seq } => {
                                        sent_seq = end_seq;
                                        acked_seq = end_seq;
                                    }
                                    ReplaySlice::Delta { end_seq, data } => {
                                        let frame = encode_snapshot(end_seq, false, &data);
                                        if ws_tx.send(Message::Binary(frame)).await.is_err() {
                                            break;
                                        }
                                        sent_seq = end_seq;
                                        acked_seq = req.last_seq;
                                    }
                                    ReplaySlice::FullSnapshot { end_seq, data } => {
                                        let frame = encode_snapshot(end_seq, true, &data);
                                        if ws_tx.send(Message::Binary(frame)).await.is_err() {
                                            break;
                                        }
                                        sent_seq = end_seq;
                                        acked_seq = end_seq.saturating_sub(data.len() as u64);
                                    }
                                }

                                attached = Some(guard);
                                pty_rx = Some(rx);
                                backpressure_paused = false;
                            }
                            Ok(ClientFrame::Input(bytes)) => {
                                let _ = state.auth.touch(&auth_token);
                                if let Some(guard) = &attached {
                                    let _ = guard.session.write_input(&bytes);
                                }
                            }
                            Ok(ClientFrame::Activity) => {
                                let _ = state.auth.touch(&auth_token);
                            }
                            Ok(ClientFrame::Resize { cols, rows }) => {
                                if let Some(guard) = &attached {
                                    let _ = guard.session.resize(cols, rows);
                                }
                            }
                            Ok(ClientFrame::Ack { seq }) => {
                                if seq > acked_seq {
                                    acked_seq = seq;
                                }
                            }
                            Ok(ClientFrame::Ping { client_ts_ms }) => {
                                let pong = encode_pong(client_ts_ms, rem_ttl.as_secs().max(1) as u32);
                                if ws_tx.send(Message::Binary(pong)).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                debug!("bad client frame: {e}");
                            }
                        }
                    }
                    Message::Ping(payload) => {
                        let _ = ws_tx.send(Message::Pong(payload)).await;
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }

            chunk_res = recv_pty_opt(&mut pty_rx), if pty_rx.is_some() && !backpressure_paused => {
                match chunk_res {
                    Ok(chunk) => {
                        if chunk.end_seq <= sent_seq {
                            continue;
                        }
                        let frame = encode_output(chunk.end_seq, &chunk.data);
                        if ws_tx.send(Message::Binary(frame)).await.is_err() {
                            break;
                        }
                        sent_seq = chunk.end_seq;
                        if sent_seq.saturating_sub(acked_seq) >= MAX_IN_FLIGHT_BYTES {
                            backpressure_paused = true;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Resync seamlessly from ring buffer
                        if let Some(guard) = &attached {
                            match guard.session.slice_from(sent_seq) {
                                ReplaySlice::UpToDate { end_seq } => sent_seq = end_seq,
                                ReplaySlice::Delta { end_seq, data } => {
                                    let frame = encode_output(end_seq, &data);
                                    if ws_tx.send(Message::Binary(frame)).await.is_err() {
                                        break;
                                    }
                                    sent_seq = end_seq;
                                }
                                ReplaySlice::FullSnapshot { end_seq, data } => {
                                    let frame = encode_snapshot(end_seq, true, &data);
                                    if ws_tx.send(Message::Binary(frame)).await.is_err() {
                                        break;
                                    }
                                    sent_seq = end_seq;
                                }
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        pty_rx = None;
                    }
                }
            }
        }
    }
}

async fn recv_pty_opt(
    rx: &mut Option<broadcast::Receiver<crate::session::OutputChunk>>,
) -> Result<crate::session::OutputChunk, broadcast::error::RecvError> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}
