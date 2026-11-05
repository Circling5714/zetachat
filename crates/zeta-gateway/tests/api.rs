//! HTTP API tests with an in-memory store and a fake Zalo connector.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tower::ServiceExt;
use zeta_gateway::api::{self, AppState};
use zeta_gateway::events::EventBus;
use zeta_gateway::media_cache::MediaCache;
use zeta_gateway::model::{
    thread_id, GroupMembers, Media, Member, NewMessage, NewReaction, Person, ThreadKind,
    ThreadUpsert,
};
use zeta_gateway::store::Store;
use zeta_gateway::uploads::Uploads;
use zeta_gateway::zalo::{LoginState, SessionState, ZaloCommand, ZaloHandle, ZaloStatus};
use zeta_zalo::upload::FileLimits;

/// Read receipts the connector was asked for: (remote id, message ids).
type SeenLog = Arc<Mutex<Vec<(String, Vec<String>)>>>;
/// Users the connector was asked to look up, per request.
type AskedLog = Arc<Mutex<Vec<Vec<String>>>>;
/// Messages deleted for this account: (message id, owner id).
type DeletedLog = Arc<Mutex<Vec<(String, String)>>>;

struct Fixture {
    app: Router,
    store: Arc<Store>,
    token: String,
    /// Read receipts the connector was asked for: (remote id, message ids).
    seen: SeenLog,
    /// Users the connector was asked to look up.
    asked: AskedLog,
    deleted: DeletedLog,
    media: Arc<MediaCache>,
    status: Arc<std::sync::RwLock<ZaloStatus>>,
}

/// A connector that "sends" by storing the message, like the real one does.
fn fake_connector(
    store: Arc<Store>,
    bus: EventBus,
    seen: SeenLog,
    asked: AskedLog,
    deleted: DeletedLog,
) -> ZaloHandle {
    let (commands, mut rx) = mpsc::channel(8);
    let handle = ZaloHandle::new(commands);
    // Small limits, so the tests can go over them.
    *handle.limits.write().unwrap() = FileLimits {
        max_file_bytes: 64,
        max_photo_bytes: 16,
        ..FileLimits::default()
    };
    tokio::spawn(async move {
        let mut sent = 0;
        while let Some(cmd) = rx.recv().await {
            sent += 1;
            match cmd {
                ZaloCommand::SendText {
                    kind,
                    remote_id,
                    text,
                    reply_to,
                    mentions,
                    styles,
                    urgency,
                    reply,
                } => {
                    let extras = zeta_zalo::api::TextExtras {
                        mentions,
                        styles,
                        urgency,
                    };
                    let raw = zeta_gateway::zalo::sent_text_raw(&text, &extras)
                        .unwrap_or_else(|| json!({}))
                        .to_string();
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
                            remote_id: format!("srv-{sent}"),
                            client_id: format!("{sent}"),
                            sender_id: "me".into(),
                            sender_name: String::new(),
                            outgoing: true,
                            kind: "webchat".into(),
                            text,
                            media: None,
                            quote: reply_to.map(|r| zeta_gateway::model::Quote {
                                sender_name: r.message.sender_name,
                                text: r.message.text,
                                remote_id: r.message.remote_id,
                            }),
                            sent_at: 10,
                            raw,
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
                    job,
                } => {
                    // "slow…" files wait to be cancelled.
                    if attachment.name.starts_with("slow") {
                        tokio::spawn(async move {
                            assert!(job.start());
                            job.cancelled().await;
                            job.remove_file().await;
                        });
                        continue;
                    }
                    assert!(job.start());
                    job.progress(attachment.size);
                    assert!(job.begin_finishing());
                    let thread = ThreadUpsert {
                        kind,
                        remote_id,
                        name: String::new(),
                        avatar: String::new(),
                    };
                    store.upsert_thread(&thread, false).unwrap();
                    // Echo what arrived, so the test can check the request was parsed.
                    let content = std::fs::read(&attachment.path).unwrap();
                    let media = Media {
                        kind: if attachment.photo { "image" } else { "file" }.into(),
                        url: "https://f.zdn.vn/x".into(),
                        name: attachment.name.clone(),
                        description: attachment.caption.clone(),
                        width: attachment.width,
                        height: attachment.height,
                        size: attachment.size,
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
                            text: format!(
                                "{} {}",
                                String::from_utf8_lossy(&content),
                                attachment.md5
                            ),
                            media: Some(media),
                            quote: None,
                            sent_at: 20,
                            raw: "{}".into(),
                        })
                        .unwrap()
                        .unwrap();
                    job.finish(Ok(message));
                    job.remove_file().await;
                }
                ZaloCommand::React {
                    kind,
                    remote_id,
                    message_id,
                    message_client_id,
                    icon,
                    reply,
                } => {
                    let changed = store
                        .add_reaction(&NewReaction {
                            thread_id: thread_id(kind, &remote_id),
                            action_id: format!("r{sent}"),
                            client_id: String::new(),
                            targets: vec![(message_id, message_client_id)],
                            user_id: "me".into(),
                            mine: true,
                            icon,
                            sent_at: sent,
                        })
                        .unwrap();
                    let _ = reply.send(Ok(changed.into_iter().next()));
                }
                ZaloCommand::MarkSeen {
                    remote_id,
                    messages,
                    ..
                } => {
                    let ids = messages
                        .iter()
                        .map(|m| m["msgId"].as_str().unwrap_or_default().to_string())
                        .collect();
                    seen.lock().unwrap().push((remote_id, ids));
                }
                ZaloCommand::Typing { .. } => {}
                ZaloCommand::Forward {
                    kind,
                    remote_id,
                    text,
                    reply,
                } => {
                    let thread = ThreadUpsert {
                        kind,
                        remote_id,
                        name: String::new(),
                        avatar: String::new(),
                    };
                    store.upsert_thread(&thread, false).unwrap();
                    let message = store
                        .insert_message(&NewMessage {
                            thread_id: thread.id(),
                            remote_id: format!("srv-{sent}"),
                            client_id: format!("{sent}"),
                            sender_id: "me".into(),
                            sender_name: String::new(),
                            outgoing: true,
                            kind: "webchat".into(),
                            text,
                            media: None,
                            quote: None,
                            sent_at: 10,
                            raw: "{}".into(),
                        })
                        .unwrap()
                        .unwrap();
                    let _ = reply.send(Ok(message));
                }
                ZaloCommand::Rename {
                    kind,
                    remote_id,
                    name,
                    reply,
                } => {
                    let id = thread_id(kind, &remote_id);
                    match kind {
                        ThreadKind::User => store.set_alias(&remote_id, &name).unwrap(),
                        ThreadKind::Group => store.name_thread(&id, &name).unwrap(),
                    }
                    let _ = reply.send(store.thread(&id).map_err(|e| e.to_string()));
                }
                ZaloCommand::CreateGroup {
                    name,
                    members,
                    reply,
                } => {
                    let thread = ThreadUpsert {
                        kind: ThreadKind::Group,
                        remote_id: "777".into(),
                        name,
                        avatar: String::new(),
                    };
                    store.upsert_thread(&thread, true).unwrap();
                    store
                        .set_members(&thread.id(), 1 + members.len() as u64)
                        .unwrap();
                    let stored = store.thread(&thread.id()).unwrap();
                    let _ = reply.send(Ok(json!({ "thread": stored, "failed": [] })));
                }
                ZaloCommand::LastOnline { uid, reply } => {
                    let at = uid.parse::<i64>().unwrap_or(0);
                    let _ = reply.send(Ok(zeta_zalo::api::LastOnline { visible: true, at }));
                }
                ZaloCommand::DeleteForMe {
                    message_id,
                    owner_id,
                    reply,
                    ..
                } => {
                    deleted.lock().unwrap().push((message_id, owner_id));
                    let _ = reply.send(Ok(()));
                }
                // The answer says what was asked.
                ZaloCommand::Friends { action, reply } => {
                    let _ = reply.send(Ok(json!({ "action": format!("{action:?}") })));
                }
                // Every group has its owner and one member.
                ZaloCommand::GroupMembers { group_id, reply } => {
                    let member = |uid: &str, role| Member {
                        uid: uid.into(),
                        name: format!("Người {uid}"),
                        avatar: String::new(),
                        friend: false,
                        role,
                    };
                    let _ = reply.send(Ok(GroupMembers {
                        total: 12,
                        members: vec![member(&group_id, "owner"), member("2", "member")],
                    }));
                }
                // The store's answer says what was asked.
                ZaloCommand::Stickers { query, reply } => {
                    let _ = reply.send(Ok(json!({ "asked": format!("{query:?}") })));
                }
                ZaloCommand::SendSticker {
                    kind,
                    remote_id,
                    sticker,
                    reply,
                } => {
                    let thread = ThreadUpsert {
                        kind,
                        remote_id,
                        name: String::new(),
                        avatar: String::new(),
                    };
                    store.upsert_thread(&thread, false).unwrap();
                    let message = store
                        .insert_message(&NewMessage {
                            thread_id: thread.id(),
                            remote_id: format!("srv-{sent}"),
                            client_id: format!("{sent}"),
                            sender_id: "me".into(),
                            sender_name: String::new(),
                            outgoing: true,
                            kind: "chat.sticker".into(),
                            text: "[Nhãn dán]".into(),
                            media: zeta_gateway::zalo::sticker_media(&sticker),
                            quote: None,
                            sent_at: 10,
                            raw: "{}".into(),
                        })
                        .unwrap()
                        .unwrap();
                    let _ = reply.send(Ok(message));
                }
                // Zalo keeps one quick message; adding another is refused
                // as Zalo does for a personal account (821).
                ZaloCommand::Quick { action, reply } => {
                    let one = json!([{ "id": 7, "keyword": "chao", "text": "Xin chào",
                        "photo": "", "updated_at": 1 }]);
                    let _ = reply.send(match action {
                        zeta_gateway::zalo::QuickAction::Add { .. } => {
                            Err("Zalo không cho tài khoản này lưu thêm tin nhắn nhanh".into())
                        }
                        _ => Ok(one),
                    });
                }
                ZaloCommand::Cards { action, reply } => match action {
                    zeta_gateway::zalo::CardAction::SendBank {
                        kind,
                        remote_id,
                        bin,
                        number,
                        holder,
                    } => {
                        let (media, raw) =
                            zeta_gateway::zalo::bank_card(zeta_gateway::model::BankCard {
                                bin,
                                number,
                                holder,
                                bank: "Vietcombank".into(),
                            });
                        let thread = ThreadUpsert {
                            kind,
                            remote_id,
                            name: String::new(),
                            avatar: String::new(),
                        };
                        store.upsert_thread(&thread, false).unwrap();
                        let message = store
                            .insert_message(&NewMessage {
                                thread_id: thread.id(),
                                remote_id: format!("srv-{sent}"),
                                client_id: format!("{sent}"),
                                sender_id: "me".into(),
                                sender_name: String::new(),
                                outgoing: true,
                                kind: "chat.webcontent".into(),
                                text: "[Tài khoản ngân hàng]".into(),
                                media: Some(media),
                                quote: None,
                                sent_at: 10,
                                raw: raw.to_string(),
                            })
                            .unwrap()
                            .unwrap();
                        let _ = reply.send(Ok(json!(message)));
                    }
                    zeta_gateway::zalo::CardAction::Banks
                    | zeta_gateway::zalo::CardAction::Accounts
                    | zeta_gateway::zalo::CardAction::AddAccount { .. }
                    | zeta_gateway::zalo::CardAction::RemoveAccount { .. }
                    | zeta_gateway::zalo::CardAction::ReadBank { .. } => {
                        let _ = reply.send(Ok(json!([])));
                    }
                    zeta_gateway::zalo::CardAction::SendContact {
                        kind,
                        remote_id,
                        uid,
                    } => {
                        let person = zeta_gateway::model::Person {
                            uid,
                            name: "Lan".into(),
                            ..Default::default()
                        };
                        let (media, raw) = zeta_gateway::zalo::contact_card(
                            &person,
                            "https://qr-talk.zdn.vn/q.jpg",
                        );
                        let thread = ThreadUpsert {
                            kind,
                            remote_id,
                            name: String::new(),
                            avatar: String::new(),
                        };
                        store.upsert_thread(&thread, false).unwrap();
                        let message = store
                            .insert_message(&NewMessage {
                                thread_id: thread.id(),
                                remote_id: format!("srv-{sent}"),
                                client_id: format!("{sent}"),
                                sender_id: "me".into(),
                                sender_name: String::new(),
                                outgoing: true,
                                kind: "chat.recommended".into(),
                                text: "[Danh thiếp] Lan".into(),
                                media: Some(media),
                                quote: None,
                                sent_at: 10,
                                raw: raw.to_string(),
                            })
                            .unwrap()
                            .unwrap();
                        let _ = reply.send(Ok(json!(message)));
                    }
                },
                // Zalo knows everyone but the users whose id starts with 9.
                ZaloCommand::LookupPeople { uids, reply } => {
                    asked.lock().unwrap().push(uids.clone());
                    for uid in uids.iter().filter(|u| !u.starts_with('9')) {
                        let person = Person {
                            uid: uid.clone(),
                            name: format!("Người {uid}"),
                            avatar: format!("https://s120-ava-talk.zadn.vn/{uid}.jpg"),
                            ..Person::default()
                        };
                        store.upsert_person(&person, None).unwrap();
                    }
                    store
                        .touch_people(&uids, zeta_gateway::model::now_ms())
                        .unwrap();
                    let _ = reply.send(store.people(&uids).map_err(|e| e.to_string()));
                }
                ZaloCommand::Recall {
                    kind,
                    remote_id,
                    message_id,
                    message_client_id,
                    reply,
                } => {
                    let recalled = store
                        .recall_message(
                            &thread_id(kind, &remote_id),
                            &message_id,
                            &message_client_id,
                        )
                        .map_err(|e| e.to_string());
                    let _ = reply.send(recalled);
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
    handle
}

fn fixture() -> Fixture {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let bus = EventBus::new(store.clone());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let asked = Arc::new(Mutex::new(Vec::new()));
    let deleted = Arc::new(Mutex::new(Vec::new()));
    let zalo = fake_connector(
        store.clone(),
        bus.clone(),
        seen.clone(),
        asked.clone(),
        deleted.clone(),
    );
    let status = zalo.status.clone();
    let (_, token) = store.add_device("test").unwrap();
    let dir = std::env::temp_dir().join(format!("zetachat-api-{:016x}", rand::random::<u64>()));
    let uploads = Arc::new(Uploads::new(dir.join("uploads"), bus.clone()).unwrap());
    let media = Arc::new(MediaCache::new(dir.join("media")).unwrap());
    let app = api::router(AppState {
        store: store.clone(),
        bus,
        zalo,
        media: media.clone(),
        uploads,
    });
    Fixture {
        app,
        store,
        token,
        seen,
        asked,
        deleted,
        media,
        status,
    }
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

/// Wait until upload `id` has ended; returns it.
async fn ended_upload(f: &Fixture, id: &str) -> Value {
    for _ in 0..200 {
        let (_, list) = call(f, "GET", "/api/v1/uploads", None, true).await;
        let found = list
            .as_array()
            .unwrap()
            .iter()
            .find(|u| u["id"] == id)
            .cloned();
        if let Some(upload) = found {
            if ["done", "failed", "cancelled"].contains(&upload["state"].as_str().unwrap()) {
                return upload;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("upload {id} did not end");
}

/// Post a file and wait for its upload to end; returns the sent message.
async fn send_file(f: &Fixture, query: &str, content_type: Option<&str>, body: &[u8]) -> Value {
    let uri = format!("/api/v1/threads/zalo:u:42/attachments?{query}");
    let (status, body) = upload(f, &uri, content_type, body).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["upload"]["thread_id"], "zalo:u:42");
    let ended = ended_upload(f, body["upload"]["id"].as_str().unwrap()).await;
    assert_eq!(ended["state"], "done", "{ended}");
    assert_eq!(ended["sent"], ended["size"]);
    ended["message"].clone()
}

#[tokio::test]
async fn photos_and_files_are_handed_to_the_connector() {
    let f = fixture();
    let message = send_file(
        &f,
        "name=%E1%BA%A2nh.png&width=640&height=480&caption=Xem%20n%C3%A8",
        Some("image/png"),
        b"\x89PNG....",
    )
    .await;
    assert_eq!(message["kind"], "image/png");
    assert_eq!(message["media"]["kind"], "image");
    assert_eq!(message["media"]["name"], "Ảnh.png");
    assert_eq!(message["media"]["description"], "Xem nè");
    assert_eq!(
        (
            message["media"]["width"].as_u64(),
            message["media"]["height"].as_u64()
        ),
        (Some(640), Some(480))
    );

    // No content type: guessed from the name. Directories are dropped. The
    // connector gets the content and its MD5.
    let message = send_file(&f, "name=..%2F..%2Fbao-cao.pdf", None, b"%PDF").await;
    assert_eq!(message["kind"], "application/pdf");
    assert_eq!(message["media"]["kind"], "file");
    assert_eq!(message["media"]["name"], "bao-cao.pdf");
    assert_eq!(message["media"]["size"], 4);
    let md5 = message["text"]
        .as_str()
        .unwrap()
        .strip_prefix("%PDF ")
        .unwrap();
    assert_eq!(md5.len(), 32, "{message}");

    // Pictures go as files when asked, or when too large for a photo.
    let message = send_file(&f, "name=a.png&as=file", Some("image/png"), b"x").await;
    assert_eq!(message["media"]["kind"], "file");
    let message = send_file(&f, "name=big.png", Some("image/png"), &[7; 17]).await;
    assert_eq!(message["media"]["kind"], "file");

    // Zalo's rules: refused extensions, the size limit (announced or not).
    let base = "/api/v1/threads/zalo:u:42/attachments";
    let (status, body) = upload(&f, &format!("{base}?name=setup.EXE"), None, b"MZ").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains(".exe"), "{body}");
    let (status, body) = upload(&f, &format!("{base}?name=a.bin"), None, &[0; 65]).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    let req = Request::builder()
        .method("POST")
        .uri(format!("{base}?name=a.bin"))
        .header("authorization", format!("Bearer {}", f.token))
        .header("content-length", "1000")
        .body(Body::from(vec![0; 1000]))
        .unwrap();
    let resp = f.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);

    let (status, _) = upload(&f, &format!("{base}?name=a.txt"), Some("text/plain"), b"").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = upload(&f, base, Some("text/plain"), b"x").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "name is required");
    let (status, _) = upload(&f, "/api/v1/threads/bogus/attachments?name=a", None, b"x").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The status tells devices the limits.
    let (_, status) = call(&f, "GET", "/api/v1/status", None, true).await;
    assert_eq!(status["limits"]["max_file_bytes"], 64);
    assert!(status["limits"]["blocked_extensions"]
        .as_array()
        .unwrap()
        .contains(&json!("exe")));
}

#[tokio::test]
async fn uploads_can_be_cancelled_until_they_end() {
    let f = fixture();
    let uri = "/api/v1/threads/zalo:u:42/attachments?name=slow.zip";
    let (status, body) = upload(&f, uri, None, b"PK").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let id = body["upload"]["id"].as_str().unwrap().to_string();
    let cancel = format!("/api/v1/uploads/{id}");
    let (status, body) = call(&f, "DELETE", &cancel, None, true).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["upload"]["state"], "cancelled");
    assert_eq!(ended_upload(&f, &id).await["state"], "cancelled");
    let (status, _) = call(&f, "DELETE", &cancel, None, true).await;
    assert_eq!(status, StatusCode::CONFLICT, "already ended");
    let (status, _) = call(&f, "DELETE", "/api/v1/uploads/up_nope", None, true).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn reactions_go_to_zalo_and_come_back_counted() {
    let f = fixture();
    let id = thread_id(ThreadKind::User, "42");
    let (_, sent) = call(
        &f,
        "POST",
        &format!("/api/v1/threads/{id}/messages"),
        Some(json!({ "text": "hi" })),
        true,
    )
    .await;
    let seq = sent["seq"].as_i64().unwrap();
    let uri = format!("/api/v1/threads/{id}/messages/{seq}/reactions");
    let (status, body) = call(&f, "POST", &uri, Some(json!({ "icon": "/-heart" })), true).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["reactions"],
        json!([{ "icon": "/-heart", "emoji": "❤️", "count": 1, "mine": true }])
    );
    let (_, body) = call(&f, "POST", &uri, Some(json!({ "icon": "" })), true).await;
    assert_eq!(body["reactions"], json!([]), "taken back");
    let (_, messages) = call(
        &f,
        "GET",
        &format!("/api/v1/threads/{id}/messages"),
        None,
        true,
    )
    .await;
    assert_eq!(messages[0]["reactions"], json!([]));

    let (status, _) = call(&f, "POST", &uri, Some(json!({ "icon": "/-beer" })), true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "only the six Zalo offers");
    let other = format!("/api/v1/threads/zalo:u:9/messages/{seq}/reactions");
    let (status, _) = call(&f, "POST", &other, Some(json!({ "icon": ":>" })), true).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "the message is in another thread"
    );
}

#[tokio::test]
async fn replies_quote_their_message_and_own_messages_can_be_recalled() {
    let f = fixture();
    let id = thread_id(ThreadKind::User, "42");
    let uri = format!("/api/v1/threads/{id}/messages");
    let (_, first) = call(
        &f,
        "POST",
        &uri,
        Some(json!({ "text": "đi cà phê nhé" })),
        true,
    )
    .await;
    let first_seq = first["seq"].as_i64().unwrap();

    let (status, reply) = call(
        &f,
        "POST",
        &uri,
        Some(json!({ "text": "ok", "reply_to": first_seq })),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert_eq!(reply["quote"]["text"], "đi cà phê nhé");
    assert_eq!(reply["quote"]["remote_id"], first["remote_id"]);
    let (status, _) = call(
        &f,
        "POST",
        &uri,
        Some(json!({ "text": "ok", "reply_to": 9999 })),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let recall = format!("/api/v1/threads/{id}/messages/{}/recall", reply["seq"]);
    let (status, recalled) = call(&f, "POST", &recall, None, true).await;
    assert_eq!(status, StatusCode::OK, "{recalled}");
    assert_eq!(recalled["kind"], "chat.undo");
    let (status, _) = call(&f, "POST", &recall, None, true).await;
    assert_eq!(status, StatusCode::CONFLICT, "already recalled");

    // Only one's own messages.
    let incoming = f
        .store
        .insert_message(&NewMessage {
            thread_id: id.clone(),
            remote_id: "theirs".into(),
            client_id: String::new(),
            sender_id: "42".into(),
            sender_name: "B".into(),
            outgoing: false,
            kind: "webchat".into(),
            text: "x".into(),
            media: None,
            quote: None,
            sent_at: 5,
            raw: "{}".into(),
        })
        .unwrap()
        .unwrap();
    let theirs = format!("/api/v1/threads/{id}/messages/{}/recall", incoming.seq);
    let (status, _) = call(&f, "POST", &theirs, None, true).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn reading_a_thread_sends_a_read_receipt_for_its_unread_messages() {
    let f = fixture();
    let id = thread_id(ThreadKind::User, "8");
    f.store
        .upsert_thread(
            &ThreadUpsert {
                kind: ThreadKind::User,
                remote_id: "8".into(),
                name: "C".into(),
                avatar: String::new(),
            },
            true,
        )
        .unwrap();
    for (i, live) in [(1, false), (2, true), (3, true)] {
        let remote = format!("m{i}");
        f.store
            .insert_message_with(
                &NewMessage {
                    thread_id: id.clone(),
                    remote_id: remote.clone(),
                    client_id: String::new(),
                    sender_id: "8".into(),
                    sender_name: "C".into(),
                    outgoing: false,
                    kind: "webchat".into(),
                    text: "x".into(),
                    media: None,
                    quote: None,
                    sent_at: i,
                    raw: json!({ "msgId": remote, "uidFrom": "8" }).to_string(),
                },
                live,
            )
            .unwrap();
    }
    let read = format!("/api/v1/threads/{id}/read");
    let (status, _) = call(&f, "POST", &read, None, true).await;
    assert_eq!(status, StatusCode::OK);
    // Only the two messages that were unread here; history was read elsewhere.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        f.seen.lock().unwrap().clone(),
        [("8".to_string(), vec!["m3".to_string(), "m2".to_string()])]
    );
    // Nothing unread any more: no second receipt.
    call(&f, "POST", &read, None, true).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(f.seen.lock().unwrap().len(), 1);
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

#[tokio::test]
async fn people_are_looked_up_once_friends_listed_and_messages_searched() {
    let f = fixture();
    // Before a session, only what the store knows.
    let (status, body) = call(&f, "GET", "/api/v1/people?ids=11", None, true).await;
    assert_eq!((status, body), (StatusCode::OK, json!([])));
    assert!(f.asked.lock().unwrap().is_empty());

    f.status.write().unwrap().session = SessionState::Valid;
    let (status, body) = call(
        &f,
        "GET",
        "/api/v1/people?ids=11,%20abc,12,9,11,",
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let names: Vec<_> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["uid"].as_str().unwrap(), p["name"].as_str().unwrap()))
        .collect();
    assert_eq!(names, [("11", "Người 11"), ("12", "Người 12"), ("9", "")]);
    assert_eq!(body[0]["avatar"], "https://s120-ava-talk.zadn.vn/11.jpg");
    assert_eq!(*f.asked.lock().unwrap(), [vec!["11", "12", "9"]]);
    // Asked about a moment ago, the unknown one included: not again.
    let (_, again) = call(&f, "GET", "/api/v1/people?ids=12,9", None, true).await;
    assert_eq!(again.as_array().unwrap().len(), 2);
    assert_eq!(f.asked.lock().unwrap().len(), 1);

    let friend = |uid: &str, name: &str| Person {
        uid: uid.into(),
        name: name.into(),
        ..Person::default()
    };
    f.store
        .set_friends(&[friend("21", "Lan"), friend("22", "An")])
        .unwrap();
    let (_, friends) = call(&f, "GET", "/api/v1/friends", None, true).await;
    let listed: Vec<_> = friends
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["name"].as_str().unwrap(), p["friend"].as_bool().unwrap()))
        .collect();
    assert_eq!(listed, [("An", true), ("Lan", true)]);

    let lan = ThreadUpsert {
        kind: ThreadKind::User,
        remote_id: "21".into(),
        name: "Lan".into(),
        avatar: String::new(),
    };
    f.store.upsert_thread(&lan, true).unwrap();
    let id = lan.id();
    for (i, text) in ["Ăn phở không?", "không", "Phở Hà Nội"].iter().enumerate() {
        f.store
            .insert_message(&NewMessage {
                thread_id: id.clone(),
                remote_id: format!("m{i}"),
                client_id: String::new(),
                sender_id: "21".into(),
                sender_name: "Lan".into(),
                outgoing: false,
                kind: "webchat".into(),
                text: (*text).into(),
                media: None,
                quote: None,
                sent_at: i as i64,
                raw: "{}".into(),
            })
            .unwrap();
    }
    let (status, found) = call(&f, "GET", "/api/v1/search?q=PHO", None, true).await;
    assert_eq!(status, StatusCode::OK);
    let hits: Vec<_> = found["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["text"].as_str().unwrap())
        .collect();
    assert_eq!(hits, ["Phở Hà Nội", "Ăn phở không?"]);
    let (_, none) = call(&f, "GET", "/api/v1/search?q=", None, true).await;
    assert_eq!(none["messages"], json!([]));
}

#[tokio::test]
async fn stickers_are_listed_searched_and_sent() {
    let f = fixture();
    let (status, packs) = call(&f, "GET", "/api/v1/stickers/packs", None, true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(packs["asked"], "Packs");
    let (_, pack) = call(&f, "GET", "/api/v1/stickers/packs/12815", None, true).await;
    assert_eq!(pack["asked"], "Pack(12815)");
    let (status, _) = call(&f, "GET", "/api/v1/stickers/packs/abc", None, true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (_, found) = call(
        &f,
        "GET",
        "/api/v1/stickers/search?q=%20haha%20",
        None,
        true,
    )
    .await;
    assert_eq!(found["asked"], "Search(\"haha\")");
    // Nothing to search: no question to Zalo.
    let (_, none) = call(&f, "GET", "/api/v1/stickers/search?q=%20", None, true).await;
    assert_eq!(none, json!([]));
    let (_, motion) = call(
        &f,
        "GET",
        "/api/v1/stickers/motion?ids=7,x,7,0,8",
        None,
        true,
    )
    .await;
    assert_eq!(motion["asked"], "Motion([7, 8])");
    let (_, nothing) = call(&f, "GET", "/api/v1/stickers/motion?ids=", None, true).await;
    assert_eq!(nothing, json!({}));

    let uri = format!(
        "/api/v1/threads/{}/stickers",
        thread_id(ThreadKind::Group, "77")
    );
    let (status, body) = call(
        &f,
        "POST",
        &uri,
        Some(json!({ "id": 21300, "cate_id": 10, "type": 7 })),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        (body["kind"].as_str(), body["text"].as_str()),
        (Some("chat.sticker"), Some("[Nhãn dán]"))
    );
    assert_eq!(body["media"]["kind"], "sticker");
    assert_eq!(body["media"]["sticker_id"], 21300);
    assert!(body["media"]["url"]
        .as_str()
        .unwrap()
        .ends_with("eid=21300&size=130"));
    let (status, _) = call(
        &f,
        "POST",
        &uri,
        Some(json!({ "id": 0, "cate_id": 0, "type": 7 })),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn group_members_and_shared_media() {
    let f = fixture();
    let group = thread_id(ThreadKind::Group, "55");
    let (status, body) = call(
        &f,
        "GET",
        &format!("/api/v1/threads/{group}/members"),
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], 12);
    assert_eq!(body["members"][0]["role"], "owner");
    assert_eq!(body["members"][1]["name"], "Người 2");
    // A 1-1 chat has no members to list.
    let user = thread_id(ThreadKind::User, "55");
    let (status, _) = call(
        &f,
        "GET",
        &format!("/api/v1/threads/{user}/members"),
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let lan = ThreadUpsert {
        kind: ThreadKind::User,
        remote_id: "55".into(),
        name: "Lan".into(),
        avatar: String::new(),
    };
    f.store.upsert_thread(&lan, true).unwrap();
    let shares = [
        ("webchat", "chào", None),
        ("chat.photo", "[Hình ảnh]", Some("image")),
        ("share.file", "[Tập tin] a.pdf", Some("file")),
        ("chat.video.msg", "[Video]", Some("video")),
        ("chat.recommended", "[Liên kết] Ví dụ", Some("link")),
    ];
    for (i, (kind, text, media)) in shares.iter().enumerate() {
        f.store
            .insert_message(&NewMessage {
                thread_id: user.clone(),
                remote_id: format!("s{i}"),
                client_id: String::new(),
                sender_id: "55".into(),
                sender_name: "Lan".into(),
                outgoing: false,
                kind: (*kind).into(),
                text: (*text).into(),
                media: media.map(|k| Media {
                    kind: k.into(),
                    url: format!("https://f.zdn.vn/{i}"),
                    ..Media::default()
                }),
                quote: None,
                sent_at: i as i64,
                raw: "{}".into(),
            })
            .unwrap();
    }
    let kinds = |body: &Value| -> Vec<String> {
        body.as_array()
            .unwrap()
            .iter()
            .map(|m| m["kind"].as_str().unwrap().to_string())
            .collect()
    };
    let shared = format!("/api/v1/threads/{user}/shared");
    let (status, media) = call(&f, "GET", &format!("{shared}?type=media"), None, true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(kinds(&media), ["chat.video.msg", "chat.photo"]);
    let (_, files) = call(&f, "GET", &format!("{shared}?type=file"), None, true).await;
    assert_eq!(kinds(&files), ["share.file"]);
    let (_, links) = call(&f, "GET", &format!("{shared}?type=link"), None, true).await;
    assert_eq!(kinds(&links), ["chat.recommended"]);
    let before = media[0]["seq"].as_i64().unwrap();
    let (_, older) = call(
        &f,
        "GET",
        &format!("{shared}?type=media&before={before}"),
        None,
        true,
    )
    .await;
    assert_eq!(kinds(&older), ["chat.photo"]);
    let (status, _) = call(&f, "GET", &format!("{shared}?type=all"), None, true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn friends_side_is_checked_then_handed_to_the_connector() {
    let f = fixture();
    let (status, _) = call(&f, "GET", "/api/v1/users/find?phone=123", None, true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, found) = call(
        &f,
        "GET",
        "/api/v1/users/find?phone=0912%20345%20678",
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(found["action"], "Find(\"0912 345 678\")");

    let (_, requests) = call(&f, "GET", "/api/v1/friends/requests", None, true).await;
    assert_eq!(requests["action"], "Requests");
    let send = |uid: &str| json!({ "uid": uid, "message": "  Chào bạn  " });
    let (status, _) = call(
        &f,
        "POST",
        "/api/v1/friends/requests",
        Some(send("12a")),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, sent) = call(
        &f,
        "POST",
        "/api/v1/friends/requests",
        Some(send("12")),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        sent["action"],
        "Send { uid: \"12\", message: \"Chào bạn\" }"
    );
    let (_, accepted) = call(&f, "POST", "/api/v1/friends/requests/34/accept", None, true).await;
    assert_eq!(accepted["action"], "Accept(\"34\")");
    let (_, rejected) = call(&f, "POST", "/api/v1/friends/requests/34/reject", None, true).await;
    assert_eq!(rejected["action"], "Reject(\"34\")");
    let (status, _) = call(&f, "POST", "/api/v1/friends/requests/x/accept", None, true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn messages_are_forwarded_and_deleted_for_me() {
    let f = fixture();
    let lan = ThreadUpsert {
        kind: ThreadKind::User,
        remote_id: "61".into(),
        name: "Lan".into(),
        avatar: String::new(),
    };
    f.store.upsert_thread(&lan, true).unwrap();
    let from = lan.id();
    let add = |remote: &str, kind: &str, text: &str, media: Option<Media>, raw: &str| {
        f.store
            .insert_message(&NewMessage {
                thread_id: from.clone(),
                remote_id: remote.into(),
                client_id: format!("c-{remote}"),
                sender_id: "61".into(),
                sender_name: "Lan".into(),
                outgoing: false,
                kind: kind.into(),
                text: text.into(),
                media,
                quote: None,
                sent_at: 1,
                raw: raw.into(),
            })
            .unwrap()
            .unwrap()
            .seq
    };
    let group = thread_id(ThreadKind::Group, "62");
    let other = thread_id(ThreadKind::User, "63");
    let forward = |seq: i64| format!("/api/v1/threads/{from}/messages/{seq}/forward");

    let text = add("m1", "webchat", "chào nhé", None, "{}");
    let to = json!({ "to": [group, other, group, "nowhere"] });
    let (status, body) = call(&f, "POST", &forward(text), Some(to), true).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["to"], group);
    assert_eq!(results[0]["message"]["text"], "chào nhé");
    assert_eq!(results[1]["message"]["thread_id"], other);

    let sticker = add(
        "m2",
        "chat.sticker",
        "[Nhãn dán]",
        Some(Media {
            kind: "sticker".into(),
            url: "https://api.zaloapp.com/api/emoticon/sticker/webpc?eid=21300&size=130".into(),
            ..Media::default()
        }),
        r#"{"content":{"id":21300,"catId":10,"type":7}}"#,
    );
    let (_, body) = call(
        &f,
        "POST",
        &forward(sticker),
        Some(json!({ "to": [group] })),
        true,
    )
    .await;
    assert_eq!(body["results"][0]["message"]["kind"], "chat.sticker");

    let url = "https://f.zdn.vn/bao-cao.pdf";
    f.media
        .insert(url, b"%PDF-1.4 tiny", "application/pdf")
        .await
        .unwrap();
    let file = add(
        "m3",
        "share.file",
        "[Tập tin] bao-cao.pdf",
        Some(Media {
            kind: "file".into(),
            url: url.into(),
            name: "bao-cao.pdf".into(),
            size: 13,
            ..Media::default()
        }),
        "{}",
    );
    let (status, body) = call(
        &f,
        "POST",
        &forward(file),
        Some(json!({ "to": [group] })),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let upload = &body["results"][0]["upload"];
    assert_eq!(
        (upload["name"].as_str(), upload["kind"].as_str()),
        (Some("bao-cao.pdf"), Some("file"))
    );
    assert_eq!(upload["thread_id"], group);

    let place = add("m4", "chat.location.new", "[Vị trí]", None, "{}");
    let (status, _) = call(
        &f,
        "POST",
        &forward(place),
        Some(json!({ "to": [group] })),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = call(&f, "POST", &forward(text), Some(json!({ "to": [] })), true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Deleted for me: gone here, and Zalo was told whose message it was.
    let uri = format!("/api/v1/threads/{from}/messages/{text}");
    let (status, _) = call(&f, "DELETE", &uri, None, true).await;
    assert_eq!(status, StatusCode::OK);
    assert!(f.store.message(text).unwrap().is_none());
    assert_eq!(
        *f.deleted.lock().unwrap(),
        [("m1".to_string(), "61".to_string())]
    );
    let (status, _) = call(&f, "DELETE", &uri, None, true).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // A message Zalo never confirmed is only deleted here.
    let local = add("local:9", "webchat", "chưa gửi", None, "{}");
    let (status, _) = call(
        &f,
        "DELETE",
        &format!("/api/v1/threads/{from}/messages/{local}"),
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(f.deleted.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn presence_of_1_1_chats_only() {
    let f = fixture();
    let user = thread_id(ThreadKind::User, "1234");
    let (status, body) = call(
        &f,
        "GET",
        &format!("/api/v1/threads/{user}/presence"),
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "visible": true, "at": 1234 }));
    let group = thread_id(ThreadKind::Group, "1234");
    let (status, _) = call(
        &f,
        "GET",
        &format!("/api/v1/threads/{group}/presence"),
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn mentions_fit_the_trimmed_text_and_stay_in_groups() {
    let f = fixture();
    let group = thread_id(ThreadKind::Group, "70");
    // "  @Lan ơi" is trimmed to "@Lan ơi": the mention moves back by two.
    let body = json!({
        "text": "  @Lan ơi",
        "mentions": [
            { "uid": "5", "pos": 2, "len": 4 },
            { "uid": "x", "pos": 2, "len": 4 },
            { "uid": "6", "pos": 8, "len": 9 },
            { "uid": "-1", "pos": 0, "len": 1 }
        ]
    });
    let (status, sent) = call(
        &f,
        "POST",
        &format!("/api/v1/threads/{group}/messages"),
        Some(body),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    assert_eq!(sent["text"], "@Lan ơi");
    assert_eq!(
        sent["mentions"],
        json!([{ "uid": "5", "pos": 0, "len": 4 }])
    );
    // 1-1 chats take no mentions.
    let user = thread_id(ThreadKind::User, "70");
    let body = json!({ "text": "@Lan", "mentions": [{ "uid": "5", "pos": 0, "len": 4 }] });
    let (_, sent) = call(
        &f,
        "POST",
        &format!("/api/v1/threads/{user}/messages"),
        Some(body),
        true,
    )
    .await;
    assert_eq!(sent["mentions"], json!([]));
}

#[tokio::test]
async fn quick_messages_are_kept_here_beside_zalos() {
    let f = fixture();
    let (status, list) = call(&f, "GET", "/api/v1/quick-messages", None, true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        list,
        json!([{ "id": "zalo:7", "keyword": "chao", "text": "Xin chào",
        "photo": "", "updated_at": 1, "zalo": true }])
    );
    // Kept by zetachat: a leading "/" and spaces are dropped.
    let (status, list) = call(
        &f,
        "POST",
        "/api/v1/quick-messages",
        Some(json!({ "keyword": " /diachi ", "text": " 12 Lê Lợi " })),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list.as_array().unwrap().len(), 2);
    let mine = list
        .as_array()
        .unwrap()
        .iter()
        .find(|q| q["zalo"] == false)
        .unwrap()
        .clone();
    assert_eq!(
        (mine["keyword"].as_str(), mine["text"].as_str()),
        (Some("diachi"), Some("12 Lê Lợi"))
    );
    // A keyword Zalo has already is taken; Zalo refuses more of its own.
    let (status, _) = call(
        &f,
        "POST",
        "/api/v1/quick-messages",
        Some(json!({ "keyword": "chao", "text": "x" })),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = call(
        &f,
        "POST",
        "/api/v1/quick-messages",
        Some(json!({ "keyword": "moi", "text": "x", "zalo": true })),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    let id = mine["id"].as_str().unwrap();
    let (status, list) = call(
        &f,
        "PUT",
        &format!("/api/v1/quick-messages/{id}"),
        Some(json!({ "keyword": "diachi", "text": "34 Lê Lợi" })),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(list.to_string().contains("34 Lê Lợi"));
    let (status, list) = call(
        &f,
        "DELETE",
        &format!("/api/v1/quick-messages/{id}"),
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);
    let (status, _) = call(&f, "DELETE", "/api/v1/quick-messages/nowhere:1", None, true).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn quick_messages_need_a_keyword_and_a_text() {
    let f = fixture();
    for bad in [
        json!({ "keyword": "hai từ", "text": "x" }),
        json!({ "keyword": "/", "text": "x" }),
        json!({ "keyword": "ok", "text": "   " }),
        json!({ "keyword": "ok", "text": "x".repeat(2001) }),
    ] {
        let (status, _) = call(&f, "POST", "/api/v1/quick-messages", Some(bad), true).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}

#[tokio::test]
async fn bank_cards_and_vietqr_codes() {
    let f = fixture();
    let user = thread_id(ThreadKind::User, "70");
    let uri = format!("/api/v1/threads/{user}/bank-card");
    let body = json!({ "bin": "970436", "number": "0123 456 789", "holder": "Nguyễn Văn A" });
    let (status, sent) = call(&f, "POST", &uri, Some(body), true).await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    assert_eq!(sent["media"]["kind"], "bank");
    assert_eq!(sent["media"]["bank"]["number"], "0123456789");
    for bad in [
        json!({ "bin": "97043", "number": "0123456789" }),
        json!({ "bin": "970436", "number": "12a45678" }),
        json!({ "bin": "970436", "number": "123" }),
    ] {
        let (status, _) = call(&f, "POST", &uri, Some(bad), true).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    // Only bank cards can be read as such.
    let seq = sent["seq"].as_i64().unwrap();
    let (status, _) = call(
        &f,
        "POST",
        &format!("/api/v1/threads/{user}/messages/{seq}/bank"),
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let resp = f
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/vietqr.svg?bin=970436&number=0123456789&amount=50000")
                .header("authorization", format!("Bearer {}", f.token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["content-type"], "image/svg+xml");
    let (status, _) = call(
        &f,
        "GET",
        "/api/v1/vietqr.svg?bin=1&number=0123456789",
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(
        &f,
        "GET",
        "/api/v1/vietqr.svg?bin=970436&number=0123456789",
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn contact_cards_are_sent_for_user_ids_only() {
    let f = fixture();
    let user = thread_id(ThreadKind::User, "70");
    let uri = format!("/api/v1/threads/{user}/contact-card");
    let (status, sent) = call(&f, "POST", &uri, Some(json!({ "uid": " 5 " })), true).await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    assert_eq!(sent["kind"], "chat.recommended");
    assert_eq!(sent["media"]["kind"], "contact");
    assert_eq!(sent["media"]["uid"], "5");
    assert_eq!(sent["media"]["url"], "https://qr-talk.zdn.vn/q.jpg");
    let (status, _) = call(&f, "POST", &uri, Some(json!({ "uid": "5; drop" })), true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // A sent card can be forwarded: it is sent again for the same person.
    let seq = sent["seq"].as_i64().unwrap();
    let group = thread_id(ThreadKind::Group, "71");
    let (status, out) = call(
        &f,
        "POST",
        &format!("/api/v1/threads/{user}/messages/{seq}/forward"),
        Some(json!({ "to": [group] })),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["results"][0]["message"]["media"]["uid"], "5");
}

#[tokio::test]
async fn styled_and_marked_texts() {
    let f = fixture();
    let user = thread_id(ThreadKind::User, "70");
    // "  đậm thường" is trimmed: runs move back by two, codes Zalo does not
    // know are dropped, a run past the end is cut.
    let body = json!({
        "text": "  đậm thường",
        "styles": [
            { "start": 2, "len": 3, "st": "b" },
            { "start": 2, "len": 3, "st": "blink" },
            { "start": 6, "len": 99, "st": "c_db342e" }
        ],
        "urgency": 2
    });
    let (status, sent) = call(
        &f,
        "POST",
        &format!("/api/v1/threads/{user}/messages"),
        Some(body),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    assert_eq!(sent["text"], "đậm thường");
    assert_eq!(
        sent["styles"],
        json!([
            { "start": 0, "len": 3, "st": "b" },
            { "start": 4, "len": 6, "st": "c_db342e" }
        ])
    );
    assert_eq!(sent["urgency"], 2);
    // Read back from the store as clients load the thread.
    let (_, page) = call(
        &f,
        "GET",
        &format!("/api/v1/threads/{user}/messages"),
        None,
        true,
    )
    .await;
    let last = page
        .as_array()
        .and_then(|list| list.iter().find(|m| m["seq"] == sent["seq"]))
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(last["styles"], sent["styles"]);
    assert_eq!(last["urgency"], 2);

    let (status, _) = call(
        &f,
        "POST",
        &format!("/api/v1/threads/{user}/messages"),
        Some(json!({ "text": "a", "urgency": 3 })),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (_, plain) = call(
        &f,
        "POST",
        &format!("/api/v1/threads/{user}/messages"),
        Some(json!({ "text": "a" })),
        true,
    )
    .await;
    assert_eq!(
        (plain["styles"].clone(), plain["urgency"].clone()),
        (json!([]), json!(0))
    );
}

#[tokio::test]
async fn groups_need_two_friends_or_more() {
    let f = fixture();
    let make = |members: Value| json!({ "name": " Đi phượt ", "members": members });
    let (status, _) = call(
        &f,
        "POST",
        "/api/v1/groups",
        Some(make(json!(["1", "1"]))),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(
        &f,
        "POST",
        "/api/v1/groups",
        Some(make(json!(["1", "x"]))),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, made) = call(
        &f,
        "POST",
        "/api/v1/groups",
        Some(make(json!(["1", "2"]))),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{made}");
    assert_eq!(made["thread"]["id"], "zalo:g:777");
    assert_eq!(made["thread"]["name"], "Đi phượt");
    assert_eq!(made["thread"]["members"], 3);
}

#[tokio::test]
async fn chats_are_renamed_by_alias_or_group_name() {
    let f = fixture();
    for (kind, remote, name) in [
        (ThreadKind::User, "80", "Hân"),
        (ThreadKind::Group, "81", "Nhà"),
    ] {
        f.store
            .upsert_thread(
                &ThreadUpsert {
                    kind,
                    remote_id: remote.into(),
                    name: name.into(),
                    avatar: String::new(),
                },
                true,
            )
            .unwrap();
    }
    let user = thread_id(ThreadKind::User, "80");
    let rename = |id: &str| format!("/api/v1/threads/{id}/name");
    let (status, t) = call(
        &f,
        "PUT",
        &rename(&user),
        Some(json!({ "name": " Hân lớp Văn " })),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{t}");
    assert_eq!(
        (t["name"].as_str(), t["alias"].as_str()),
        (Some("Hân"), Some("Hân lớp Văn"))
    );
    // Empty: the alias goes, the name stays.
    let (_, t) = call(&f, "PUT", &rename(&user), Some(json!({ "name": "" })), true).await;
    assert_eq!(t["alias"], "");
    let group = thread_id(ThreadKind::Group, "81");
    let (status, _) = call(
        &f,
        "PUT",
        &rename(&group),
        Some(json!({ "name": "  " })),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (_, t) = call(
        &f,
        "PUT",
        &rename(&group),
        Some(json!({ "name": "Nhà mình" })),
        true,
    )
    .await;
    assert_eq!(t["name"], "Nhà mình");
}
