//! Demo connector: sample conversations and an echo reply, no Zalo involved.
//! For trying the web client and taking screenshots (`serve --demo`).

use std::sync::Arc;
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::events::EventBus;
use crate::media::RECALLED_TEXT;
use crate::media_cache::{MediaCache, LOCAL_URL_PREFIX};
use crate::model::{
    now_ms, thread_id, GroupMembers, Media, Member, Message, NewMessage, NewReaction, Person,
    Quote, ThreadKind, ThreadUpsert,
};
use crate::stickers::StickerQuery;
use crate::store::{fold, Store};
use crate::uploads::UploadJob;
use crate::zalo::{
    Attachment, FriendAction, LoginState, SessionState, ZaloCommand, ZaloHandle, ZaloStatus,
};

/// Sample pictures are inline SVG, so the demo needs no network at all.
const PHOTO_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="800" height="600" viewBox="0 0 800 600"><defs><linearGradient id="g" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="#fde68a"/><stop offset="1" stop-color="#f97316"/></linearGradient></defs><rect width="800" height="600" fill="url(#g)"/><ellipse cx="400" cy="330" rx="310" ry="205" fill="#7c2d12" opacity="0.85"/><circle cx="290" cy="270" r="84" fill="#fff"/><circle cx="290" cy="270" r="62" fill="#16a34a"/><circle cx="510" cy="270" r="84" fill="#fff"/><circle cx="510" cy="270" r="62" fill="#dc2626"/><circle cx="400" cy="420" r="74" fill="#fff"/><circle cx="400" cy="420" r="54" fill="#facc15"/></svg>"##;

const POSTER_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="1280" height="720" viewBox="0 0 1280 720"><defs><linearGradient id="s" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#38bdf8"/><stop offset="1" stop-color="#e0f2fe"/></linearGradient></defs><rect width="1280" height="720" fill="url(#s)"/><circle cx="1010" cy="170" r="80" fill="#fde047"/><path d="M0 560 L300 300 L520 520 L760 260 L1280 600 L1280 720 L0 720 Z" fill="#15803d"/><path d="M0 640 L400 480 L800 620 L1280 520 L1280 720 L0 720 Z" fill="#166534"/></svg>"##;

const STICKER_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="240" height="240" viewBox="0 0 240 240"><circle cx="120" cy="124" r="100" fill="#facc15" stroke="#a16207" stroke-width="6"/><circle cx="85" cy="102" r="13" fill="#422006"/><circle cx="155" cy="102" r="13" fill="#422006"/><path d="M68 148 Q120 200 172 148" fill="none" stroke="#422006" stroke-width="11" stroke-linecap="round"/><circle cx="62" cy="140" r="14" fill="#fb7185" opacity="0.6"/><circle cx="178" cy="140" r="14" fill="#fb7185" opacity="0.6"/></svg>"##;

const LINK_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="640" height="320" viewBox="0 0 640 320"><rect width="640" height="320" fill="#1e293b"/><circle cx="320" cy="160" r="96" fill="none" stroke="#f97316" stroke-width="22"/><circle cx="320" cy="160" r="36" fill="#f97316"/></svg>"##;

const HOUSE_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="160" height="160" viewBox="0 0 160 160"><rect width="160" height="160" fill="#0ea5e9"/><path d="M34 82 L80 42 L126 82" fill="none" stroke="#fff" stroke-width="10" stroke-linecap="round" stroke-linejoin="round"/><path d="M46 76 V122 H114 V76" fill="#fff"/><rect x="70" y="94" width="20" height="28" fill="#0ea5e9"/></svg>"##;

fn svg_data(svg: &str) -> String {
    format!("data:image/svg+xml;base64,{}", STANDARD.encode(svg))
}

/// A drawn portrait for a sample person: a coloured square and a silhouette.
fn portrait(hue: u32) -> String {
    let deep = (hue + 30) % 360;
    svg_data(&format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="160" height="160" viewBox="0 0 160 160"><defs><linearGradient id="p" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="hsl({hue},70%,74%)"/><stop offset="1" stop-color="hsl({deep},62%,50%)"/></linearGradient></defs><rect width="160" height="160" fill="url(#p)"/><circle cx="80" cy="66" r="30" fill="#fff" opacity="0.92"/><path d="M24 160 C28 118 52 104 80 104 C108 104 132 118 136 160 Z" fill="#fff" opacity="0.92"/></svg>"##
    ))
}

/// A drawn sticker face: its colour and the path of its mouth.
fn face(fill: &str, mouth: &str) -> String {
    svg_data(&format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="240" height="240" viewBox="0 0 240 240"><circle cx="120" cy="124" r="100" fill="{fill}" stroke="#a16207" stroke-width="6"/><circle cx="85" cy="102" r="13" fill="#422006"/><circle cx="155" cy="102" r="13" fill="#422006"/><path d="{mouth}" fill="none" stroke="#422006" stroke-width="11" stroke-linecap="round"/></svg>"##
    ))
}

fn heart(fill: &str) -> String {
    svg_data(&format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="240" height="240" viewBox="0 0 240 240"><path d="M120 206 C40 150 20 110 40 76 C60 42 104 46 120 80 C136 46 180 42 200 76 C220 110 200 150 120 206 Z" fill="{fill}" stroke="#fff" stroke-width="8"/></svg>"##
    ))
}

/// Sample stickers: id, pack, words a search finds them by, picture.
fn demo_stickers() -> Vec<(u64, u64, &'static str, String)> {
    vec![
        (
            9001,
            1,
            "vui cười haha",
            face("#facc15", "M68 148 Q120 200 172 148"),
        ),
        (
            9002,
            1,
            "buồn khóc",
            face("#93c5fd", "M72 172 Q120 128 168 172"),
        ),
        (
            9003,
            1,
            "ngạc nhiên wow",
            face("#fdba74", "M100 162 Q120 196 140 162 Q120 128 100 162"),
        ),
        (
            9004,
            1,
            "bình thường ok",
            face("#bef264", "M78 160 L162 160"),
        ),
        (
            9005,
            1,
            "giận tức",
            face("#fca5a5", "M72 170 Q120 144 168 170"),
        ),
        (
            9006,
            1,
            "vui yêu thương",
            face("#f9a8d4", "M64 144 Q120 210 176 144"),
        ),
        (9101, 2, "yêu tim thương", heart("#ef4444")),
        (9102, 2, "yêu tim", heart("#ec4899")),
        (9103, 2, "yêu tim", heart("#a855f7")),
        (9104, 2, "yêu tim", heart("#f97316")),
    ]
}

/// What the demo's sticker store answers.
fn demo_sticker_answer(query: &StickerQuery) -> Value {
    let all = demo_stickers();
    let item =
        |s: &(u64, u64, &str, String)| json!({ "id": s.0, "cate_id": s.1, "type": 7, "url": s.3 });
    match query {
        StickerQuery::Packs => json!([
            { "id": 1, "name": "Mặt cười", "icon": all[0].3, "count": 6 },
            { "id": 2, "name": "Trái tim", "icon": all[6].3, "count": 4 },
        ]),
        StickerQuery::Pack(id) => {
            Value::Array(all.iter().filter(|s| s.1 == *id).map(item).collect())
        }
        StickerQuery::Search(q) => {
            let q = fold(q);
            Value::Array(
                all.iter()
                    .filter(|s| fold(s.2).contains(&q))
                    .map(item)
                    .collect(),
            )
        }
    }
}

/// Who is in the sample groups: size, and the members listed with their role.
fn demo_members(store: &Store, group_id: &str) -> Result<GroupMembers, String> {
    let (total, list): (u64, &[(&str, &'static str)]) = match group_id {
        "2001" => (
            5,
            &[
                ("3001", "owner"),
                ("3003", "admin"),
                ("3002", "member"),
                (ME, "member"),
                ("1004", "member"),
            ],
        ),
        "2002" => (
            38,
            &[
                ("1007", "owner"),
                ("1005", "member"),
                ("1006", "member"),
                (ME, "member"),
                ("1001", "member"),
            ],
        ),
        "2003" => (
            7,
            &[
                ("1006", "owner"),
                ("1004", "admin"),
                (ME, "member"),
                ("1002", "member"),
                ("1003", "member"),
                ("1005", "member"),
                ("1001", "member"),
            ],
        ),
        _ => return Err("demo: không có nhóm này".into()),
    };
    let uids: Vec<String> = list.iter().map(|(uid, _)| (*uid).to_string()).collect();
    let people = store.people(&uids).map_err(|e| e.to_string())?;
    let members = list
        .iter()
        .map(|&(uid, role)| {
            let person = people.iter().find(|p| p.uid == uid);
            Member {
                uid: uid.to_string(),
                name: person.map(|p| p.name.clone()).unwrap_or_default(),
                avatar: person.map(|p| p.avatar.clone()).unwrap_or_default(),
                friend: person.is_some_and(|p| p.friend),
                role,
            }
        })
        .collect();
    Ok(GroupMembers { total, members })
}

/// A person of the demo as clients get them from the friends side.
fn demo_person(store: &Store, uid: &str) -> Person {
    store
        .people(&[uid.to_string()])
        .ok()
        .and_then(|list| list.into_iter().next())
        .unwrap_or_default()
}

/// The friends side of the demo: a number ending in 5678 finds Ngọc Hân, one
/// ending in 0001 finds Lan Anh (a friend); Minh Khoa asked to be friends.
fn demo_friends(store: &Store, bus: &EventBus, action: FriendAction, answered: &mut bool) -> Value {
    match action {
        FriendAction::Find(phone) => {
            let digits: String = phone.chars().filter(char::is_ascii_digit).collect();
            let (uid, friendship) = if digits.ends_with("5678") {
                ("1010", "none")
            } else if digits.ends_with("0001") {
                ("1001", "friend")
            } else {
                return json!({ "user": null });
            };
            let p = demo_person(store, uid);
            json!({ "user": {
                "uid": p.uid, "name": p.name, "zalo_name": p.name, "avatar": p.avatar,
                "friendship": friendship,
            }})
        }
        FriendAction::Requests if *answered => json!([]),
        FriendAction::Requests => {
            let p = demo_person(store, "1011");
            json!([{
                "uid": p.uid, "display_name": p.name, "zalo_name": p.name, "avatar": p.avatar,
                "message": "Chào bạn, mình là Khoa, bạn học cũ nè", "time": now_ms() - 3_600_000,
            }])
        }
        FriendAction::Send { .. } => json!({ "ok": true }),
        FriendAction::Accept(uid) => {
            *answered = true;
            let p = demo_person(store, &uid);
            let _ = store.upsert_person(&p, Some(true));
            let thread = ThreadUpsert {
                kind: ThreadKind::User,
                remote_id: uid,
                name: p.name,
                avatar: p.avatar,
            };
            let _ = store.upsert_thread(&thread, true);
            let _ = bus.publish("threads_changed", json!({}));
            json!({ "ok": true })
        }
        FriendAction::Reject(_) => {
            *answered = true;
            json!({ "ok": true })
        }
    }
}

/// This account in the demo.
const ME: &str = "1000";

/// Sample people: user id, name, hue of their portrait (none: no picture),
/// friend of this account.
const PEOPLE: &[(&str, &str, Option<u32>, bool)] = &[
    (ME, "Demo", Some(210), false),
    ("1001", "Lan Anh", Some(330), true),
    ("1002", "Minh Tuấn", Some(25), true),
    ("1003", "Hoàng Nam", None, true),
    ("1004", "Thu Trang", Some(280), true),
    ("1005", "Bảo Ngọc", Some(160), true),
    ("1006", "Đức Anh", None, true),
    ("1007", "Quốc Huy", Some(195), true),
    ("3001", "Mẹ", Some(15), false),
    ("3002", "Minh", Some(120), false),
    ("3003", "Bố", None, false),
    // Found by phone number, and a friend request.
    ("1010", "Ngọc Hân", Some(45), false),
    ("1011", "Minh Khoa", Some(250), false),
];

fn picture_of(uid: &str) -> String {
    PEOPLE
        .iter()
        .find(|p| p.0 == uid)
        .and_then(|p| p.2)
        .map(portrait)
        .unwrap_or_default()
}

/// User id of a sample sender, by name.
fn uid_of(name: &str) -> &'static str {
    PEOPLE.iter().find(|p| p.1 == name).map_or("", |p| p.0)
}

fn seed_people(store: &Store) {
    let mut friends = Vec::new();
    for (uid, name, _, friend) in PEOPLE {
        let person = Person {
            uid: (*uid).into(),
            name: (*name).into(),
            avatar: picture_of(uid),
            ..Person::default()
        };
        if *friend {
            friends.push(person);
        } else {
            let _ = store.upsert_person(&person, None);
        }
    }
    let _ = store.set_friends(&friends);
    // A name the account gave a friend.
    let _ = store.set_aliases(&[("1003".into(), "Nam (đồng nghiệp)".into())]);
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
    /// Zalo-shaped JSON of the message (mentions…), `{}` for none.
    raw: String,
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
        raw: "{}".into(),
    }
}

/// A message naming `uid` with its first `len` UTF-16 units.
fn naming(
    sender: &'static str,
    text: &'static str,
    uid: &str,
    len: u32,
    minutes_ago: i64,
) -> Sample {
    Sample {
        raw: json!({ "mentions": [{ "uid": uid, "pos": 0, "len": len, "type": 0 }] }).to_string(),
        ..say(false, sender, text, minutes_ago)
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
        raw: "{}".into(),
    }
}

fn add_thread(store: &Store, kind: ThreadKind, remote: &str, name: &str, messages: Vec<Sample>) {
    let thread = ThreadUpsert {
        kind,
        remote_id: remote.into(),
        name: name.into(),
        avatar: match (kind, remote) {
            (ThreadKind::User, _) => picture_of(remote),
            (ThreadKind::Group, "2001") => svg_data(HOUSE_SVG),
            (ThreadKind::Group, _) => String::new(),
        },
    };
    if store.upsert_thread(&thread, true).is_err() {
        return;
    }
    for (i, sample) in messages.into_iter().enumerate() {
        let _ = store.insert_message(&NewMessage {
            thread_id: thread.id(),
            remote_id: format!("demo-{remote}-{i}"),
            client_id: String::new(),
            sender_id: if sample.outgoing {
                ME.into()
            } else if kind == ThreadKind::User {
                remote.into()
            } else {
                uid_of(sample.sender).into()
            },
            sender_name: sample.sender.to_string(),
            outgoing: sample.outgoing,
            kind: sample.kind.into(),
            text: sample.text.to_string(),
            media: sample.media,
            quote: sample.quote,
            sent_at: now_ms() - sample.minutes_ago * 60_000,
            raw: sample.raw,
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
            mine: *user == ME,
            icon: icon.to_string(),
            sent_at: now_ms() - 60_000 + i as i64,
        });
    }
}

/// Fill an empty store with a few conversations.
fn seed(store: &Store) {
    seed_people(store);
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
    add_thread(
        store,
        ThreadKind::Group,
        "2003",
        "Team dự án",
        vec![
            say(false, "Đức Anh", "Mai họp lúc 9 giờ nhé mọi người", 2900),
            say(false, "Thu Trang", "Ok anh, em chuẩn bị slide", 2890),
            say(true, "", "Mình sẽ gửi bản demo trước buổi họp", 2880),
            naming("Đức Anh", "@Demo nhớ gửi trước 8 giờ nhé", ME, 5, 2870),
        ],
    );
    add_thread(store, ThreadKind::Group, "2002", "Lớp 12A1", Vec::new());
    for remote in ["1003", "1004", "1005", "1006", "1007"] {
        let name = PEOPLE.iter().find(|p| p.0 == remote).map_or("", |p| p.1);
        add_thread(store, ThreadKind::User, remote, name, Vec::new());
    }
    for (group, members) in [("2001", 5), ("2002", 38), ("2003", 7)] {
        let _ = store.set_members(&thread_id(ThreadKind::Group, group), members);
    }
    add_reactions(store, ThreadKind::User, "1001", 2, &[(ME, "/-strong")]);
    add_reactions(store, ThreadKind::User, "1001", 5, &[("1001", "/-heart")]);
    add_reactions(
        store,
        ThreadKind::Group,
        "2001",
        3,
        &[
            ("3002", "/-heart"),
            ("3001", "/-heart"),
            (ME, "/-heart"),
            ("3002", ":o"),
        ],
    );
    let _ = store.mark_read(&thread_id(ThreadKind::Group, "2003"));
    // Lan Anh read the last reply; Minh Tuấn got the link but has not opened it.
    let _ = store.mark_receipt(&["demo-1001-5".into()], true);
    let _ = store.mark_receipt(&["demo-1002-4".into()], false);
    // Read the older threads so only some show unread badges.
    let _ = store.mark_read(&thread_id(ThreadKind::User, "1002"));
}

/// What a demo message carries besides its text.
struct Content<'a> {
    kind: &'a str,
    text: &'a str,
    media: Option<Media>,
    quote: Option<Quote>,
    raw: String,
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
                ME.into()
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
            raw: content.raw,
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
        raw: "{}".into(),
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
                raw: "{}".into(),
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
        uid: ME.into(),
        avatar: picture_of(ME),
        ..ZaloStatus::default()
    };
    let shared = handle.status.clone();
    tokio::spawn(async move {
        // Whether the demo's friend request was answered.
        let mut answered = false;
        while let Some(cmd) = rx.recv().await {
            match cmd {
                ZaloCommand::SendText {
                    kind,
                    remote_id,
                    text,
                    reply_to,
                    mentions,
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
                            raw: json!({ "mentions": mentions }).to_string(),
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
                        user_id: ME.into(),
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
                // Everyone in the demo is known from the start.
                ZaloCommand::LookupPeople { uids, reply } => {
                    let _ = reply.send(store.people(&uids).map_err(|e| e.to_string()));
                }
                ZaloCommand::Stickers { query, reply } => {
                    let _ = reply.send(Ok(demo_sticker_answer(&query)));
                }
                ZaloCommand::GroupMembers { group_id, reply } => {
                    let _ = reply.send(demo_members(&store, &group_id));
                }
                ZaloCommand::Forward {
                    kind,
                    remote_id,
                    text,
                    reply,
                } => {
                    let sent = store_and_publish(
                        &store,
                        &bus,
                        kind,
                        &remote_id,
                        true,
                        "",
                        self::text(&text),
                    );
                    let _ = reply.send(sent.ok_or_else(|| "demo: không lưu được tin".to_string()));
                }
                // Nothing to delete elsewhere in the demo.
                ZaloCommand::DeleteForMe { reply, .. } => {
                    let _ = reply.send(Ok(()));
                }
                ZaloCommand::Rename {
                    kind,
                    remote_id,
                    name,
                    reply,
                } => {
                    let id = thread_id(kind, &remote_id);
                    let _ = match kind {
                        ThreadKind::User => store.set_alias(&remote_id, &name),
                        ThreadKind::Group => store.name_thread(&id, name.trim()),
                    };
                    let _ = bus.publish("threads_changed", json!({}));
                    let _ = reply.send(store.thread(&id).map_err(|e| e.to_string()));
                }
                ZaloCommand::CreateGroup {
                    name,
                    members,
                    reply,
                } => {
                    let id = format!("{}", 2100 + rand::random::<u16>() % 800);
                    let names: Vec<String> = store
                        .people(&members)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|p| p.name)
                        .collect();
                    let thread = ThreadUpsert {
                        kind: ThreadKind::Group,
                        remote_id: id,
                        name: if name.trim().is_empty() {
                            format!("Demo, {}", names.join(", "))
                        } else {
                            name
                        },
                        avatar: String::new(),
                    };
                    let _ = store.upsert_thread(&thread, true);
                    let _ = store.set_members(&thread.id(), 1 + members.len() as u64);
                    let _ = bus.publish("threads_changed", json!({}));
                    let stored = store.thread(&thread.id()).ok().flatten();
                    let _ = reply.send(Ok(json!({ "thread": stored, "failed": [] })));
                }
                // Lan Anh is around, Minh Tuấn was a while ago, Hoàng Nam hides it.
                ZaloCommand::LastOnline { uid, reply } => {
                    let minutes = match uid.as_str() {
                        "1001" => 2,
                        "1002" => 180,
                        "1003" => -1,
                        _ => 26 * 60,
                    };
                    let seen = if minutes < 0 {
                        zeta_zalo::api::LastOnline {
                            visible: false,
                            at: 0,
                        }
                    } else {
                        zeta_zalo::api::LastOnline {
                            visible: true,
                            at: now_ms() - minutes * 60_000,
                        }
                    };
                    let _ = reply.send(Ok(seen));
                }
                ZaloCommand::Friends { action, reply } => {
                    let _ = reply.send(Ok(demo_friends(&store, &bus, action, &mut answered)));
                }
                ZaloCommand::SendSticker {
                    kind,
                    remote_id,
                    sticker,
                    reply,
                } => {
                    let url = demo_stickers()
                        .into_iter()
                        .find(|s| s.0 == sticker.id)
                        .map_or_else(|| sticker.url(), |s| s.3);
                    let content = Content {
                        kind: "chat.sticker",
                        text: "[Nhãn dán]",
                        media: Some(Media {
                            kind: "sticker".into(),
                            url,
                            ..Media::default()
                        }),
                        quote: None,
                        raw: "{}".into(),
                    };
                    let sent = store_and_publish(&store, &bus, kind, &remote_id, true, "", content);
                    let _ = reply.send(sent.ok_or_else(|| "demo: không lưu được tin".to_string()));
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
    handle
}
