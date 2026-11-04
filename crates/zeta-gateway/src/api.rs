//! HTTP API for client devices.
//!
//! Every route except `/api/v1/health` needs a device token (`Authorization:
//! Bearer zc_…` or the `zetachat_token` cookie). Live updates come from the
//! server-sent event stream, which resumes from `Last-Event-ID`.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::Stream;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{broadcast, oneshot};

use crate::auth::AuthDevice;
use crate::events::EventBus;
use crate::model::{split_thread_id, Message, StoredEvent, Thread};
use crate::store::Store;
use crate::zalo::{LoginState, ZaloCommand, ZaloHandle};

/// Shared state of the HTTP handlers.
#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
    pub bus: EventBus,
    pub zalo: ZaloHandle,
}

/// JSON error `{ "error": "…" }` with an HTTP status.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
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
        .route("/api/v1/threads/{id}/read", post(mark_read))
        .route("/api/v1/events", get(list_events))
        .route("/api/v1/events/stream", get(stream_events))
        .route("/api/v1/zalo/login", get(login_state).post(start_login))
        .with_state(state)
}

fn clamp(limit: Option<i64>, default: i64) -> i64 {
    limit.unwrap_or(default).clamp(1, 200)
}

async fn health() -> Json<Value> {
    Json(json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") }))
}

async fn status(State(st): State<AppState>, _: AuthDevice) -> Json<Value> {
    Json(json!({ "version": env!("CARGO_PKG_VERSION"), "zalo": st.zalo.status() }))
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
        .threads(q.all, clamp(q.limit, 50), q.offset.unwrap_or(0).max(0))
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

async fn mark_read(
    State(st): State<AppState>,
    _: AuthDevice,
    Path(id): Path<String>,
) -> ApiResult<Value> {
    if !st.store.mark_read(&id).map_err(ApiError::internal)? {
        return Err(ApiError::not_found("thread"));
    }
    let _ = st.bus.publish("thread_read", json!({ "thread_id": id }));
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

fn to_sse(event: &StoredEvent) -> SseEvent {
    SseEvent::default()
        .id(event.seq.to_string())
        .event(event.kind.clone())
        .data(event.payload.to_string())
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
