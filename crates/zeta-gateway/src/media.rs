//! Message content: the text shown for a message and the media it carries.
//!
//! Shapes verified on live messages (November 2026): `content` is an object with
//! `href`, `thumb`, `title`, `description` and `params`, a JSON *string* holding
//! sizes (`width`/`height`, `video_width`/`video_height`), the HD image (`hd`) or
//! the file size. Stickers carry only `{id, catId, type}`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Media attached to a message. URLs point at Zalo's CDN; clients load them
/// through the gateway's `/api/v1/media` proxy.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Media {
    /// `image`, `video`, `sticker`, `gif`, `file`, `audio` or `link`.
    pub kind: String,
    /// Full-size resource, or the target of a link.
    pub url: String,
    /// Preview image: thumbnail, video poster, link preview.
    #[serde(default)]
    pub thumb: String,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    /// File name or link title.
    #[serde(default)]
    pub name: String,
    /// Link description or photo caption.
    #[serde(default)]
    pub description: String,
    /// Size in bytes, when known.
    #[serde(default)]
    pub size: u64,
    /// Length of a video or voice message, when known.
    #[serde(default)]
    pub duration_ms: u64,
    /// A sticker's id, to ask how it moves (0 for other media, and for
    /// stickers stored before it was kept).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub sticker_id: u64,
    /// A contact card's person (`contact`): `name` is theirs, `thumb` their
    /// picture, `url` their profile QR, `description` a phone number if the
    /// sender added one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub uid: String,
    /// A bank account card (`bank`), once read: `url` is then its template
    /// on Zalo's CDN, read on demand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bank: Option<BankCard>,
    /// A group poll (`poll`): its id, to ask how it stands; `name` is the
    /// question.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub poll_id: u64,
}

/// What a bank account card shows.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BankCard {
    /// NAPAS code of the bank, for VietQR.
    pub bin: String,
    pub number: String,
    /// Capitals without accents, as banks write it; may be empty.
    pub holder: String,
    /// The bank's short name: "Vietcombank", "MB"…
    pub bank: String,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// Videos up to this size are downloaded as soon as their message arrives.
const PREFETCH_VIDEO_MAX: u64 = 100 * 1024 * 1024;

impl Media {
    /// Files worth downloading before anyone opens the message: what the client
    /// shows at once, videos and voice messages. Not attached files, which can be
    /// large and are rarely all opened, and not the pages links point at.
    pub fn prefetch_urls(&self) -> Vec<String> {
        let mut urls = Vec::new();
        if !self.thumb.is_empty() {
            urls.push(self.thumb.clone());
        }
        let main = match self.kind.as_str() {
            "image" | "gif" | "sticker" | "audio" => true,
            "video" => self.size <= PREFETCH_VIDEO_MAX,
            _ => false,
        };
        if main && !urls.contains(&self.url) {
            urls.push(self.url.clone());
        }
        urls
    }
}

/// The message a reply quotes.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Quote {
    /// Display name of the quoted message's sender.
    #[serde(default)]
    pub sender_name: String,
    /// Text of the quoted message, or a summary of its attachment.
    #[serde(default)]
    pub text: String,
    /// Zalo id of the quoted message.
    #[serde(default)]
    pub remote_id: String,
}

/// Text shown for a recalled message.
pub const RECALLED_TEXT: &str = "Tin nhắn đã được thu hồi";

/// The quote of a reply: the message's `quote` object (`fromD`, `msg`,
/// `attach`, `globalMsgId`), as described by zca-js.
pub fn extract_quote(message: &Value) -> Option<Quote> {
    let q = message.get("quote").filter(|q| q.is_object())?;
    let text = q.get("msg").and_then(Value::as_str).unwrap_or("").trim();
    let attached = match q.get("attach") {
        Some(Value::String(s)) => !s.trim().is_empty() && s.trim() != "{}",
        Some(Value::Object(o)) => !o.is_empty(),
        _ => false,
    };
    let text = if !text.is_empty() {
        text.to_string()
    } else if attached {
        "[Tệp đính kèm]".to_string()
    } else {
        String::new()
    };
    let remote_id = match q.get("globalMsgId") {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    };
    let sender_name = q
        .get("fromD")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    (!text.is_empty() || !sender_name.is_empty()).then_some(Quote {
        sender_name,
        text,
        remote_id,
    })
}

/// How a styled text is drawn. Seen live (November 2026): Zalo sends it as
/// `webchat` with `content` an object whose `action` is `rtf`, `title` the
/// text and `params` the JSON text of `{ styles: [{ start, len, st }], ver }`.
/// Unknown codes are dropped.
pub fn extract_styles(message: &Value) -> Vec<zeta_zalo::api::TextStyle> {
    let content = message.get("content");
    if content
        .and_then(|c| c.get("action"))
        .and_then(Value::as_str)
        != Some("rtf")
    {
        return Vec::new();
    }
    let p = params(content);
    let Some(list) = p.get("styles").and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|s| {
            let st = s.get("st").and_then(Value::as_str)?.trim().to_string();
            let start = u32::try_from(number(s, &["start"])).ok()?;
            let len = u32::try_from(number(s, &["len"])).ok()?;
            let known = zeta_zalo::api::known_style(&st);
            (len > 0 && known).then_some(zeta_zalo::api::TextStyle { start, len, st })
        })
        .take(500)
        .collect()
}

/// How much a message matters: its `specialMsgType`, 1 important and 2
/// urgent (seen live, November 2026), else 0.
pub fn extract_urgency(message: &Value) -> u8 {
    match number(message, &["specialMsgType"]) {
        1 => 1,
        2 => 2,
        _ => 0,
    }
}

/// Messages a `chat.delete` says this account deleted for itself (on another
/// device): `(Zalo id, client id)` of each, from `content`, a list of `{
/// globalDelMsgId, clientDelMsgId, … }` (seen live).
pub fn deleted_ids(content: Option<&Value>) -> Vec<(String, String)> {
    let Some(list) = content.and_then(Value::as_array) else {
        return Vec::new();
    };
    let id = |v: &Value, key: &str| match v.get(key) {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.trim().to_string(),
        _ => String::new(),
    };
    list.iter()
        .map(|d| (id(d, "globalDelMsgId"), id(d, "clientDelMsgId")))
        .filter(|(remote, client)| {
            let known = |s: &str| !s.is_empty() && s != "0";
            known(remote) || known(client)
        })
        .collect()
}

/// People named in a message: its `mentions`, `[{ uid, pos, len, type }]`
/// as zca-js describes them (`uid` "-1" is everyone).
pub fn extract_mentions(message: &Value) -> Vec<zeta_zalo::api::Mention> {
    let Some(list) = message.get("mentions").and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|m| {
            let uid = match m.get("uid") {
                Some(Value::String(s)) => s.trim().to_string(),
                Some(Value::Number(n)) => n.to_string(),
                _ => return None,
            };
            let pos = u32::try_from(number(m, &["pos"])).ok()?;
            let len = u32::try_from(number(m, &["len"])).ok()?;
            (!uid.is_empty() && len > 0).then_some(zeta_zalo::api::Mention { uid, pos, len })
        })
        .collect()
}

/// Ids of the message a `chat.undo` recalls: (global id, client id).
pub fn recalled_ids(content: Option<&Value>) -> Option<(String, String)> {
    let id = |key: &str| match content.and_then(|c| c.get(key)) {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.trim().to_string(),
        _ => String::new(),
    };
    let ids = (id("globalMsgId"), id("cliMsgId"));
    (!ids.0.is_empty() || !ids.1.is_empty()).then_some(ids)
}

fn get_str<'a>(v: Option<&'a Value>, key: &str) -> &'a str {
    v.and_then(|c| c.get(key))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
}

/// `content.params`, which Zalo sends as a JSON string (sometimes an object).
fn params(content: Option<&Value>) -> Value {
    match content.and_then(|c| c.get("params")) {
        Some(Value::String(s)) => serde_json::from_str(s).unwrap_or(Value::Null),
        Some(v @ Value::Object(_)) => v.clone(),
        _ => Value::Null,
    }
}

fn number(v: &Value, keys: &[&str]) -> u64 {
    keys.iter()
        .find_map(|k| match v.get(*k) {
            Some(Value::Number(n)) => n.as_u64(),
            Some(Value::String(s)) => s.trim().parse().ok(),
            _ => None,
        })
        .unwrap_or(0)
}

fn http_url(s: &str) -> String {
    if s.starts_with("https://") || s.starts_with("http://") {
        s.to_string()
    } else {
        String::new()
    }
}

/// Text of a message, or a short Vietnamese summary for non-text content. Used
/// for previews and notifications, and as a fallback when media cannot be shown.
pub fn summarize(msg_type: &str, content: Option<&Value>) -> String {
    if let Some(Value::String(text)) = content {
        return text.clone();
    }
    let with_caption = |label: &str, caption: &str| {
        if caption.is_empty() {
            label.to_string()
        } else {
            format!("{label} {caption}")
        }
    };
    match msg_type {
        "chat.photo" => with_caption("[Hình ảnh]", get_str(content, "title")),
        "chat.sticker" => "[Nhãn dán]".into(),
        "chat.gif" => "[GIF]".into(),
        "chat.voice" => "[Tin nhắn thoại]".into(),
        "chat.video.msg" => with_caption("[Video]", get_str(content, "title")),
        "share.file" => with_caption("[Tập tin]", get_str(content, "title")),
        "chat.location.new" => "[Vị trí]".into(),
        "chat.undo" => RECALLED_TEXT.into(),
        "group.poll" => {
            let p = params(content);
            let question = p
                .get("question")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|q| !q.is_empty())
                .unwrap_or_else(|| get_str(content, "title"));
            with_caption("[Bình chọn]", question)
        }
        // Cards ("thiệp"), as Zalo Web: the preview text they carry.
        "chat.ecard" => {
            let p = params(content);
            let preview = p
                .get("previewTxt")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|t| !t.is_empty());
            match preview {
                Some(t) => t.to_string(),
                None => with_caption("[Thiệp]", get_str(content, "title")),
            }
        }
        "chat.todo" => with_caption("[Công việc]", get_str(content, "title")),
        "chat.recommended" if contact_card(content) => {
            with_caption("[Danh thiếp]", get_str(content, "title"))
        }
        "chat.recommended" | "chat.link" => {
            let caption = ["title", "description", "href"]
                .into_iter()
                .map(|k| get_str(content, k))
                .find(|s| !s.is_empty())
                .unwrap_or_default();
            with_caption("[Liên kết]", caption)
        }
        // Rich cards: the client shows the fallback sentence Zalo provides,
        // in Vietnamese when it gives several (`{ en, vi }`, seen live).
        "chat.webcontent" => {
            let p = params(content);
            let msg = match p.pointer("/customMsg/msg") {
                Some(Value::String(s)) => s.trim().to_string(),
                Some(Value::Object(by_lang)) => ["vi", "en"]
                    .into_iter()
                    .find_map(|l| by_lang.get(l).and_then(Value::as_str))
                    .unwrap_or("")
                    .trim()
                    .to_string(),
                _ => String::new(),
            };
            if !msg.is_empty() {
                msg
            } else if bank_card(content) {
                "[Tài khoản ngân hàng]".into()
            } else {
                "[Nội dung đặc biệt]".into()
            }
        }
        other => {
            let title = get_str(content, "title");
            if title.is_empty() {
                format!("[{other}]")
            } else {
                title.to_string()
            }
        }
    }
}

/// Sticker image on Zalo's CDN (rule from ctdozlite): the 30 built-in stickers of
/// category 0 have static files, the rest come from the emoticon service.
fn sticker_url(content: Option<&Value>) -> String {
    let c = content.cloned().unwrap_or(Value::Null);
    let id = number(&c, &["id"]);
    if id == 0 {
        return String::new();
    }
    let category = number(&c, &["catId"]);
    if category == 0 && (1..=30).contains(&id) {
        format!("https://stc-chat.zdn.vn/images/stickers/default/thumb/{id}.png")
    } else {
        format!("https://api.zaloapp.com/api/emoticon/sticker/webpc?eid={id}&size=130")
    }
}

/// A `chat.recommended` that shares a person (a contact card) rather than a
/// link: its `action` is `recommened.user` (sic, seen live).
fn contact_card(content: Option<&Value>) -> bool {
    matches!(
        get_str(content, "action"),
        "recommened.user" | "recommended.user"
    )
}

/// A contact card: the person's id is `params`, `description` the JSON text
/// of `{ qrCodeUrl, gUid, phone? }` (seen live, November 2026).
fn contact_media(content: Option<&Value>) -> Option<Media> {
    let uid = match content.and_then(|c| c.get("params")) {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    if uid.is_empty() || !uid.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let details: Value =
        serde_json::from_str(get_str(content, "description")).unwrap_or(Value::Null);
    let phone = match details.get("phone") {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    Some(Media {
        kind: "contact".into(),
        url: http_url(
            details
                .get("qrCodeUrl")
                .and_then(Value::as_str)
                .unwrap_or(""),
        ),
        thumb: http_url(get_str(content, "thumb")),
        name: get_str(content, "title").trim().to_string(),
        description: phone,
        uid,
        ..Media::default()
    })
}

/// A `chat.webcontent` that is a bank account card (`zinstant.bankcard`).
fn bank_card(content: Option<&Value>) -> bool {
    get_str(content, "action") == "zinstant.bankcard"
}

/// A bank account card: what it shows is in its template
/// (`params.pcItem.data_url`, seen live), read when someone looks; cards sent
/// from here keep what they show under `content.bank`.
fn bank_media(content: Option<&Value>) -> Option<Media> {
    let p = params(content);
    let template = ["/pcItem/data_url", "/item/data_url"]
        .into_iter()
        .find_map(|at| p.pointer(at).and_then(Value::as_str))
        .unwrap_or("");
    let known = content
        .and_then(|c| c.get("bank"))
        .and_then(|b| serde_json::from_value::<BankCard>(b.clone()).ok());
    let media = Media {
        kind: "bank".into(),
        url: http_url(template),
        bank: known,
        ..Media::default()
    };
    (!media.url.is_empty() || media.bank.is_some()).then_some(media)
}

/// A group poll message (`group.poll`): `params` is the JSON text of `{
/// pollId, question, … }`, as Zalo Web reads it.
fn poll_media(content: Option<&Value>) -> Option<Media> {
    let p = params(content);
    let poll_id = number(&p, &["pollId", "poll_id"]);
    let question = p
        .get("question")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .unwrap_or_else(|| get_str(content, "title"))
        .to_string();
    (poll_id > 0).then(|| Media {
        kind: "poll".into(),
        name: question,
        poll_id,
        ..Media::default()
    })
}

/// A line Zalo writes into a chat about something done in it ("Lan tạo nhắc
/// hẹn mới …", "… xóa nhắc hẹn …"): a `webchat` whose `content.action` is
/// `msginfo.actionlist` (seen live). `name` is the sentence, `thumb` its
/// icon, `description` the label of the button Zalo shows with it.
fn notice_media(content: Option<&Value>) -> Option<Media> {
    if get_str(content, "action") != "msginfo.actionlist" {
        return None;
    }
    let p = params(content);
    let label = p
        .pointer("/actions/0/actionLabelv2")
        .and_then(|l| {
            ["vi", "en"]
                .into_iter()
                .find_map(|k| l.get(k).and_then(Value::as_str))
        })
        .unwrap_or("")
        .to_string();
    Some(Media {
        kind: "notice".into(),
        name: get_str(content, "title").trim().to_string(),
        thumb: http_url(p.get("iconUrl").and_then(Value::as_str).unwrap_or("")),
        description: label,
        ..Media::default()
    })
}

/// A card ("thiệp"), such as the one a new reminder brings (seen live:
/// `title` "⏰ <title>", `description` when it is due, `thumb` a picture).
/// A reminder's card says so with `url` `reminder:`.
fn ecard_media(content: Option<&Value>) -> Option<Media> {
    let title = get_str(content, "title").trim().to_string();
    if title.is_empty() {
        return None;
    }
    let p = params(content);
    let reminder = p.get("actions").and_then(Value::as_array).is_some_and(|a| {
        a.iter()
            .any(|x| x.get("actionId").and_then(Value::as_str) == Some("action.open.reminder"))
    });
    Some(Media {
        kind: "ecard".into(),
        url: if reminder {
            "reminder:".into()
        } else {
            http_url(get_str(content, "href"))
        },
        thumb: http_url(get_str(content, "thumb")),
        name: title,
        description: get_str(content, "description").trim().to_string(),
        ..Media::default()
    })
}

/// Media carried by a message, if any.
pub fn extract(msg_type: &str, content: Option<&Value>) -> Option<Media> {
    if !matches!(content, Some(Value::Object(_))) {
        return None;
    }
    if msg_type == "chat.recommended" && contact_card(content) {
        return contact_media(content);
    }
    if msg_type == "chat.webcontent" && bank_card(content) {
        return bank_media(content);
    }
    if msg_type == "group.poll" {
        return poll_media(content);
    }
    if msg_type == "webchat" {
        return notice_media(content);
    }
    if msg_type == "chat.ecard" {
        return ecard_media(content);
    }
    let p = params(content);
    let href = http_url(get_str(content, "href"));
    let thumb = http_url(get_str(content, "thumb"));
    let title = get_str(content, "title").to_string();
    let description = get_str(content, "description").to_string();
    let media = match msg_type {
        "chat.photo" | "chat.doodle" => {
            // Prefer the HD copy when Zalo has one.
            let hd = http_url(p.get("hd").and_then(Value::as_str).unwrap_or(""));
            Media {
                kind: "image".into(),
                url: if hd.is_empty() { href } else { hd },
                thumb,
                width: number(&p, &["width"]) as u32,
                height: number(&p, &["height"]) as u32,
                description: title,
                ..Media::default()
            }
        }
        "chat.video.msg" => {
            let (mut width, mut height) = (
                number(&p, &["video_width"]) as u32,
                number(&p, &["video_height"]) as u32,
            );
            // Sizes are those of the stored frames; a rotated video shows turned.
            if matches!(number(&p, &["video_rotation"]), 90 | 270) {
                std::mem::swap(&mut width, &mut height);
            }
            Media {
                kind: "video".into(),
                url: href,
                thumb,
                width,
                height,
                size: number(&p, &["fileSize"]),
                duration_ms: number(&p, &["duration"]),
                description: title,
                ..Media::default()
            }
        }
        "chat.gif" => Media {
            kind: "gif".into(),
            url: href,
            thumb,
            width: number(&p, &["width"]) as u32,
            height: number(&p, &["height"]) as u32,
            ..Media::default()
        },
        "chat.sticker" => Media {
            kind: "sticker".into(),
            url: sticker_url(content),
            sticker_id: content.map_or(0, |c| number(c, &["id"])),
            ..Media::default()
        },
        "share.file" => Media {
            kind: "file".into(),
            url: href,
            name: title,
            size: number(&p, &["fileSize", "size"]),
            ..Media::default()
        },
        "chat.voice" => Media {
            kind: "audio".into(),
            url: href,
            duration_ms: number(&p, &["duration"]),
            ..Media::default()
        },
        "chat.recommended" | "chat.link" => Media {
            kind: "link".into(),
            url: href,
            thumb,
            name: title,
            description,
            ..Media::default()
        },
        _ => return None,
    };
    (!media.url.is_empty()).then_some(media)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn photo() -> Value {
        json!({
            "href": "https://photo-stal-1.zdn.vn/a.jpg",
            "thumb": "https://photo-stal-1.zdn.vn/a_t.jpg",
            "title": "",
            "params": json!({ "width": 1280, "height": 960, "hd": "https://photo-stal-1.zdn.vn/a_hd.jpg" }).to_string()
        })
    }

    #[test]
    fn photo_prefers_hd_and_keeps_sizes() {
        let m = extract("chat.photo", Some(&photo())).unwrap();
        assert_eq!(m.kind, "image");
        assert_eq!(m.url, "https://photo-stal-1.zdn.vn/a_hd.jpg");
        assert_eq!(m.thumb, "https://photo-stal-1.zdn.vn/a_t.jpg");
        assert_eq!((m.width, m.height), (1280, 960));
    }

    #[test]
    fn video_from_a_separate_host() {
        let c = json!({
            "href": "https://video-stal-43.dlmd.me/v.mp4",
            "thumb": "https://photo-stal-13.zdn.vn/p.jpg",
            "params": json!({ "video_width": 720, "video_height": 1280, "fileSize": "123456", "duration": 38000 }).to_string()
        });
        let m = extract("chat.video.msg", Some(&c)).unwrap();
        assert_eq!(
            (m.kind.as_str(), m.url.as_str()),
            ("video", "https://video-stal-43.dlmd.me/v.mp4")
        );
        assert_eq!(
            (m.width, m.height, m.size, m.duration_ms),
            (720, 1280, 123_456, 38_000)
        );

        let rotated = json!({
            "href": "https://video-stal-43.dlmd.me/r.mp4",
            "params": json!({ "video_width": 1920, "video_height": 1080, "video_rotation": 90 }).to_string()
        });
        let m = extract("chat.video.msg", Some(&rotated)).unwrap();
        assert_eq!((m.width, m.height), (1080, 1920));
    }

    #[test]
    fn sticker_urls_follow_the_category_rule() {
        let builtin = extract(
            "chat.sticker",
            Some(&json!({ "id": 7, "catId": 0, "type": 1 })),
        )
        .unwrap();
        assert_eq!(
            builtin.url,
            "https://stc-chat.zdn.vn/images/stickers/default/thumb/7.png"
        );
        let store = extract(
            "chat.sticker",
            Some(&json!({ "id": 21300, "catId": 10, "type": 7 })),
        )
        .unwrap();
        assert!(store.url.ends_with("eid=21300&size=130"));
        assert!(extract("chat.sticker", Some(&json!({ "id": 0 }))).is_none());
    }

    #[test]
    fn files_links_and_text() {
        let file = extract(
            "share.file",
            Some(&json!({ "href": "https://dl.zdn.vn/f", "title": "bao-cao.pdf", "params": "{\"fileSize\":2048}" })),
        )
        .unwrap();
        assert_eq!((file.name.as_str(), file.size), ("bao-cao.pdf", 2048));

        let link = extract(
            "chat.recommended",
            Some(
                &json!({ "href": "https://example.com", "title": "Ví dụ", "description": "mô tả" }),
            ),
        )
        .unwrap();
        assert_eq!((link.kind.as_str(), link.name.as_str()), ("link", "Ví dụ"));

        assert!(extract("webchat", Some(&json!("chào"))).is_none());
        assert!(extract(
            "chat.photo",
            Some(&json!({ "href": "javascript:alert(1)" }))
        )
        .is_none());
    }

    #[test]
    fn prefetching_skips_files_links_and_huge_videos() {
        let image = extract("chat.photo", Some(&photo())).unwrap();
        assert_eq!(
            image.prefetch_urls(),
            [
                "https://photo-stal-1.zdn.vn/a_t.jpg",
                "https://photo-stal-1.zdn.vn/a_hd.jpg"
            ]
        );
        let same = Media {
            kind: "image".into(),
            url: "https://p.zdn.vn/x.jpg".into(),
            thumb: "https://p.zdn.vn/x.jpg".into(),
            ..Media::default()
        };
        assert_eq!(same.prefetch_urls().len(), 1);
        let file = Media {
            kind: "file".into(),
            url: "https://f.zdn.vn/a.zip".into(),
            ..Media::default()
        };
        assert!(file.prefetch_urls().is_empty());
        let link = Media {
            kind: "link".into(),
            url: "https://example.com".into(),
            thumb: "https://p.zdn.vn/t.jpg".into(),
            ..Media::default()
        };
        assert_eq!(link.prefetch_urls(), ["https://p.zdn.vn/t.jpg"]);
        let huge = Media {
            kind: "video".into(),
            url: "https://v.dlmd.me/v.mp4".into(),
            size: 500 * 1024 * 1024,
            ..Media::default()
        };
        assert!(huge.prefetch_urls().is_empty());
    }

    #[test]
    fn quotes_and_recalls() {
        let reply = json!({ "content": "đồng ý", "quote": {
            "ownerId": "5", "cliMsgId": 11, "globalMsgId": 22, "cliMsgType": 1,
            "ts": 1, "msg": " đi cà phê không? ", "attach": "", "fromD": "Lan", "ttl": 0
        }});
        assert_eq!(
            extract_quote(&reply),
            Some(Quote {
                sender_name: "Lan".into(),
                text: "đi cà phê không?".into(),
                remote_id: "22".into(),
            })
        );
        let photo_reply =
            json!({ "quote": { "msg": "", "attach": "{\"thumb\":\"x\"}", "fromD": "Lan" } });
        assert_eq!(extract_quote(&photo_reply).unwrap().text, "[Tệp đính kèm]");
        assert_eq!(extract_quote(&json!({ "content": "x" })), None);
        assert_eq!(extract_quote(&json!({ "quote": {} })), None);

        let undo = json!({ "globalMsgId": 7_000_000_001_u64, "cliMsgId": 123, "deleteMsg": 1 });
        assert_eq!(
            recalled_ids(Some(&undo)),
            Some(("7000000001".to_string(), "123".to_string()))
        );
        assert_eq!(recalled_ids(Some(&json!({}))), None);
    }

    #[test]
    fn styled_and_marked_texts() {
        // As seen live on a message sent from here with styles and urgency 1.
        let message = json!({
            "msgType": "webchat",
            "specialMsgType": 1,
            "content": {
                "action": "rtf", "title": "Thu dinh dang", "href": "", "description": "",
                "params": "{\"styles\":[{\"len\":3,\"st\":\"b\",\"start\":0},{\"len\":2,\"st\":\"c_db342e\",\"start\":4},{\"len\":1,\"st\":\"bogus\",\"start\":0}],\"ver\":0}"
            }
        });
        let styles = extract_styles(&message);
        assert_eq!(styles.len(), 2, "unknown codes are dropped");
        assert_eq!((styles[1].start, styles[1].len), (4, 2));
        assert_eq!(styles[1].st, "c_db342e");
        assert_eq!(extract_urgency(&message), 1);
        assert_eq!(
            summarize("webchat", message.get("content")),
            "Thu dinh dang"
        );
        assert!(extract("webchat", message.get("content")).is_none());
        let plain = json!({ "msgType": "webchat", "content": "chào" });
        assert!(extract_styles(&plain).is_empty());
        assert_eq!(extract_urgency(&plain), 0);
        assert_eq!(extract_urgency(&json!({ "specialMsgType": 9 })), 0);
    }

    #[test]
    fn contact_cards() {
        // As seen live (ids and addresses changed).
        let card = json!({
            "action": "recommened.user", "childnumber": 0, "href": "www.zaloapp.com",
            "params": "1234567", "thumb": "https://s120-ava-talk.zadn.vn/a/b.jpg", "title": "Lan",
            "description": "{\"qrCodeUrl\":\"https:\\/\\/qr-talk.zdn.vn\\/1\\/2\\/c.jpg\",\"gUid\":\"G\",\"phone\":\"0901\"}",
            "type": ""
        });
        let m = extract("chat.recommended", Some(&card)).unwrap();
        assert_eq!(
            (m.kind.as_str(), m.uid.as_str(), m.name.as_str()),
            ("contact", "1234567", "Lan")
        );
        assert_eq!(m.url, "https://qr-talk.zdn.vn/1/2/c.jpg");
        assert_eq!(m.description, "0901");
        assert_eq!(
            summarize("chat.recommended", Some(&card)),
            "[Danh thiếp] Lan"
        );
        // A link shared the same way stays a link.
        let link = json!({ "action": "recommened.link", "href": "https://x.vn", "title": "X" });
        assert_eq!(
            extract("chat.recommended", Some(&link)).unwrap().kind,
            "link"
        );
        let broken = json!({ "action": "recommened.user", "params": "abc" });
        assert!(extract("chat.recommended", Some(&broken)).is_none());
    }

    #[test]
    fn notices_and_reminder_cards() {
        // As seen live after a reminder was made in the Cloud thread (cut down).
        let notice = json!({
            "action": "msginfo.actionlist", "title": "Lan tạo nhắc hẹn mới Họp - 17:53.",
            "params": "{\"msg\":{\"vi\":\"%1$s tạo nhắc hẹn mới %2$s - %3$s.\"},\"iconUrl\":\"https://res-zalo.zadn.vn/i.png\",\"actions\":[{\"actionLabelv2\":{\"vi\":\"Xem\",\"en\":\"View\"},\"actionType\":\"action.open.grouptopic.detail\"}]}"
        });
        let m = extract("webchat", Some(&notice)).unwrap();
        assert_eq!((m.kind.as_str(), m.description.as_str()), ("notice", "Xem"));
        assert_eq!(m.name, "Lan tạo nhắc hẹn mới Họp - 17:53.");
        assert_eq!(
            summarize("webchat", Some(&notice)),
            "Lan tạo nhắc hẹn mới Họp - 17:53."
        );
        let card = json!({
            "action": "show.profile", "title": "⏰ Họp", "description": "Thứ Năm, 5 tháng 11 lúc 17:53",
            "href": "https://zalo.me/x", "thumb": "https://res-zalo.zadn.vn/r.png",
            "params": "{\"previewTxt\":\"⏰ Họp\",\"actions\":[{\"actionId\":\"action.open.reminder\"}]}"
        });
        let m = extract("chat.ecard", Some(&card)).unwrap();
        assert_eq!((m.kind.as_str(), m.url.as_str()), ("ecard", "reminder:"));
        assert_eq!(m.description, "Thứ Năm, 5 tháng 11 lúc 17:53");
        assert!(extract("webchat", Some(&json!({ "action": "rtf", "title": "x" }))).is_none());
    }

    #[test]
    fn polls_and_other_group_cards() {
        let poll = json!({
            "title": "", "action": "",
            "params": "{\"pollId\":501,\"question\":\"Đi đâu chơi?\"}"
        });
        let m = extract("group.poll", Some(&poll)).unwrap();
        assert_eq!(
            (m.kind.as_str(), m.poll_id, m.name.as_str()),
            ("poll", 501, "Đi đâu chơi?")
        );
        assert_eq!(
            summarize("group.poll", Some(&poll)),
            "[Bình chọn] Đi đâu chơi?"
        );
        assert!(extract("group.poll", Some(&json!({ "title": "x" }))).is_none());
        let card =
            json!({ "title": "Sinh nhật", "params": { "previewTxt": "Chúc mừng sinh nhật!" } });
        assert_eq!(summarize("chat.ecard", Some(&card)), "Chúc mừng sinh nhật!");
        assert_eq!(
            summarize("chat.ecard", Some(&json!({ "title": "Tết" }))),
            "[Thiệp] Tết"
        );
        assert_eq!(
            summarize("chat.todo", Some(&json!({ "title": "Báo cáo" }))),
            "[Công việc] Báo cáo"
        );
    }

    #[test]
    fn bank_cards() {
        // As seen live, cut down.
        let params = json!({
            "item": { "data_url": "https://zinst-stc.zadn.vn/m/1.bin", "data_type": 2 },
            "pcItem": { "data_url": "https://zinst-stc.zadn.vn/p/1.html", "data_type": 4 },
            "customMsg": { "msg": { "en": "[Bank account]", "vi": "[Tài khoản ngân hàng]" } }
        });
        let card =
            json!({ "action": "zinstant.bankcard", "params": params.to_string(), "title": "" });
        let m = extract("chat.webcontent", Some(&card)).unwrap();
        assert_eq!(
            (m.kind.as_str(), m.url.as_str()),
            ("bank", "https://zinst-stc.zadn.vn/p/1.html")
        );
        assert!(m.bank.is_none());
        assert_eq!(
            summarize("chat.webcontent", Some(&card)),
            "[Tài khoản ngân hàng]"
        );
        // Sent from here: what it shows is kept with it.
        let sent = json!({ "action": "zinstant.bankcard", "bank": {
            "bin": "970436", "number": "0123456789", "holder": "NGUYEN VAN A", "bank": "Vietcombank"
        } });
        let m = extract("chat.webcontent", Some(&sent)).unwrap();
        assert_eq!(m.bank.unwrap().number, "0123456789");
        // Other rich cards keep Zalo's sentence and carry no media.
        let other =
            json!({ "action": "zinstant.x", "params": "{\"customMsg\":{\"msg\":\"[Vị trí]\"}}" });
        assert_eq!(summarize("chat.webcontent", Some(&other)), "[Vị trí]");
        assert!(extract("chat.webcontent", Some(&other)).is_none());
    }

    #[test]
    fn deletions_on_other_devices() {
        let content = json!([
            { "actionType": 0, "clientDelMsgId": 17, "destId": 5, "globalDelMsgId": 900,
              "type": 0, "uidFrom": 1, "uidTo": 2 },
            { "globalDelMsgId": 0, "clientDelMsgId": 0 }
        ]);
        assert_eq!(
            deleted_ids(Some(&content)),
            [("900".to_string(), "17".to_string())]
        );
        assert!(deleted_ids(Some(&json!({}))).is_empty());
    }

    #[test]
    fn summaries_for_non_text_content() {
        assert_eq!(
            summarize("chat.sticker", Some(&json!({ "id": 1 }))),
            "[Nhãn dán]"
        );
        assert_eq!(
            summarize("share.file", Some(&json!({ "title": "a.pdf" }))),
            "[Tập tin] a.pdf"
        );
        assert_eq!(
            summarize("chat.recommended", Some(&json!({ "href": "https://x" }))),
            "[Liên kết] https://x"
        );
        assert_eq!(
            summarize("chat.new.kind", Some(&json!({}))),
            "[chat.new.kind]"
        );
        assert_eq!(summarize("webchat", Some(&json!("plain"))), "plain");
        let card = json!({ "params": json!({ "customMsg": { "msg": "Bạn có lời nhắc mới" } }).to_string() });
        assert_eq!(
            summarize("chat.webcontent", Some(&card)),
            "Bạn có lời nhắc mới"
        );
    }
}
