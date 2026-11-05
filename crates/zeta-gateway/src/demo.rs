//! Demo connector: sample conversations and an echo reply, no Zalo involved.
//! For trying the web client and taking screenshots (`serve --demo`).

use std::sync::Arc;
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::json;
use tokio::sync::mpsc;

use crate::events::EventBus;
use crate::media::RECALLED_TEXT;
use crate::media_cache::{MediaCache, LOCAL_URL_PREFIX};
use crate::model::{
    now_ms, thread_id, Media, Message, NewMessage, NewReaction, Quote, ThreadKind, ThreadUpsert,
};
use crate::store::Store;
use crate::uploads::UploadJob;
use crate::zalo::{Attachment, LoginState, SessionState, ZaloCommand, ZaloHandle, ZaloStatus};

/// Sample pictures are inline SVG, so the demo needs no network at all.
const PHOTO_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="800" height="600" viewBox="0 0 800 600"><defs><linearGradient id="g" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="#fde68a"/><stop offset="1" stop-color="#f97316"/></linearGradient></defs><rect width="800" height="600" fill="url(#g)"/><ellipse cx="400" cy="330" rx="310" ry="205" fill="#7c2d12" opacity="0.85"/><circle cx="290" cy="270" r="84" fill="#fff"/><circle cx="290" cy="270" r="62" fill="#16a34a"/><circle cx="510" cy="270" r="84" fill="#fff"/><circle cx="510" cy="270" r="62" fill="#dc2626"/><circle cx="400" cy="420" r="74" fill="#fff"/><circle cx="400" cy="420" r="54" fill="#facc15"/></svg>"##;

const POSTER_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="1280" height="720" viewBox="0 0 1280 720"><defs><linearGradient id="s" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#38bdf8"/><stop offset="1" stop-color="#e0f2fe"/></linearGradient></defs><rect width="1280" height="720" fill="url(#s)"/><circle cx="1010" cy="170" r="80" fill="#fde047"/><path d="M0 560 L300 300 L520 520 L760 260 L1280 600 L1280 720 L0 720 Z" fill="#15803d"/><path d="M0 640 L400 480 L800 620 L1280 520 L1280 720 L0 720 Z" fill="#166534"/></svg>"##;

const STICKER_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="240" height="240" viewBox="0 0 240 240"><circle cx="120" cy="124" r="100" fill="#facc15" stroke="#a16207" stroke-width="6"/><circle cx="85" cy="102" r="13" fill="#422006"/><circle cx="155" cy="102" r="13" fill="#422006"/><path d="M68 148 Q120 200 172 148" fill="none" stroke="#422006" stroke-width="11" stroke-linecap="round"/><circle cx="62" cy="140" r="14" fill="#fb7185" opacity="0.6"/><circle cx="178" cy="140" r="14" fill="#fb7185" opacity="0.6"/></svg>"##;

const LINK_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="640" height="320" viewBox="0 0 640 320"><rect width="640" height="320" fill="#1e293b"/><circle cx="320" cy="160" r="96" fill="none" stroke="#f97316" stroke-width="22"/><circle cx="320" cy="160" r="36" fill="#f97316"/></svg>"##;

fn svg_data(svg: &str) -> String {
    format!("data:image/svg+xml;base64,{}", STANDARD.encode(svg))
}

/// One sample message.
struct Sample {
    outgoing: bool,
    sender: &'static str,
    kind: &'static str,
    text: &'static str,
    minutes_ago: i64,
    media: Option<Media>,
    quote: Option<Quote>,
}

fn say(outgoing: bool, sender: &'static str, text: &'static str, minutes_ago: i64) -> Sample {
    Sample {
        outgoing,
        sender,
        kind: "webchat",
        text,
        minutes_ago,
        media: None,
        quote: None,
    }
}

/// An outgoing reply quoting `quoted` by `author`.
fn reply(text: &'static str, minutes_ago: i64, author: &str, quoted: &str) -> Sample {
    Sample {
        quote: Some(Quote {
            sender_name: author.into(),
            text: quoted.into(),
            remote_id: String::new(),
        }),
        ..say(true, "", text, minutes_ago)
    }
}

/// A message its sender recalled.
fn recalled(sender: &'static str, minutes_ago: i64) -> Sample {
    Sample {
        kind: "chat.undo",
        ..say(false, sender, RECALLED_TEXT, minutes_ago)
    }
}

fn share(
    outgoing: bool,
    sender: &'static str,
    kind: &'static str,
    text: &'static str,
    minutes_ago: i64,
    media: Media,
) -> Sample {
    Sample {
        outgoing,
        sender,
        kind,
        text,
        minutes_ago,
        media: Some(media),
        quote: None,
    }
}

fn add_thread(store: &Store, kind: ThreadKind, remote: &str, name: &str, messages: Vec<Sample>) {
    let thread = ThreadUpsert {
        kind,
        remote_id: remote.into(),
        name: name.into(),
        avatar: String::new(),
    };
    if store.upsert_thread(&thread, true).is_err() {
        return;
    }
    for (i, sample) in messages.into_iter().enumerate() {
        let _ = store.insert_message(&NewMessage {
            thread_id: thread.id(),
            remote_id: format!("demo-{remote}-{i}"),
            client_id: String::new(),
            // In groups each sample sender needs its own id so names show per sender.
            sender_id: if sample.outgoing {
                "me".into()
            } else {
                format!("{remote}-{}", sample.sender)
            },
            sender_name: sample.sender.to_string(),
            outgoing: sample.outgoing,
            kind: sample.kind.into(),
            text: sample.text.to_string(),
            media: sample.media,
            quote: sample.quote,
            sent_at: now_ms() - sample.minutes_ago * 60_000,
            raw: "{}".into(),
        });
    }
}

/// Reactions to sample message `index` of a thread: `(user, icon)` in order.
fn add_reactions(
    store: &Store,
    kind: ThreadKind,
    remote: &str,
    index: usize,
    given: &[(&str, &str)],
) {
    for (i, (user, icon)) in given.iter().enumerate() {
        let _ = store.add_reaction(&NewReaction {
            thread_id: thread_id(kind, remote),
            action_id: format!("demo-r-{remote}-{index}-{i}"),
            client_id: String::new(),
            targets: vec![(format!("demo-{remote}-{index}"), String::new())],
            user_id: user.to_string(),
            mine: *user == "me",
            icon: icon.to_string(),
            sent_at: now_ms() - 60_000 + i as i64,
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
        vec![
            say(false, "Lan Anh", "Chiều nay anh rảnh không?", 95),
            say(true, "", "Rảnh em, có gì không?", 90),
            say(false, "Lan Anh", "Đi cà phê nhé, 5 giờ chỗ cũ ☕", 88),
            share(
                false,
                "Lan Anh",
                "chat.sticker",
                "[Nhãn dán]",
                87,
                Media {
                    kind: "sticker".into(),
                    url: svg_data(STICKER_SVG),
                    ..Media::default()
                },
            ),
            recalled("Lan Anh", 86),
            reply(
                "Ok em, 5 giờ anh qua",
                85,
                "Lan Anh",
                "Đi cà phê nhé, 5 giờ chỗ cũ ☕",
            ),
        ],
    );
    add_thread(
        store,
        ThreadKind::Group,
        "2001",
        "Gia đình",
        vec![
            say(false, "Mẹ", "Cuối tuần về ăn cơm nha các con", 300),
            say(false, "Minh", "Dạ con về ạ", 280),
            say(true, "", "Con cũng về ạ, con mang trái cây", 270),
            share(
                false,
                "Mẹ",
                "chat.photo",
                "[Hình ảnh] Mâm cơm hôm nay",
                30,
                Media {
                    kind: "image".into(),
                    url: svg_data(PHOTO_SVG),
                    width: 800,
                    height: 600,
                    description: "Mâm cơm hôm nay".into(),
                    ..Media::default()
                },
            ),
            share(
                false,
                "Minh",
                "chat.video.msg",
                "[Video]",
                25,
                Media {
                    kind: "video".into(),
                    // Not a Zalo URL: the gateway refuses it, the demo plays nothing.
                    url: "https://zetachat.invalid/demo.mp4".into(),
                    thumb: svg_data(POSTER_SVG),
                    width: 1280,
                    height: 720,
                    size: 8_400_000,
                    duration_ms: 42_000,
                    ..Media::default()
                },
            ),
        ],
    );
    add_thread(
        store,
        ThreadKind::User,
        "1002",
        "Minh Tuấn",
        vec![
            say(false, "Minh Tuấn", "Em gửi anh báo cáo tháng rồi nhé", 1500),
            share(
                false,
                "Minh Tuấn",
                "share.file",
                "[Tập tin] Bao-cao-thang-10.pdf",
                1499,
                Media {
                    kind: "file".into(),
                    url: "https://zetachat.invalid/bao-cao.pdf".into(),
                    name: "Bao-cao-thang-10.pdf".into(),
                    size: 1_250_000,
                    ..Media::default()
                },
            ),
            say(
                false,
                "Minh Tuấn",
                "Bản online ở đây: https://example.com/bao-cao",
                1498,
            ),
            say(true, "", "Ok em, cảm ơn nhiều", 1490),
            share(
                true,
                "",
                "chat.recommended",
                "[Liên kết] Rust Programming Language",
                1489,
                Media {
                    kind: "link".into(),
                    url: "https://www.rust-lang.org/".into(),
                    thumb: svg_data(LINK_SVG),
                    name: "Rust Programming Language".into(),
                    description:
                        "A language empowering everyone to build reliable and efficient software."
                            .into(),
                    ..Media::default()
                },
            ),
        ],
    );
    add_thread(store, ThreadKind::User, "1003", "Hoàng Nam", Vec::new());
    add_reactions(store, ThreadKind::User, "1001", 2, &[("me", "/-strong")]);
    add_reactions(store, ThreadKind::User, "1001", 5, &[("1001", "/-heart")]);
    add_reactions(
        store,
        ThreadKind::Group,
        "2001",
        3,
        &[
            ("2001-Minh", "/-heart"),
            ("2001-Mẹ", "/-heart"),
            ("me", "/-heart"),
            ("2001-Minh", ":o"),
        ],
    );
    // Read the older threads so only some show unread badges.
    let _ = store.mark_read(&thread_id(ThreadKind::User, "1002"));
}

/// What a demo message carries besides its text.
struct Content<'a> {
    kind: &'a str,
    text: &'a str,
    media: Option<Media>,
    quote: Option<Quote>,
}

fn store_and_publish(
    store: &Store,
    bus: &EventBus,
    kind: ThreadKind,
    remote_id: &str,
    outgoing: bool,
    sender_name: &str,
    content: Content<'_>,
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
            kind: content.kind.into(),
            text: content.text.into(),
            media: content.media,
            quote: content.quote,
            sent_at: now,
            raw: "{}".into(),
        })
        .ok()??;
    let snapshot = store.thread(&message.thread_id).ok().flatten();
    let _ = bus.publish("message", json!({ "message": message, "thread": snapshot }));
    Some(message)
}

fn text(text: &str) -> Content<'_> {
    Content {
        kind: "webchat",
        text,
        media: None,
        quote: None,
    }
}

/// A demo "send": the file stays in the gateway's cache under a local URL.
async fn keep_attachment(media: &MediaCache, a: &Attachment) -> Result<(Media, String), String> {
    let url = format!("{LOCAL_URL_PREFIX}{}/{}", now_ms(), a.name);
    media
        .insert_file(&url, &a.path, &a.mime)
        .await
        .map_err(|e| e.to_string())?;
    let size = a.size;
    let caption = a.caption.trim().to_string();
    Ok(if a.photo {
        let summary = if caption.is_empty() {
            "[Hình ảnh]".to_string()
        } else {
            format!("[Hình ảnh] {caption}")
        };
        let media = Media {
            kind: "image".into(),
            url,
            width: a.width,
            height: a.height,
            description: caption,
            size,
            ..Media::default()
        };
        (media, summary)
    } else {
        let media = Media {
            kind: "file".into(),
            url,
            name: a.name.clone(),
            size,
            ..Media::default()
        };
        (media, format!("[Tập tin] {}", a.name))
    })
}

/// Pretend to upload, with progress, then store the message.
async fn demo_upload(
    store: &Store,
    bus: &EventBus,
    media: &MediaCache,
    kind: ThreadKind,
    remote_id: &str,
    a: &Attachment,
    job: &UploadJob,
) {
    if !job.start() {
        return;
    }
    let steps = 12;
    for step in 1..=steps {
        tokio::select! {
            () = tokio::time::sleep(Duration::from_millis(250)) => {}
            () = job.cancelled() => return,
        }
        job.progress(a.size * step / steps);
    }
    if !job.begin_finishing() {
        return;
    }
    let result = match keep_attachment(media, a).await {
        Ok((found, summary)) => {
            let content = Content {
                kind: if a.photo { "chat.photo" } else { "share.file" },
                text: &summary,
                media: Some(found),
                quote: None,
            };
            store_and_publish(store, bus, kind, remote_id, true, "", content)
                .ok_or_else(|| "demo: không lưu được tin".to_string())
        }
        Err(e) => Err(e),
    };
    job.finish(result);
}

/// Start the demo connector.
pub fn spawn(store: Arc<Store>, bus: EventBus, media: Arc<MediaCache>) -> ZaloHandle {
    seed(&store);
    let (commands, mut rx) = mpsc::channel(32);
    let handle = ZaloHandle::new(commands);
    *handle.status.write().unwrap_or_else(|e| e.into_inner()) = ZaloStatus {
        session: SessionState::Valid,
        realtime: true,
        display_name: "Demo".into(),
        ..ZaloStatus::default()
    };
    let shared = handle.status.clone();
    tokio::spawn(async move {
        while let Some(cmd) = rx.recv().await {
            match cmd {
                ZaloCommand::SendText {
                    kind,
                    remote_id,
                    text,
                    reply_to,
                    reply,
                } => {
                    let quote = reply_to.map(|r| Quote {
                        sender_name: if r.message.outgoing {
                            "Demo".into()
                        } else {
                            r.message.sender_name
                        },
                        text: r.message.text,
                        remote_id: r.message.remote_id,
                    });
                    let sent = store_and_publish(
                        &store,
                        &bus,
                        kind,
                        &remote_id,
                        true,
                        "",
                        Content {
                            quote,
                            ..self::text(&text)
                        },
                    );
                    let ok = sent.is_some();
                    let _ = reply.send(sent.ok_or_else(|| "demo: không lưu được tin".to_string()));
                    if ok && kind == ThreadKind::User {
                        let (store, bus) = (store.clone(), bus.clone());
                        tokio::spawn(async move {
                            let thread = thread_id(kind, &remote_id);
                            let name = store
                                .thread(&thread)
                                .ok()
                                .flatten()
                                .map(|t| t.name)
                                .unwrap_or_default();
                            // The other side "types" before answering.
                            tokio::time::sleep(Duration::from_millis(300)).await;
                            bus.notify(
                                "typing",
                                json!({ "thread_id": thread, "user_id": remote_id, "name": name }),
                            );
                            tokio::time::sleep(Duration::from_millis(1500)).await;
                            let echo = format!("Đã nhận: {text}");
                            store_and_publish(
                                &store,
                                &bus,
                                kind,
                                &remote_id,
                                false,
                                &name,
                                self::text(&echo),
                            );
                        });
                    }
                }
                ZaloCommand::SendAttachment {
                    kind,
                    remote_id,
                    attachment,
                    job,
                } => {
                    let (store, bus, media) = (store.clone(), bus.clone(), media.clone());
                    tokio::spawn(async move {
                        demo_upload(&store, &bus, &media, kind, &remote_id, &attachment, &job)
                            .await;
                        job.remove_file().await;
                    });
                }
                ZaloCommand::Recall {
                    kind,
                    remote_id,
                    message_id,
                    message_client_id,
                    reply,
                } => {
                    let thread = thread_id(kind, &remote_id);
                    let recalled = store
                        .recall_message(&thread, &message_id, &message_client_id)
                        .map_err(|e| e.to_string());
                    if let Ok(Some(message)) = &recalled {
                        let snapshot = store.thread(&thread).ok().flatten();
                        let _ = bus.publish(
                            "message_updated",
                            json!({ "message": message, "thread": snapshot }),
                        );
                    }
                    let _ = reply.send(recalled);
                }
                ZaloCommand::React {
                    kind,
                    remote_id,
                    message_id,
                    message_client_id,
                    icon,
                    reply,
                } => {
                    let reaction = NewReaction {
                        thread_id: thread_id(kind, &remote_id),
                        action_id: format!("demo-{:016x}", rand::random::<u64>()),
                        client_id: String::new(),
                        targets: vec![(message_id, message_client_id)],
                        user_id: "me".into(),
                        mine: true,
                        icon,
                        sent_at: now_ms(),
                    };
                    let changed = store
                        .add_reaction(&reaction)
                        .ok()
                        .and_then(|changed| changed.into_iter().next());
                    if let Some(message) = &changed {
                        let thread = store.thread(&message.thread_id).ok().flatten();
                        let _ = bus.publish(
                            "message_updated",
                            json!({ "message": message, "thread": thread }),
                        );
                    }
                    let _ = reply.send(Ok(changed));
                }
                // Nobody to tell in the demo.
                ZaloCommand::MarkSeen { .. } | ZaloCommand::Typing { .. } => {}
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
    handle
}
