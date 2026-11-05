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
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use futures_util::Stream;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{broadcast, oneshot};
use tower::ServiceExt;
use tower_http::services::ServeFile;
use zeta_zalo::api::LastOnline;
use zeta_zalo::sticker::Sticker;

use crate::auth::AuthDevice;
use crate::events::EventBus;
use crate::media_cache::{self, CachedFile, FetchError, MediaCache};
use crate::model::{
    now_ms, split_thread_id, GroupMembers, Mention, Message, Person, StoredEvent, Thread,
    ThreadKind, PEOPLE_FRESH_MS,
};
use crate::stickers::StickerQuery;
use crate::store::Store;
use crate::uploads::{CancelError, SpoolError, Upload, Uploads, MAX_PENDING};
use crate::zalo::{
    photo_type, Attachment, FriendAction, LoginState, ReplyTo, SessionState, ZaloCommand,
    ZaloHandle,
};

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
        .route("/api/v1/threads/{id}/messages/{seq}/recall", post(recall))
        .route(
            "/api/v1/threads/{id}/messages/{seq}/forward",
            post(forward_message),
        )
        .route(
            "/api/v1/threads/{id}/messages/{seq}",
            delete(delete_message),
        )
        .route("/api/v1/uploads", get(list_uploads))
        .route("/api/v1/uploads/{id}", delete(cancel_upload))
        .route("/api/v1/threads/{id}/read", post(mark_read))
        .route("/api/v1/threads/{id}/typing", post(typing))
        .route("/api/v1/people", get(people))
        .route("/api/v1/friends", get(friends))
        .route("/api/v1/search", get(search))
        .route("/api/v1/stickers/packs", get(sticker_packs))
        .route("/api/v1/stickers/packs/{id}", get(pack_stickers))
        .route("/api/v1/stickers/search", get(search_stickers))
        .route("/api/v1/stickers/motion", get(sticker_motion))
        .route("/api/v1/threads/{id}/stickers", post(send_sticker))
        .route("/api/v1/threads/{id}/members", get(group_members))
        .route("/api/v1/threads/{id}/presence", get(presence))
        .route("/api/v1/users/find", get(find_user))
        .route("/api/v1/groups", post(create_group))
        .route("/api/v1/threads/{id}/name", put(rename_thread))
        .route(
            "/api/v1/friends/requests",
            get(friend_requests).post(send_friend_request),
        )
        .route(
            "/api/v1/friends/requests/{uid}/accept",
            post(accept_friend_request),
        )
        .route(
            "/api/v1/friends/requests/{uid}/reject",
            post(reject_friend_request),
        )
        .route("/api/v1/threads/{id}/shared", get(shared))
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
struct PeopleQuery {
    /// User ids, separated by commas; at most 100 are read.
    #[serde(default)]
    ids: String,
}

/// User ids of a query: digits only, no repeats, at most 100.
fn user_ids(list: &str) -> Vec<String> {
    let mut uids: Vec<String> = Vec::new();
    for uid in list.split(',').map(str::trim) {
        let valid = !uid.is_empty() && uid.len() <= 32 && uid.bytes().all(|b| b.is_ascii_digit());
        if valid && !uids.iter().any(|u| u == uid) {
            uids.push(uid.to_string());
            if uids.len() == 100 {
                break;
            }
        }
    }
    uids
}

/// Name and avatar of users: message senders, group members. Users the gateway
/// does not know, or has not heard about for days, are asked about first (at
/// most 50 at a time); the answer has the users the gateway knows, in the order
/// asked. A user Zalo says nothing about comes back with an empty name.
async fn people(
    State(st): State<AppState>,
    _: AuthDevice,
    Query(q): Query<PeopleQuery>,
) -> ApiResult<Vec<Person>> {
    let uids = user_ids(&q.ids);
    let known = st.store.people(&uids).map_err(ApiError::internal)?;
    let now = now_ms();
    let stale: Vec<String> = uids
        .iter()
        .filter(|uid| {
            !known
                .iter()
                .any(|p| &p.uid == *uid && now - p.updated_at < PEOPLE_FRESH_MS)
        })
        .take(50)
        .cloned()
        .collect();
    if stale.is_empty() || st.zalo.status().session != SessionState::Valid {
        return Ok(Json(known));
    }
    let (reply, answer) = oneshot::channel();
    let command = ZaloCommand::LookupPeople { uids: stale, reply };
    if st.zalo.commands.send(command).await.is_ok() {
        if let Ok(Err(e)) = answer.await {
            tracing::debug!(error = %e, "people lookup failed");
        }
    }
    let people = st.store.people(&uids).map_err(ApiError::internal)?;
    Ok(Json(people))
}

/// The friend list, with names and avatars.
async fn friends(State(st): State<AppState>, _: AuthDevice) -> ApiResult<Vec<Person>> {
    Ok(Json(st.store.friends().map_err(ApiError::internal)?))
}

#[derive(Deserialize)]
struct SearchQuery {
    #[serde(default)]
    q: String,
    limit: Option<i64>,
}

/// Messages containing the text searched for, ignoring case and Vietnamese
/// marks, newest first: `{ "messages": [...] }`.
async fn search(
    State(st): State<AppState>,
    _: AuthDevice,
    Query(q): Query<SearchQuery>,
) -> ApiResult<Value> {
    let limit = usize::try_from(clamp(q.limit, 30)).unwrap_or(30);
    let query: String = q.q.chars().take(100).collect();
    let store = st.store.clone();
    // A search may read the whole history: not on the async workers.
    let messages = tokio::task::spawn_blocking(move || store.search_messages(&query, limit))
        .await
        .map_err(ApiError::internal)?
        .map_err(ApiError::internal)?;
    Ok(Json(json!({ "messages": messages })))
}

/// Who is in a group, asked of Zalo now: `{ total, members: [{ uid, name,
/// avatar, friend, role }] }`, owner and admins first. Members the gateway does
/// not know yet are looked up (up to 300 are listed).
async fn group_members(
    State(st): State<AppState>,
    _: AuthDevice,
    Path(id): Path<String>,
) -> ApiResult<GroupMembers> {
    let Some((ThreadKind::Group, group_id)) = split_thread_id(&id) else {
        return Err(ApiError::not_found("group"));
    };
    let (reply, answer) = oneshot::channel();
    st.zalo
        .commands
        .send(ZaloCommand::GroupMembers {
            group_id: group_id.to_string(),
            reply,
        })
        .await
        .map_err(|_| ApiError::unavailable("Zalo connector is not running"))?;
    let members = answer
        .await
        .map_err(|_| ApiError::unavailable("Zalo connector is not running"))?
        .map_err(ApiError::bad_gateway)?;
    Ok(Json(members))
}

/// Ask the connector to do something on the friends side.
async fn friends_side(st: &AppState, action: FriendAction) -> ApiResult<Value> {
    let (reply, answer) = oneshot::channel();
    st.zalo
        .commands
        .send(ZaloCommand::Friends { action, reply })
        .await
        .map_err(|_| ApiError::unavailable("Zalo connector is not running"))?;
    let value = answer
        .await
        .map_err(|_| ApiError::unavailable("Zalo connector is not running"))?
        .map_err(ApiError::bad_gateway)?;
    Ok(Json(value))
}

/// A user id from a path or a body: digits only.
fn valid_uid(uid: &str) -> Result<String, ApiError> {
    let uid = uid.trim();
    if uid.is_empty() || uid.len() > 32 || !uid.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ApiError::bad_request("invalid user id"));
    }
    Ok(uid.to_string())
}

#[derive(Deserialize)]
struct RenameBody {
    #[serde(default)]
    name: String,
}

/// Rename a conversation: for a 1-1 chat, the name the account gives the other
/// person ("tên gợi nhớ", kept by Zalo and shown on the account's devices;
/// empty removes it); for a group, its name for every member (not empty).
/// Answers with the thread as stored.
async fn rename_thread(
    State(st): State<AppState>,
    _: AuthDevice,
    Path(id): Path<String>,
    Json(body): Json<RenameBody>,
) -> ApiResult<Thread> {
    let (kind, remote_id) = split_thread_id(&id).ok_or_else(|| ApiError::not_found("thread"))?;
    let name: String = body.name.trim().chars().take(100).collect();
    if kind == ThreadKind::Group && name.is_empty() {
        return Err(ApiError::bad_request("tên nhóm không được để trống"));
    }
    let (reply, answer) = oneshot::channel();
    st.zalo
        .commands
        .send(ZaloCommand::Rename {
            kind,
            remote_id: remote_id.to_string(),
            name,
            reply,
        })
        .await
        .map_err(|_| ApiError::unavailable("Zalo connector is not running"))?;
    answer
        .await
        .map_err(|_| ApiError::unavailable("Zalo connector is not running"))?
        .map_err(ApiError::bad_gateway)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("thread"))
}

#[derive(Deserialize)]
struct NewGroup {
    #[serde(default)]
    name: String,
    members: Vec<String>,
}

/// Make a group with friends `{ "name", "members": [uid…] }` (two or more, as
/// Zalo PC asks; an empty name lets Zalo name it). Answers `{ "thread",
/// "failed": [uid…] }`, `failed` being who Zalo could not add.
async fn create_group(
    State(st): State<AppState>,
    _: AuthDevice,
    Json(body): Json<NewGroup>,
) -> ApiResult<Value> {
    let name: String = body.name.trim().chars().take(100).collect();
    let mut members: Vec<String> = Vec::new();
    for uid in &body.members {
        let uid = valid_uid(uid)?;
        if !members.contains(&uid) {
            members.push(uid);
        }
    }
    if members.len() < 2 {
        return Err(ApiError::bad_request("chọn ít nhất 2 người để tạo nhóm"));
    }
    if members.len() > 100 {
        return Err(ApiError::bad_request("chọn tối đa 100 người một lần"));
    }
    let (reply, answer) = oneshot::channel();
    st.zalo
        .commands
        .send(ZaloCommand::CreateGroup {
            name,
            members,
            reply,
        })
        .await
        .map_err(|_| ApiError::unavailable("Zalo connector is not running"))?;
    let made = answer
        .await
        .map_err(|_| ApiError::unavailable("Zalo connector is not running"))?
        .map_err(ApiError::bad_gateway)?;
    Ok(Json(made))
}

#[derive(Deserialize)]
struct FindQuery {
    #[serde(default)]
    phone: String,
}

/// The Zalo account behind a phone number: `{ "user": { uid, name,
/// zalo_name, avatar, friendship } }`, or `{ "user": null }` when there is
/// none (or its owner does not let people find them by number). `friendship`
/// is `friend`, `request_sent`, `request_received` or `none`.
async fn find_user(
    State(st): State<AppState>,
    _: AuthDevice,
    Query(q): Query<FindQuery>,
) -> ApiResult<Value> {
    let digits = q.phone.chars().filter(char::is_ascii_digit).count();
    if !(9..=15).contains(&digits) {
        return Err(ApiError::bad_request("Số điện thoại không hợp lệ"));
    }
    friends_side(&st, FriendAction::Find(q.phone)).await
}

/// Friend requests received and not answered yet: `[{ uid, display_name,
/// zalo_name, avatar, message, time }]`.
async fn friend_requests(State(st): State<AppState>, _: AuthDevice) -> ApiResult<Value> {
    friends_side(&st, FriendAction::Requests).await
}

#[derive(Deserialize)]
struct FriendRequestBody {
    uid: String,
    #[serde(default)]
    message: String,
}

/// Ask a user to be friends: `{ "uid", "message" }`.
async fn send_friend_request(
    State(st): State<AppState>,
    _: AuthDevice,
    Json(body): Json<FriendRequestBody>,
) -> ApiResult<Value> {
    let uid = valid_uid(&body.uid)?;
    let message: String = body.message.trim().chars().take(150).collect();
    friends_side(&st, FriendAction::Send { uid, message }).await
}

async fn accept_friend_request(
    State(st): State<AppState>,
    _: AuthDevice,
    Path(uid): Path<String>,
) -> ApiResult<Value> {
    friends_side(&st, FriendAction::Accept(valid_uid(&uid)?)).await
}

async fn reject_friend_request(
    State(st): State<AppState>,
    _: AuthDevice,
    Path(uid): Path<String>,
) -> ApiResult<Value> {
    friends_side(&st, FriendAction::Reject(valid_uid(&uid)?)).await
}

/// When the other side of a 1-1 chat was last on Zalo: `{ visible, at }`,
/// `at` in milliseconds and 0 when they hide it. `404` for groups.
async fn presence(
    State(st): State<AppState>,
    _: AuthDevice,
    Path(id): Path<String>,
) -> ApiResult<LastOnline> {
    let Some((ThreadKind::User, uid)) = split_thread_id(&id) else {
        return Err(ApiError::not_found("user"));
    };
    let (reply, answer) = oneshot::channel();
    st.zalo
        .commands
        .send(ZaloCommand::LastOnline {
            uid: uid.to_string(),
            reply,
        })
        .await
        .map_err(|_| ApiError::unavailable("Zalo connector is not running"))?;
    let seen = answer
        .await
        .map_err(|_| ApiError::unavailable("Zalo connector is not running"))?
        .map_err(ApiError::bad_gateway)?;
    Ok(Json(seen))
}

#[derive(Deserialize)]
struct SharedQuery {
    /// `media` (photos, videos, GIFs), `file` or `link`.
    #[serde(rename = "type")]
    kind: String,
    before: Option<i64>,
    limit: Option<i64>,
}

/// What was shared in a conversation, newest first: the messages carrying
/// photos and videos, files, or links.
async fn shared(
    State(st): State<AppState>,
    _: AuthDevice,
    Path(id): Path<String>,
    Query(q): Query<SharedQuery>,
) -> ApiResult<Vec<Message>> {
    if split_thread_id(&id).is_none() {
        return Err(ApiError::not_found("thread"));
    }
    let kinds: &[&str] = match q.kind.as_str() {
        "media" => &["chat.photo", "chat.video.msg", "chat.gif"],
        "file" => &["share.file"],
        "link" => &["chat.recommended", "chat.link"],
        _ => return Err(ApiError::bad_request("type is media, file or link")),
    };
    let messages = st
        .store
        .messages_of_kinds(&id, kinds, q.before, clamp(q.limit, 30))
        .map_err(ApiError::internal)?;
    Ok(Json(messages))
}

/// Ask the connector's sticker store.
async fn ask_stickers(st: &AppState, query: StickerQuery) -> ApiResult<Value> {
    let (reply, answer) = oneshot::channel();
    st.zalo
        .commands
        .send(ZaloCommand::Stickers { query, reply })
        .await
        .map_err(|_| ApiError::unavailable("Zalo connector is not running"))?;
    let value = answer
        .await
        .map_err(|_| ApiError::unavailable("Zalo connector is not running"))?
        .map_err(ApiError::bad_gateway)?;
    Ok(Json(value))
}

/// The packs of Zalo's sticker store, newest first: `[{ id, name, icon, count }]`.
async fn sticker_packs(State(st): State<AppState>, _: AuthDevice) -> ApiResult<Value> {
    ask_stickers(&st, StickerQuery::Packs).await
}

/// The stickers of a pack: `[{ id, cate_id, type, url }]`.
async fn pack_stickers(
    State(st): State<AppState>,
    _: AuthDevice,
    Path(id): Path<u64>,
) -> ApiResult<Value> {
    ask_stickers(&st, StickerQuery::Pack(id)).await
}

#[derive(Deserialize)]
struct MotionQuery {
    #[serde(default)]
    ids: String,
}

/// How stickers move: `{ "<id>": { sprite, frames, duration_ms } | null }`
/// for at most 50 ids. Animated stickers are a sprite sheet, `frames` side
/// by side, each shown `duration_ms`; `null` is a still sticker.
async fn sticker_motion(
    State(st): State<AppState>,
    _: AuthDevice,
    Query(q): Query<MotionQuery>,
) -> ApiResult<Value> {
    let mut ids: Vec<u64> = Vec::new();
    for id in q
        .ids
        .split(',')
        .filter_map(|s| s.trim().parse::<u64>().ok())
    {
        if id > 0 && !ids.contains(&id) && ids.len() < 50 {
            ids.push(id);
        }
    }
    if ids.is_empty() {
        return Ok(Json(json!({})));
    }
    ask_stickers(&st, StickerQuery::Motion(ids)).await
}

#[derive(Deserialize)]
struct StickerSearch {
    #[serde(default)]
    q: String,
}

/// Stickers for a keyword ("haha", "cảm ơn"…), as Zalo suggests them.
async fn search_stickers(
    State(st): State<AppState>,
    _: AuthDevice,
    Query(q): Query<StickerSearch>,
) -> ApiResult<Value> {
    let keyword: String = q.q.trim().chars().take(50).collect();
    if keyword.is_empty() {
        return Ok(Json(json!([])));
    }
    ask_stickers(&st, StickerQuery::Search(keyword)).await
}

/// Send a sticker `{ "id", "cate_id", "type" }` (as listed by the sticker
/// routes); answers with the stored message.
async fn send_sticker(
    State(st): State<AppState>,
    _: AuthDevice,
    Path(id): Path<String>,
    Json(sticker): Json<Sticker>,
) -> ApiResult<Message> {
    let (kind, remote_id) = split_thread_id(&id).ok_or_else(|| ApiError::not_found("thread"))?;
    if sticker.id == 0 {
        return Err(ApiError::bad_request("sticker id is missing"));
    }
    Ok(Json(ask_send_sticker(&st, kind, remote_id, sticker).await?))
}

async fn ask_send_sticker(
    st: &AppState,
    kind: ThreadKind,
    remote_id: &str,
    sticker: Sticker,
) -> Result<Message, ApiError> {
    let (reply, answer) = oneshot::channel();
    st.zalo
        .commands
        .send(ZaloCommand::SendSticker {
            kind,
            remote_id: remote_id.to_string(),
            sticker,
            reply,
        })
        .await
        .map_err(|_| ApiError::unavailable("Zalo connector is not running"))?;
    let message = answer
        .await
        .map_err(|_| ApiError::unavailable("Zalo connector is not running"))?
        .map_err(ApiError::bad_gateway)?;
    Ok(message)
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
    /// `seq` of the message this one answers.
    #[serde(default)]
    reply_to: Option<i64>,
    /// People named in a group message (`@Tên`): positions in UTF-16 units
    /// of `text` as sent.
    #[serde(default)]
    mentions: Vec<Mention>,
}

/// Mentions that fit the text once it is trimmed: shifted by what the trim took
/// from the front, inside the text, with a user id (digits, or "-1" for
/// everyone). At most 50.
fn checked_mentions(raw: &str, text: &str, mentions: Vec<Mention>) -> Vec<Mention> {
    let lead = raw.len() - raw.trim_start().len();
    let shift = raw[..lead].encode_utf16().count() as u32;
    let units = text.encode_utf16().count() as u32;
    mentions
        .into_iter()
        .filter(|m| {
            m.uid == "-1" || (!m.uid.is_empty() && m.uid.bytes().all(|b| b.is_ascii_digit()))
        })
        .filter_map(|m| {
            let pos = m.pos.checked_sub(shift)?;
            (m.len > 0 && pos + m.len <= units).then_some(Mention { pos, ..m })
        })
        .take(50)
        .collect()
}

/// A message of thread `id` by its `seq`.
fn message_in(st: &AppState, id: &str, seq: i64) -> Result<Message, ApiError> {
    st.store
        .message(seq)
        .map_err(ApiError::internal)?
        .filter(|m| m.thread_id == id)
        .ok_or_else(|| ApiError::not_found("message"))
}

/// Zalo can act on a message once it has confirmed it, and not after a recall.
fn actionable(m: &Message) -> bool {
    !m.remote_id.starts_with("local:") && m.kind != "chat.undo"
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
    // Zalo takes mentions in groups only.
    let mentions = match kind {
        ThreadKind::Group => checked_mentions(&body.text, text, body.mentions),
        ThreadKind::User => Vec::new(),
    };
    let reply_to = match body.reply_to {
        Some(seq) => {
            let message = message_in(&st, &id, seq)?;
            if !actionable(&message) {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "tin này chưa trả lời được",
                ));
            }
            let raw = st.store.message_raw(seq).map_err(ApiError::internal)?;
            Some(Box::new(ReplyTo { message, raw }))
        }
        None => None,
    };
    let (reply, answer) = oneshot::channel();
    st.zalo
        .commands
        .send(ZaloCommand::SendText {
            kind,
            remote_id: remote_id.to_string(),
            text: text.to_string(),
            reply_to,
            mentions,
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
    let message = message_in(&st, &id, seq)?;
    if !actionable(&message) {
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

/// Recall one of this account's messages, for everyone in the chat. Answers
/// with the message as it is now.
async fn recall(
    State(st): State<AppState>,
    _: AuthDevice,
    Path((id, seq)): Path<(String, i64)>,
) -> ApiResult<Message> {
    let (kind, remote_id) = split_thread_id(&id).ok_or_else(|| ApiError::not_found("thread"))?;
    let message = message_in(&st, &id, seq)?;
    if !message.outgoing || !actionable(&message) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "chỉ thu hồi được tin mình đã gửi",
        ));
    }
    let (reply, answer) = oneshot::channel();
    st.zalo
        .commands
        .send(ZaloCommand::Recall {
            kind,
            remote_id: remote_id.to_string(),
            message_id: message.remote_id.clone(),
            message_client_id: message.client_id.clone(),
            reply,
        })
        .await
        .map_err(|_| ApiError::unavailable("Zalo connector is not running"))?;
    answer
        .await
        .map_err(|_| ApiError::unavailable("Zalo connector is not running"))?
        .map_err(ApiError::bad_gateway)?;
    let now = st.store.message(seq).map_err(ApiError::internal)?;
    Ok(Json(now.unwrap_or(message)))
}

/// What forwarding a message takes.
enum Forwardable {
    /// A text, or the address of a shared link: Zalo's own forward.
    Text(String),
    Sticker(Sticker),
    /// A photo, video, voice message or file: sent again from the gateway's
    /// copy (Zalo's forward refuses them as zca-js would send them).
    File {
        url: String,
        name: String,
        kind: String,
        width: u32,
        height: u32,
    },
}

/// The sticker of a message: from its Zalo JSON, else from its picture's
/// address (stickers sent before their ids were kept).
fn sticker_in(raw: &Value, url: &str) -> Option<Sticker> {
    let content = raw.get("content");
    let number = |key: &str| {
        content
            .and_then(|c| c.get(key))
            .and_then(|v| v.as_u64().or_else(|| v.as_str()?.trim().parse().ok()))
    };
    if let Some(id) = number("id").filter(|id| *id > 0) {
        return Some(Sticker {
            id,
            cate_id: number("catId").unwrap_or(0),
            kind: number("type")
                .and_then(|t| u32::try_from(t).ok())
                .filter(|t| *t > 0)
                .unwrap_or(7),
        });
    }
    let id = url.split("eid=").nth(1)?.split('&').next()?.parse().ok()?;
    Some(Sticker {
        id,
        cate_id: 0,
        kind: 7,
    })
}

fn forwardable(m: &Message, raw: &Value) -> Option<Forwardable> {
    let Some(md) = &m.media else {
        let text = m.text.trim();
        return (m.kind == "webchat" && !text.is_empty())
            .then(|| Forwardable::Text(text.to_string()));
    };
    match md.kind.as_str() {
        "link" if md.url.starts_with("http") => Some(Forwardable::Text(md.url.clone())),
        "sticker" => sticker_in(raw, &md.url).map(Forwardable::Sticker),
        "image" | "gif" | "video" | "audio" | "file" if !md.url.is_empty() => {
            Some(Forwardable::File {
                url: md.url.clone(),
                name: md.name.trim().to_string(),
                kind: md.kind.clone(),
                width: md.width,
                height: md.height,
            })
        }
        _ => None,
    }
}

/// A file name for media that came without one.
fn forward_name(name: &str, kind: &str, content_type: &str) -> String {
    if !name.is_empty() {
        return name.to_string();
    }
    let ext = match content_type {
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "video/mp4" => "mp4",
        t if t.starts_with("audio/") => "aac",
        t if t.starts_with("image/") => "jpg",
        t if t.starts_with("video/") => "mp4",
        _ => "bin",
    };
    let stem = match kind {
        "image" | "gif" => "anh",
        "video" => "video",
        "audio" => "ghi-am",
        _ => "tep",
    };
    format!("{stem}-zetachat.{ext}")
}

async fn forward_text(
    st: &AppState,
    kind: ThreadKind,
    remote_id: &str,
    text: &str,
) -> Result<Message, ApiError> {
    let (reply, answer) = oneshot::channel();
    st.zalo
        .commands
        .send(ZaloCommand::Forward {
            kind,
            remote_id: remote_id.to_string(),
            text: text.to_string(),
            reply,
        })
        .await
        .map_err(|_| ApiError::unavailable("Zalo connector is not running"))?;
    answer
        .await
        .map_err(|_| ApiError::unavailable("Zalo connector is not running"))?
        .map_err(ApiError::bad_gateway)
}

/// Send a file the gateway has as a new upload to a conversation.
async fn forward_file(
    st: &AppState,
    (kind, remote_id, thread): (ThreadKind, &str, &str),
    src: &CachedFile,
    name: &str,
    image: bool,
    (width, height): (u32, u32),
) -> Result<Upload, ApiError> {
    if st.uploads.pending() >= MAX_PENDING {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "đang có nhiều tệp chờ gửi, hãy đợi một chút",
        ));
    }
    let limits = st.zalo.limits();
    let file = st
        .uploads
        .spool_copy(&src.path, limits.max_file_bytes)
        .await
        .map_err(|e| match e {
            SpoolError::TooLarge(limit) => too_large(limit),
            SpoolError::Empty => ApiError::bad_request("file is empty"),
            other => ApiError::internal(other),
        })?;
    let mime = src.content_type.to_string();
    let photo = image && photo_type(&mime) && file.size <= limits.max_photo_bytes;
    let (size, md5) = (file.size, file.md5.clone());
    let job = st.uploads.register(file, thread, name, photo);
    let attachment = Attachment {
        name: name.to_string(),
        mime,
        path: job.path().to_path_buf(),
        size,
        md5,
        width,
        height,
        caption: String::new(),
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
    Ok(job.snapshot())
}

#[derive(Deserialize)]
struct ForwardBody {
    /// Conversations to forward to (at most 20).
    to: Vec<String>,
}

/// Forward a message to conversations: `{ "results": [{ "to", "message" |
/// "upload" | "error" }] }`, one per conversation. Texts and links go through
/// Zalo's forward, stickers are sent again; photos, videos, voice messages and
/// files are sent again from the gateway's copy, as uploads to follow.
async fn forward_message(
    State(st): State<AppState>,
    _: AuthDevice,
    Path((id, seq)): Path<(String, i64)>,
    Json(body): Json<ForwardBody>,
) -> ApiResult<Value> {
    let message = message_in(&st, &id, seq)?;
    if !actionable(&message) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "tin này chưa chuyển tiếp được",
        ));
    }
    let mut targets: Vec<String> = Vec::new();
    for target in body.to {
        if split_thread_id(&target).is_some() && !targets.contains(&target) {
            targets.push(target);
        }
    }
    if targets.is_empty() {
        return Err(ApiError::bad_request("chọn ít nhất một hội thoại"));
    }
    if targets.len() > 20 {
        return Err(ApiError::bad_request(
            "chuyển tiếp tối đa 20 hội thoại một lần",
        ));
    }
    let raw = st.store.message_raw(seq).map_err(ApiError::internal)?;
    let what = forwardable(&message, &raw)
        .ok_or_else(|| ApiError::new(StatusCode::CONFLICT, "chưa chuyển tiếp được loại tin này"))?;
    // A file is fetched once, then each conversation gets its own copy.
    let cached = match &what {
        Forwardable::File { url, .. } => Some(st.media.get(url).await.map_err(|e| match e {
            FetchError::TooLarge => ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "tệp lớn hơn 256 MB, gateway không giữ để chuyển tiếp được",
            ),
            other => ApiError::bad_gateway(other.to_string()),
        })?),
        _ => None,
    };
    let mut results = Vec::with_capacity(targets.len());
    for target in &targets {
        let Some((kind, remote_id)) = split_thread_id(target) else {
            continue;
        };
        let outcome = match (&what, &cached) {
            (Forwardable::Text(text), _) => forward_text(&st, kind, remote_id, text)
                .await
                .map(|m| json!({ "message": m })),
            (Forwardable::Sticker(sticker), _) => ask_send_sticker(&st, kind, remote_id, *sticker)
                .await
                .map(|m| json!({ "message": m })),
            (
                Forwardable::File {
                    name,
                    kind: media_kind,
                    width,
                    height,
                    ..
                },
                Some(file),
            ) => {
                let name = forward_name(name, media_kind, file.content_type);
                forward_file(
                    &st,
                    (kind, remote_id, target),
                    file,
                    &name,
                    media_kind == "image",
                    (*width, *height),
                )
                .await
                .map(|u| json!({ "upload": u }))
            }
            (Forwardable::File { .. }, None) => Err(ApiError::internal("file not fetched")),
        };
        results.push(match outcome {
            Ok(mut v) => {
                v["to"] = json!(target);
                v
            }
            Err(e) => json!({ "to": target, "error": e.message }),
        });
    }
    Ok(Json(json!({ "results": results })))
}

/// Delete a message for this account only: gone here and, through Zalo, on
/// the account's other devices; the others in the chat still see it. Clients
/// hear of it as `message_deleted`.
async fn delete_message(
    State(st): State<AppState>,
    _: AuthDevice,
    Path((id, seq)): Path<(String, i64)>,
) -> ApiResult<Value> {
    let (kind, remote_id) = split_thread_id(&id).ok_or_else(|| ApiError::not_found("thread"))?;
    let message = message_in(&st, &id, seq)?;
    // A message Zalo never confirmed is only here.
    if !message.remote_id.starts_with("local:") {
        let (reply, answer) = oneshot::channel();
        st.zalo
            .commands
            .send(ZaloCommand::DeleteForMe {
                kind,
                remote_id: remote_id.to_string(),
                message_id: message.remote_id.clone(),
                message_client_id: if message.client_id.is_empty() {
                    "0".into()
                } else {
                    message.client_id.clone()
                },
                owner_id: message.sender_id.clone(),
                reply,
            })
            .await
            .map_err(|_| ApiError::unavailable("Zalo connector is not running"))?;
        answer
            .await
            .map_err(|_| ApiError::unavailable("Zalo connector is not running"))?
            .map_err(ApiError::bad_gateway)?;
    }
    st.store.delete_message(seq).map_err(ApiError::internal)?;
    let thread = st.store.thread(&id).map_err(ApiError::internal)?;
    st.bus
        .publish(
            "message_deleted",
            json!({ "thread_id": id, "seq": seq, "thread": thread }),
        )
        .map_err(ApiError::internal)?;
    Ok(Json(json!({ "ok": true })))
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

/// The device user is typing in this thread: the gateway tells Zalo (if
/// enabled), at most every few seconds. Clients call it while typing goes on.
async fn typing(
    State(st): State<AppState>,
    _: AuthDevice,
    Path(id): Path<String>,
) -> ApiResult<Value> {
    let (kind, remote_id) = split_thread_id(&id).ok_or_else(|| ApiError::not_found("thread"))?;
    // Best effort: a busy connector simply skips it.
    let _ = st.zalo.commands.try_send(ZaloCommand::Typing {
        kind,
        remote_id: remote_id.to_string(),
    });
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
