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
        "chat.undo" => "[Tin nhắn đã thu hồi]".into(),
        "chat.recommended" | "chat.link" => {
            let caption = ["title", "description", "href"]
                .into_iter()
                .map(|k| get_str(content, k))
                .find(|s| !s.is_empty())
                .unwrap_or_default();
            with_caption("[Liên kết]", caption)
        }
        // Rich cards: the client shows the fallback sentence Zalo provides.
        "chat.webcontent" => {
            let p = params(content);
            let msg = p
                .pointer("/customMsg/msg")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            if msg.is_empty() {
                "[Nội dung đặc biệt]".into()
            } else {
                msg.to_string()
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

/// Media carried by a message, if any.
pub fn extract(msg_type: &str, content: Option<&Value>) -> Option<Media> {
    if !matches!(content, Some(Value::Object(_))) {
        return None;
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
