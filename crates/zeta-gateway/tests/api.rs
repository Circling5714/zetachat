//! HTTP API tests with an in-memory store and a fake Zalo connector.

use std::sync::{Arc, RwLock};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tower::ServiceExt;
use zeta_gateway::api::{self, AppState};
use zeta_gateway::events::EventBus;
use zeta_gateway::media_cache::MediaCache;
use zeta_gateway::model::{thread_id, Media, NewMessage, ThreadKind, ThreadUpsert};
use zeta_gateway::store::Store;
use zeta_gateway::zalo::{LoginState, ZaloCommand, ZaloHandle, ZaloStatus};

struct Fixture {
    app: Router,
    store: Arc<Store>,
    token: String,
}

/// A connector that "sends" by storing the message, like the real one does.
fn fake_connector(store: Arc<Store>, bus: EventBus) -> ZaloHandle {
    let (commands, mut rx) = mpsc::channel(8);
    let status = Arc::new(RwLock::new(ZaloStatus::default()));
    tokio::spawn(async move {
        let mut sent = 0;
        while let Some(cmd) = rx.recv().await {
            sent += 1;
            match cmd {
                ZaloCommand::SendText {
                    kind,
                    remote_id,
                    text,
                    reply,
                } => {
                    let thread = ThreadUpsert {
                        kind,
                        remote_id: remote_id.clone(),
                        name: String::new(),
                        avatar: String::new(),
                    };
                    store.upsert_thread(&thread, false).unwrap();
                    let message = store
                        .insert_message(&NewMessage {
                            thread_id: thread.id(),
                            remote_id: "srv-1".into(),
                            client_id: "1".into(),
                            sender_id: "me".into(),
                            sender_name: String::new(),
                            outgoing: true,
                            kind: "webchat".into(),
                            text,
                            media: None,
                            sent_at: 10,
                            raw: "{}".into(),
                        })
                        .unwrap()
                        .unwrap();
                    bus.publish("message", json!({ "message": message }))
                        .unwrap();
                    let _ = reply.send(Ok(message));
                }
                ZaloCommand::SendAttachment {
                    kind,
                    remote_id,
                    attachment,
                    reply,
                } => {
                    let thread = ThreadUpsert {
                        kind,
                        remote_id,
                        name: String::new(),
                        avatar: String::new(),
                    };
                    store.upsert_thread(&thread, false).unwrap();
                    // Echo what arrived, so the test can check the request was parsed.
                    let media = Media {
                        kind: if attachment.is_photo() {
                            "image"
                        } else {
                            "file"
                        }
                        .into(),
                        url: "https://f.zdn.vn/x".into(),
                        name: attachment.name.clone(),
                        description: attachment.caption.clone(),
                        width: attachment.width,
                        height: attachment.height,
                        size: attachment.bytes.len() as u64,
                        ..Media::default()
                    };
                    let message = store
                        .insert_message(&NewMessage {
                            thread_id: thread.id(),
                            remote_id: format!("srv-file-{sent}"),
                            client_id: format!("file-{sent}"),
                            sender_id: "me".into(),
                            sender_name: String::new(),
                            outgoing: true,
                            kind: attachment.mime.clone(),
                            text: String::new(),
                            media: Some(media),
                            sent_at: 20,
                            raw: "{}".into(),
                        })
                        .unwrap()
                        .unwrap();
                    let _ = reply.send(Ok(message));
                }
                ZaloCommand::StartLogin { reply } => {
                    let _ = reply.send(LoginState::WaitingScan {
                        qr: "https://example/qr".into(),
                        expires_at: 1,
                    });
                }
            }
        }
    });
    ZaloHandle { commands, status }
}

fn fixture() -> Fixture {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let bus = EventBus::new(store.clone());
    let zalo = fake_connector(store.clone(), bus.clone());
    let (_, token) = store.add_device("test").unwrap();
    let media = std::env::temp_dir().join(format!("zetachat-api-{:016x}", rand::random::<u64>()));
    let app = api::router(AppState {
        store: store.clone(),
        bus,
        zalo,
        media: Arc::new(MediaCache::new(media).unwrap()),
    });
    Fixture { app, store, token }
}

async fn call(
    f: &Fixture,
    method: &str,
    uri: &str,
    body: Option<Value>,
    auth: bool,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if auth {
        req = req.header("authorization", format!("Bearer {}", f.token));
    }
    let req = match body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(b.to_string())),
        None => req.body(Body::empty()),
    }
    .unwrap();
    let resp = f.app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn health_is_public_everything_else_needs_a_token() {
    let f = fixture();
    let (status, body) = call(&f, "GET", "/api/v1/health", None, false).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);

    for uri in ["/api/v1/status", "/api/v1/threads", "/api/v1/events"] {
        let (status, _) = call(&f, "GET", uri, None, false).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
    }
    let (status, body) = call(&f, "GET", "/api/v1/status", None, true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["zalo"]["session"], "starting");
}

#[tokio::test]
async fn revoked_device_is_refused() {
    let f = fixture();
    let device = f.store.devices().unwrap().remove(0);
    f.store.revoke_device(&device.id).unwrap();
    let (status, _) = call(&f, "GET", "/api/v1/threads", None, true).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn send_then_read_back_through_threads_messages_and_events() {
    let f = fixture();
    let id = thread_id(ThreadKind::User, "42");
    let uri = format!("/api/v1/threads/{id}/messages");

    let (status, body) = call(
        &f,
        "POST",
        &uri,
        Some(json!({ "text": "  xin chào  " })),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["text"], "xin chào");
    assert_eq!(body["outgoing"], true);

    let (_, threads) = call(&f, "GET", "/api/v1/threads", None, true).await;
    assert_eq!(threads.as_array().unwrap().len(), 1);
    assert_eq!(threads[0]["id"], id);

    let (_, messages) = call(&f, "GET", &uri, None, true).await;
    assert_eq!(messages[0]["remote_id"], "srv-1");

    let (_, events) = call(&f, "GET", "/api/v1/events?since=0", None, true).await;
    assert_eq!(events[0]["kind"], "message");
    let seq = events[0]["seq"].as_i64().unwrap();
    let (_, later) = call(
        &f,
        "GET",
        &format!("/api/v1/events?since={seq}"),
        None,
        true,
    )
    .await;
    assert!(later.as_array().unwrap().is_empty());
}

/// POST raw bytes, as the web client does for photos and files.
async fn upload(
    f: &Fixture,
    uri: &str,
    content_type: Option<&str>,
    body: &[u8],
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("authorization", format!("Bearer {}", f.token));
    if let Some(content_type) = content_type {
        req = req.header("content-type", content_type);
    }
    let resp = f
        .app
        .clone()
        .oneshot(req.body(Body::from(body.to_vec())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn photos_and_files_are_handed_to_the_connector() {
    let f = fixture();
    let base = "/api/v1/threads/zalo:u:42/attachments";

    let (status, body) = upload(
        &f,
        &format!("{base}?name=%E1%BA%A2nh.png&width=640&height=480&caption=Xem%20n%C3%A8"),
        Some("image/png"),
        b"\x89PNG....",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["kind"], "image/png");
    assert_eq!(body["media"]["kind"], "image");
    assert_eq!(body["media"]["name"], "Ảnh.png");
    assert_eq!(body["media"]["description"], "Xem nè");
    assert_eq!(
        (
            body["media"]["width"].as_u64(),
            body["media"]["height"].as_u64()
        ),
        (Some(640), Some(480))
    );

    // No content type: guessed from the name. Directories are dropped.
    let (status, body) = upload(
        &f,
        &format!("{base}?name=..%2F..%2Fbao-cao.pdf"),
        None,
        b"%PDF",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["kind"], "application/pdf");
    assert_eq!(body["media"]["kind"], "file");
    assert_eq!(body["media"]["name"], "bao-cao.pdf");
    assert_eq!(body["media"]["size"], 4);

    let (status, _) = upload(&f, &format!("{base}?name=a.txt"), Some("text/plain"), b"").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = upload(&f, base, Some("text/plain"), b"x").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "name is required");
    let (status, _) = upload(&f, "/api/v1/threads/bogus/attachments?name=a", None, b"x").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn bad_input_is_rejected() {
    let f = fixture();
    let (status, _) = call(
        &f,
        "POST",
        "/api/v1/threads/zalo:u:42/messages",
        Some(json!({ "text": "  " })),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(
        &f,
        "POST",
        "/api/v1/threads/bogus/messages",
        Some(json!({ "text": "x" })),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(&f, "POST", "/api/v1/threads/zalo:u:404/read", None, true).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn read_marker_and_login_endpoints() {
    let f = fixture();
    let id = thread_id(ThreadKind::User, "7");
    f.store
        .upsert_thread(
            &ThreadUpsert {
                kind: ThreadKind::User,
                remote_id: "7".into(),
                name: "B".into(),
                avatar: String::new(),
            },
            true,
        )
        .unwrap();
    let (status, _) = call(
        &f,
        "POST",
        &format!("/api/v1/threads/{id}/read"),
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(&f, "POST", "/api/v1/zalo/login", None, true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["state"], "waiting_scan");
    assert_eq!(body["qr"], "https://example/qr");

    let (_, body) = call(&f, "GET", "/api/v1/zalo/login", None, true).await;
    assert_eq!(body["state"], "idle");
}
