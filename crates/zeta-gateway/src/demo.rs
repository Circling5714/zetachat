//! Demo connector: sample conversations and an echo reply, no Zalo involved.
//! For trying the web client and taking screenshots (`serve --demo`).

use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde_json::json;
use tokio::sync::mpsc;

use crate::events::EventBus;
use crate::model::{now_ms, thread_id, Message, NewMessage, ThreadKind, ThreadUpsert};
use crate::store::Store;
use crate::zalo::{LoginState, SessionState, ZaloCommand, ZaloHandle, ZaloStatus};

/// (outgoing, sender name, text, minutes ago)
type Sample<'a> = (bool, &'a str, &'a str, i64);

fn add_thread(store: &Store, kind: ThreadKind, remote: &str, name: &str, messages: &[Sample<'_>]) {
    let thread = ThreadUpsert {
        kind,
        remote_id: remote.into(),
        name: name.into(),
        avatar: String::new(),
    };
    if store.upsert_thread(&thread, true).is_err() {
        return;
    }
    for (i, (outgoing, sender, text, minutes_ago)) in messages.iter().enumerate() {
        let _ = store.insert_message(&NewMessage {
            thread_id: thread.id(),
            remote_id: format!("demo-{remote}-{i}"),
            client_id: String::new(),
            // In groups each sample sender needs its own id so names show per sender.
            sender_id: if *outgoing {
                "me".into()
            } else {
                format!("{remote}-{sender}")
            },
            sender_name: sender.to_string(),
            outgoing: *outgoing,
            kind: "webchat".into(),
            text: text.to_string(),
            sent_at: now_ms() - minutes_ago * 60_000,
            raw: "{}".into(),
        });
    }
}

/// Fill an empty store with a few conversations.
fn seed(store: &Store) {
    if store
        .threads(true, 1, 0)
        .map(|t| !t.is_empty())
        .unwrap_or(true)
    {
        return;
    }
    add_thread(
        store,
        ThreadKind::User,
        "1001",
        "Lan Anh",
        &[
            (false, "Lan Anh", "Chiều nay anh rảnh không?", 95),
            (true, "", "Rảnh em, có gì không?", 90),
            (false, "Lan Anh", "Đi cà phê nhé, 5 giờ chỗ cũ ☕", 88),
        ],
    );
    add_thread(
        store,
        ThreadKind::Group,
        "2001",
        "Gia đình",
        &[
            (false, "Mẹ", "Cuối tuần về ăn cơm nha các con", 300),
            (false, "Minh", "Dạ con về ạ", 280),
            (true, "", "Con cũng về ạ, con mang trái cây", 270),
            (false, "Mẹ", "[Hình ảnh] Mâm cơm hôm nay", 30),
        ],
    );
    add_thread(
        store,
        ThreadKind::User,
        "1002",
        "Minh Tuấn",
        &[
            (false, "Minh Tuấn", "Em gửi anh báo cáo tháng rồi nhé", 1500),
            (true, "", "Ok em, cảm ơn nhiều", 1490),
        ],
    );
    add_thread(store, ThreadKind::User, "1003", "Hoàng Nam", &[]);
    // Read the older threads so only some show unread badges.
    let _ = store.mark_read(&thread_id(ThreadKind::User, "1002"));
}

fn store_and_publish(
    store: &Store,
    bus: &EventBus,
    kind: ThreadKind,
    remote_id: &str,
    outgoing: bool,
    sender_name: &str,
    text: &str,
) -> Option<Message> {
    let thread = ThreadUpsert {
        kind,
        remote_id: remote_id.into(),
        name: String::new(),
        avatar: String::new(),
    };
    store.upsert_thread(&thread, false).ok()?;
    let now = now_ms();
    let message = store
        .insert_message(&NewMessage {
            thread_id: thread.id(),
            remote_id: format!("demo-{now}-{}", if outgoing { "out" } else { "in" }),
            client_id: String::new(),
            sender_id: if outgoing {
                "me".into()
            } else {
                remote_id.into()
            },
            sender_name: sender_name.into(),
            outgoing,
            kind: "webchat".into(),
            text: text.into(),
            sent_at: now,
            raw: "{}".into(),
        })
        .ok()??;
    let snapshot = store.thread(&message.thread_id).ok().flatten();
    let _ = bus.publish("message", json!({ "message": message, "thread": snapshot }));
    Some(message)
}

/// Start the demo connector.
pub fn spawn(store: Arc<Store>, bus: EventBus) -> ZaloHandle {
    seed(&store);
    let status = Arc::new(RwLock::new(ZaloStatus {
        session: SessionState::Valid,
        realtime: true,
        display_name: "Demo".into(),
        ..ZaloStatus::default()
    }));
    let (commands, mut rx) = mpsc::channel(32);
    let shared = status.clone();
    tokio::spawn(async move {
        while let Some(cmd) = rx.recv().await {
            match cmd {
                ZaloCommand::SendText {
                    kind,
                    remote_id,
                    text,
                    reply,
                } => {
                    let sent = store_and_publish(&store, &bus, kind, &remote_id, true, "", &text);
                    let ok = sent.is_some();
                    let _ = reply.send(sent.ok_or_else(|| "demo: không lưu được tin".to_string()));
                    if ok && kind == ThreadKind::User {
                        let (store, bus) = (store.clone(), bus.clone());
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_millis(1200)).await;
                            let name = store
                                .thread(&thread_id(kind, &remote_id))
                                .ok()
                                .flatten()
                                .map(|t| t.name)
                                .unwrap_or_default();
                            let echo = format!("Đã nhận: {text}");
                            store_and_publish(&store, &bus, kind, &remote_id, false, &name, &echo);
                        });
                    }
                }
                ZaloCommand::StartLogin { reply } => {
                    let state = LoginState::WaitingScan {
                        qr: "https://zetachat.invalid/demo-login".into(),
                        expires_at: now_ms() + 180_000,
                    };
                    shared.write().unwrap_or_else(|e| e.into_inner()).login = state.clone();
                    let snapshot = shared.read().unwrap_or_else(|e| e.into_inner()).clone();
                    let _ = bus.publish("status", json!(snapshot));
                    let _ = reply.send(state);
                }
            }
        }
    });
    ZaloHandle { commands, status }
}
