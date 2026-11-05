//! HTTP API for client devices.
//!
//! Every route except `/api/v1/health` needs a device token (`Authorization:
//! Bearer zc_…` or the `zetachat_token` cookie). Live updates come from the
//! server-sent event stream, which resumes from `Last-Event-ID`.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE, SET_COOKIE,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures_util::Stream;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{broadcast, oneshot};
use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::auth::AuthDevice;
use crate::events::EventBus;
use crate::media_cache::{self, FetchError, MediaCache};
use crate::model::{split_thread_id, Message, StoredEvent, Thread};
use crate::store::Store;
use crate::uploads::{CancelError, SpoolError, Upload, Uploads, MAX_PENDING};
use crate::zalo::{photo_type, Attachment, LoginState, ZaloCommand, ZaloHandle};

/// Shared state of the HTTP handlers.
#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
    pub bus: EventBus,
    pub zalo: ZaloHandle,
    pub media: Arc<MediaCache>,
    pub uploads: Arc<Uploads>,
}

/// JSON error `{ "error": "…" }` with an HTTP status.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized")
    }

    pub fn not_found(what: &str) -> Self {
        Self::new(StatusCode::NOT_FOUND, format!("{what} not found"))
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    /// The Zalo side failed (send rejected, not logged in…).
    pub fn bad_gateway(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, message)
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, message)
    }

    /// Logs the cause; the client only sees a generic message.
    pub fn internal(e: impl std::fmt::Display) -> Self {
        tracing::error!(error = %e, "internal error");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/status", get(status))
        .route("/api/v1/threads", get(list_threads))
        .route(
            "/api/v1/threads/{id}/messages",
            get(list_messages).post(send_message),
        )
        // The body goes to disk as it comes, within Zalo's limit (checked there).
        .route(
            "/api/v1/threads/{id}/attachments",
            post(send_attachment).layer(DefaultBodyLimit::disable()),
        )
        .route("/api/v1/threads/{id}/messages/{seq}/reactions", post(react))
        .route("/api/v1/uploads", get(list_uploads))
        .route("/api/v1/uploads/{id}", delete(cancel_upload))
        .route("/api/v1/threads/{id}/read", post(mark_read))
        .route("/api/v1/events", get(list_events))
        .route("/api/v1/events/stream", get(stream_events))
        .route("/api/v1/media", get(media))
        .route("/api/v1/zalo/login", get(login_state).post(start_login))
        .route("/api/v1/zalo/login/qr.svg", get(login_qr_svg))
        .route(
            "/api/v1/session",
            post(create_session).delete(delete_session),
        )
        .merge(crate::web::routes())
        .with_state(state)
}

fn clamp(limit: Option<i64>, default: i64) -> i64 {
    limit.unwrap_or(default).clamp(1, 200)
}

async fn health() -> Json<Value> {
    Json(json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") }))
}

async fn status(State(st): State<AppState>, _: AuthDevice) -> ApiResult<Value> {
    let last_event_seq = st.store.last_event_seq().map_err(ApiError::internal)?;
    let limits = st.zalo.limits();
    Ok(Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "zalo": st.zalo.status(),
        "last_event_seq": last_event_seq,
        "limits": {
            "max_file_bytes": limits.max_file_bytes,
            "max_photo_bytes": limits.max_photo_bytes,
            "blocked_extensions": limits.blocked_extensions,
        },
    })))
}

#[derive(Deserialize)]
struct SessionBody {
    token: String,
}

fn session_cookie(value: &str, max_age: i64, secure: bool) -> String {
    format!(
        "{}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}{}",
        crate::auth::COOKIE_NAME,
        if secure { "; Secure" } else { "" }
    )
}

/// Exchange a device token for an HttpOnly cookie, so the browser client never has
/// to keep the token in JavaScript-readable storage. `SameSite=Strict` keeps other
/// sites from riding on the cookie.
async fn create_session(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SessionBody>,
) -> Result<Response, ApiError> {
    let token = body.token.trim();
    let device = st
        .store
        .device_for_token(token)
        .map_err(ApiError::internal)?
        .ok_or_else(ApiError::unauthorized)?;
    // Behind the reverse proxy the original scheme is in X-Forwarded-Proto.
    let secure = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|p| p.eq_ignore_ascii_case("https"));
    let cookie = session_cookie(token, 365 * 24 * 3600, secure);
    let body = Json(json!({ "device": { "id": device.id, "name": device.name } }));
    Ok(([(SET_COOKIE, cookie)], body).into_response())
}

async fn delete_session() -> Response {
    (
        [(SET_COOKIE, session_cookie("", 0, false))],
        Json(json!({ "ok": true })),
    )
        .into_response()
}

/// The pending login QR code as SVG, for clients that cannot draw QR codes.
async fn login_qr_svg(State(st): State<AppState>, _: AuthDevice) -> Result<Response, ApiError> {
    let LoginState::WaitingScan { qr, .. } = st.zalo.status().login else {
        return Err(ApiError::not_found("login QR code"));
    };
    let code = qrcode::QrCode::new(qr.as_bytes()).map_err(ApiError::internal)?;
    let svg = code
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(256, 256)
        .quiet_zone(true)
        .build();
    Ok((
        [(CONTENT_TYPE, "image/svg+xml"), (CACHE_CONTROL, "no-store")],
        svg,
    )
        .into_response())
}

#[derive(Deserialize)]
struct ThreadQuery {
    limit: Option<i64>,
    offset: Option<i64>,
    /// Include contacts that never exchanged a message.
    #[serde(default)]
    all: bool,
}

async fn list_threads(
    State(st): State<AppState>,
    _: AuthDevice,
    Query(q): Query<ThreadQuery>,
) -> ApiResult<Vec<Thread>> {
    let threads = st
        .store
        // Up to 1000 so a client can list a whole friend list in one call.
        .threads(
            q.all,
            q.limit.unwrap_or(50).clamp(1, 1000),
            q.offset.unwrap_or(0).max(0),
        )
        .map_err(ApiError::internal)?;
    Ok(Json(threads))
}

#[derive(Deserialize)]
struct MessageQuery {
    before: Option<i64>,
    limit: Option<i64>,
}

async fn list_messages(
    State(st): State<AppState>,
    _: AuthDevice,
    Path(id): Path<String>,
    Query(q): Query<MessageQuery>,
) -> ApiResult<Vec<Message>> {
    if split_thread_id(&id).is_none() {
        return Err(ApiError::not_found("thread"));
    }
    let messages = st
        .store
        .messages(&id, q.before, clamp(q.limit, 50))
        .map_err(ApiError::internal)?;
    Ok(Json(messages))
}

#[derive(Deserialize)]
struct SendBody {
    text: String,
}

async fn send_message(
    State(st): State<AppState>,
    _: AuthDevice,
    Path(id): Path<String>,
    Json(body): Json<SendBody>,
) -> ApiResult<Message> {
    let (kind, remote_id) = split_thread_id(&id).ok_or_else(|| ApiError::not_found("thread"))?;
    let text = body.text.trim();
    if text.is_empty() {
        return Err(ApiError::bad_request("text is empty"));
    }
    let (reply, answer) = oneshot::channel();
    st.zalo
        .commands
        .send(ZaloCommand::SendText {
            kind,
            remote_id: remote_id.to_string(),
            text: text.to_string(),
            reply,
        })
        .await
        .map_err(|_| ApiError::unavailable("Zalo connector is not running"))?;
    let message = answer
        .await
        .map_err(|_| ApiError::unavailable("Zalo connector is not running"))?
        .map_err(ApiError::bad_gateway)?;
    Ok(Json(message))
}

#[derive(Deserialize)]
struct ReactBody {
    icon: String,
}

/// React to a message with one of the six reactions Zalo offers (`/-strong`,
/// `/-heart`, `:>`, `:o`, `:-((`, `:-h`); an empty icon takes back this
/// account's reactions to it. Answers with the message and its reactions.
async fn react(
    State(st): State<AppState>,
    _: AuthDevice,
    Path((id, seq)): Path<(String, i64)>,
    Json(body): Json<ReactBody>,
) -> ApiResult<Message> {
    let (kind, remote_id) = split_thread_id(&id).ok_or_else(|| ApiError::not_found("thread"))?;
    if zeta_zalo::reaction::reaction_type(&body.icon).is_none() {
        return Err(ApiError::bad_request("unsupported reaction"));
    }
    let message = st
        .store
        .message(seq)
        .map_err(ApiError::internal)?
        .filter(|m| m.thread_id == id)
        .ok_or_else(|| ApiError::not_found("message"))?;
    if message.remote_id.starts_with("local:") || message.kind == "chat.undo" {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "tin này chưa thả cảm xúc được",
        ));
    }
    let client_id = if message.client_id.is_empty() {
        "0".to_string()
    } else {
        message.client_id.clone()
    };
    let (reply, answer) = oneshot::channel();
    st.zalo
        .commands
        .send(ZaloCommand::React {
            kind,
            remote_id: remote_id.to_string(),
            message_id: message.remote_id.clone(),
            message_client_id: client_id,
            icon: body.icon,
            reply,
        })
        .await
        .map_err(|_| ApiError::unavailable("Zalo connector is not running"))?;
    let updated = answer
        .await
        .map_err(|_| ApiError::unavailable("Zalo connector is not running"))?
        .map_err(ApiError::bad_gateway)?;
    // Nothing changed (the same reaction seen before): the message as stored.
    Ok(Json(updated.unwrap_or(message)))
}

#[derive(Deserialize)]
struct AttachmentQuery {
    /// File name shown to the recipient.
    name: String,
    #[serde(default)]
    caption: String,
    /// Picture size, when the client knows it.
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
    /// `file` sends a picture as a file (original quality, no preview).
    #[serde(default, rename = "as")]
    send_as: String,
}

/// A file name without directories or control characters, at most 180 characters.
fn clean_file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned: String = base.chars().filter(|c| !c.is_control()).take(180).collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() || cleaned.chars().all(|c| c == '.') {
        "tep".into()
    } else {
        cleaned.to_string()
    }
}

/// Content type from the file name, for clients that send none.
fn type_from_name(name: &str) -> &'static str {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "txt" => "text/plain",
        _ => "application/octet-stream",
    }
}

fn too_large(limit: u64) -> ApiError {
    ApiError::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        format!("Zalo chỉ nhận tệp tới {} MB", limit / (1024 * 1024)),
    )
}

/// Send a photo or a file. The body is the file itself, written to disk as it
/// arrives; the answer (`202`) comes once it is all there, with the upload that
/// then carries it to Zalo (follow it through `upload` events). Pictures in
/// common formats become photos unless `as=file`; anything else is a file.
async fn send_attachment(
    State(st): State<AppState>,
    _: AuthDevice,
    Path(id): Path<String>,
    Query(q): Query<AttachmentQuery>,
    headers: HeaderMap,
    body: Body,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let (kind, remote_id) = split_thread_id(&id).ok_or_else(|| ApiError::not_found("thread"))?;
    let name = clean_file_name(&q.name);
    let limits = st.zalo.limits();
    if let Some(ext) = limits.blocked_extension(&name) {
        return Err(ApiError::bad_request(format!(
            "Zalo không cho gửi tệp .{ext}; hãy nén thành .zip rồi gửi"
        )));
    }
    let announced = headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if announced.is_some_and(|n| n > limits.max_file_bytes) {
        return Err(too_large(limits.max_file_bytes));
    }
    if st.uploads.pending() >= MAX_PENDING {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "đang có nhiều tệp chờ gửi, hãy đợi một chút",
        ));
    }
    let declared = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty() && v != "application/octet-stream");
    let mime = declared.unwrap_or_else(|| type_from_name(&name).to_string());

    let file = st
        .uploads
        .receive(body, limits.max_file_bytes)
        .await
        .map_err(|e| match e {
            SpoolError::Empty => ApiError::bad_request("file is empty"),
            SpoolError::TooLarge(limit) => too_large(limit),
            SpoolError::Interrupted(reason) => ApiError::bad_request(reason),
            SpoolError::Io(e) => ApiError::internal(e),
        })?;
    let photo = q.send_as != "file" && photo_type(&mime) && file.size <= limits.max_photo_bytes;
    let (size, md5) = (file.size, file.md5.clone());
    let job = st.uploads.register(file, &id, &name, photo);
    let attachment = Attachment {
        name,
        mime,
        path: job.path().to_path_buf(),
        size,
        md5,
        width: q.width,
        height: q.height,
        caption: q.caption,
        photo,
    };
    let command = ZaloCommand::SendAttachment {
        kind,
        remote_id: remote_id.to_string(),
        attachment,
        job: job.clone(),
    };
    if st.zalo.commands.send(command).await.is_err() {
        job.finish(Err("Zalo connector is not running".into()));
        job.remove_file().await;
        return Err(ApiError::unavailable("Zalo connector is not running"));
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "upload": job.snapshot() })),
    ))
}

async fn list_uploads(State(st): State<AppState>, _: AuthDevice) -> Json<Vec<Upload>> {
    Json(st.uploads.list())
}

/// Cancel an upload that has not reached the sending of its message.
async fn cancel_upload(
    State(st): State<AppState>,
    _: AuthDevice,
    Path(id): Path<String>,
) -> ApiResult<Value> {
    let job = st
        .uploads
        .get(&id)
        .ok_or_else(|| ApiError::not_found("upload"))?;
    match job.cancel() {
        Ok(upload) => Ok(Json(json!({ "upload": upload }))),
        Err(CancelError::TooLate(_)) => Err(ApiError::new(
            StatusCode::CONFLICT,
            "tệp đã lên Zalo, không hủy được nữa",
        )),
    }
}

async fn mark_read(
    State(st): State<AppState>,
    _: AuthDevice,
    Path(id): Path<String>,
) -> ApiResult<Value> {
    let unread = st
        .store
        .thread(&id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("thread"))?
        .unread;
    st.store.mark_read(&id).map_err(ApiError::internal)?;
    let _ = st.bus.publish("thread_read", json!({ "thread_id": id }));
    // Tell Zalo too, for the messages that were unread here.
    if let (true, Some((kind, remote_id))) = (unread > 0, split_thread_id(&id)) {
        let messages = st
            .store
            .incoming_raw(&id, unread.min(50))
            .map_err(ApiError::internal)?;
        if !messages.is_empty() {
            let _ = st.zalo.commands.try_send(ZaloCommand::MarkSeen {
                kind,
                remote_id: remote_id.to_string(),
                messages,
            });
        }
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct EventQuery {
    since: Option<i64>,
    limit: Option<i64>,
}

async fn list_events(
    State(st): State<AppState>,
    _: AuthDevice,
    Query(q): Query<EventQuery>,
) -> ApiResult<Vec<StoredEvent>> {
    let events = st
        .store
        .events_since(q.since.unwrap_or(0), clamp(q.limit, 100))
        .map_err(ApiError::internal)?;
    Ok(Json(events))
}

/// Logged events carry their `seq` as id; transient ones (seq 0) carry none, so
/// a client's `Last-Event-ID` only ever points into the log.
fn to_sse(event: &StoredEvent) -> SseEvent {
    let sse = SseEvent::default()
        .event(event.kind.clone())
        .data(event.payload.to_string());
    if event.seq > 0 {
        sse.id(event.seq.to_string())
    } else {
        sse
    }
}

/// Server-sent events: the backlog after `Last-Event-ID` (or `?since=`), then live
/// events. A client that falls too far behind is disconnected and resumes from its
/// last id, so no event is lost.
async fn stream_events(
    State(st): State<AppState>,
    _: AuthDevice,
    headers: HeaderMap,
    Query(q): Query<EventQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let since = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok())
        .or(q.since)
        .unwrap_or(0);
    // Subscribe before reading the backlog so nothing falls in between.
    let live = st.bus.subscribe();
    let backlog = st
        .store
        .events_since(since, 1000)
        .map_err(ApiError::internal)?;
    let last = backlog.last().map(|e| e.seq).unwrap_or(since);
    let stream = event_stream(VecDeque::from(backlog), live, last);
    let sse = Sse::new(stream).keep_alive(KeepAlive::default());
    // Ask nginx (Nginx Proxy Manager) not to buffer the stream.
    Ok(([(HeaderName::from_static("x-accel-buffering"), "no")], sse))
}

fn event_stream(
    backlog: VecDeque<StoredEvent>,
    live: broadcast::Receiver<StoredEvent>,
    last: i64,
) -> impl Stream<Item = Result<SseEvent, Infallible>> {
    futures_util::stream::unfold(
        (backlog, live, last),
        |(mut backlog, mut live, mut last)| async move {
            if let Some(event) = backlog.pop_front() {
                return Some((Ok(to_sse(&event)), (backlog, live, last)));
            }
            loop {
                match live.recv().await {
                    Ok(event) if event.seq == 0 => {
                        return Some((Ok(to_sse(&event)), (backlog, live, last)));
                    }
                    Ok(event) if event.seq > last => {
                        last = event.seq;
                        return Some((Ok(to_sse(&event)), (backlog, live, last)));
                    }
                    Ok(_) => continue,
                    Err(_) => return None,
                }
            }
        },
    )
}

#[derive(Deserialize)]
struct MediaQuery {
    /// URL on Zalo's media CDN, as found in a message's `media`.
    u: String,
}

/// A file of Zalo's media CDN through the gateway's cache, so devices never
/// contact Zalo themselves. Range requests work (video seeking). Only displayable
/// media is served inline; everything else comes as an attachment. A sandbox CSP
/// and `nosniff` keep any file from running as a page of the gateway.
async fn media(
    State(st): State<AppState>,
    _: AuthDevice,
    Query(q): Query<MediaQuery>,
    request: Request,
) -> Result<Response, ApiError> {
    let file = st.media.get(&q.u).await.map_err(|e| match e {
        FetchError::NotAllowed => ApiError::bad_request("not a Zalo media URL"),
        FetchError::NotFound => ApiError::not_found("media"),
        FetchError::TooLarge => ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "file too large for the gateway",
        ),
        FetchError::Upstream(reason) => {
            tracing::warn!(%reason, "media download failed");
            ApiError::bad_gateway("Zalo did not deliver the file")
        }
        FetchError::Io(e) => ApiError::internal(e),
    })?;
    let mut response = ServeFile::new(&file.path)
        .oneshot(request)
        .await
        .map_err(ApiError::internal)?
        .map(Body::new);
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(file.content_type));
    // Zalo media URLs never change content; the token makes the answer private.
    headers.insert(
        CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=31536000, immutable"),
    );
    headers.insert(
        HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static("default-src 'none'; sandbox"),
    );
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        HeaderName::from_static("cross-origin-resource-policy"),
        HeaderValue::from_static("same-origin"),
    );
    if !media_cache::is_inline(file.content_type) {
        headers.insert(CONTENT_DISPOSITION, HeaderValue::from_static("attachment"));
    }
    Ok(response)
}

async fn login_state(State(st): State<AppState>, _: AuthDevice) -> Json<LoginState> {
    Json(st.zalo.status().login)
}

/// Start a QR login. The answer carries the QR payload once Zalo issued it.
async fn start_login(State(st): State<AppState>, _: AuthDevice) -> ApiResult<LoginState> {
    let (reply, answer) = oneshot::channel();
    st.zalo
        .commands
        .send(ZaloCommand::StartLogin { reply })
        .await
        .map_err(|_| ApiError::unavailable("Zalo connector is not running"))?;
    let state = answer
        .await
        .map_err(|_| ApiError::unavailable("Zalo connector is not running"))?;
    Ok(Json(state))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_are_cleaned() {
        assert_eq!(clean_file_name(r"D:\docs\Báo cáo.pdf"), "Báo cáo.pdf");
        assert_eq!(clean_file_name("../../etc/passwd"), "passwd");
        assert_eq!(clean_file_name("a\r\nb.txt"), "ab.txt");
        assert_eq!(clean_file_name(".."), "tep");
        assert_eq!(clean_file_name("  "), "tep");
        assert_eq!(clean_file_name(&"x".repeat(500)).len(), 180);
        assert_eq!(type_from_name("ẢNH.JPG"), "image/jpeg");
        assert_eq!(type_from_name("noext"), "application/octet-stream");
    }
}
