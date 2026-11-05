//! Replying to a message (a text that quotes it) and recalling one's own.
//!
//! As in zca-js `sendMessage` with a quote: `POST {chat}/api/message/quote` or
//! `{group}/api/group/quote`, the text plus `qmsgOwner`, `qmsgId`,
//! `qmsgCliId`, `qmsgType` (a number, see [`client_message_type`]), `qmsgTs`,
//! `qmsg` (the quoted text, empty for media), `qmsgTTL`; groups add `qmsgAttach`
//! (the quoted media as JSON text) and `visibility`, 1-1 chats add `imei`.
//!
//! And zca-js `undo`: `POST {chat}/api/message/undo` or `{group}/api/group/undomsg`
//! with `msgId`, `clientId` and `cliMsgIdUndo`. Zalo then pushes a `chat.undo`
//! message, as for a recall made on the phone.

use serde_json::{json, Map, Value};

use crate::api::{first_str, Recipient, SentMessage};
use crate::client::ZaloClient;
use crate::domains::Service;
use crate::error::Result;
use crate::now_millis;

/// The message a reply quotes.
#[derive(Clone, Debug, PartialEq)]
pub struct QuotedMessage {
    /// Who sent it: this account's uid for its own messages.
    pub owner_id: String,
    pub msg_id: String,
    pub cli_msg_id: String,
    /// `msgType`: `webchat`, `chat.photo`…
    pub msg_type: String,
    /// Milliseconds since the epoch.
    pub ts: i64,
    /// The text of a text message, else the original `content` object.
    pub content: Value,
    pub ttl: i64,
}

/// `qmsgType` of a `msgType` (zca-js `getClientMessageType`).
pub fn client_message_type(msg_type: &str) -> i64 {
    match msg_type {
        "chat.voice" => 31,
        "chat.photo" => 32,
        "chat.sticker" => 36,
        "chat.doodle" => 37,
        "chat.recommended" | "chat.link" => 38,
        "chat.location.new" => 43,
        "chat.video.msg" => 44,
        "share.file" => 46,
        "chat.gif" => 49,
        _ => 1,
    }
}

/// The quoted media as zca-js describes it (`prepareQMSGAttach`): the content
/// with its picture under the names the official client reads.
fn quoted_attachment(content: &Value) -> Value {
    let Value::Object(fields) = content else {
        return json!({});
    };
    let mut out = fields.clone();
    if let Some(thumb) = fields.get("thumb") {
        out.insert("thumbUrl".into(), thumb.clone());
    }
    if let Some(href) = fields.get("href") {
        out.insert("oriUrl".into(), href.clone());
        out.insert("normalUrl".into(), href.clone());
    }
    Value::Object(out)
}

/// `params` of a reply.
fn reply_params(
    to: Recipient<'_>,
    text: &str,
    quote: &QuotedMessage,
    client_id: u64,
    imei: &str,
) -> Map<String, Value> {
    let mut p = Map::new();
    match to {
        Recipient::User(uid) => p.insert("toid".into(), json!(uid)),
        Recipient::Group(grid) => p.insert("grid".into(), json!(grid)),
    };
    p.insert("message".into(), json!(text));
    p.insert("clientId".into(), json!(client_id));
    p.insert("qmsgOwner".into(), json!(quote.owner_id));
    p.insert("qmsgId".into(), json!(quote.msg_id));
    p.insert("qmsgCliId".into(), json!(quote.cli_msg_id));
    p.insert(
        "qmsgType".into(),
        json!(client_message_type(&quote.msg_type)),
    );
    p.insert("qmsgTs".into(), json!(quote.ts));
    let qmsg = quote.content.as_str().unwrap_or_default();
    p.insert("qmsg".into(), json!(qmsg));
    match to {
        Recipient::User(_) => {
            p.insert("imei".into(), json!(imei));
        }
        Recipient::Group(_) => {
            p.insert("visibility".into(), json!(0));
            let attach = if quote.content.is_string() {
                json!({})
            } else {
                quoted_attachment(&quote.content)
            };
            p.insert("qmsgAttach".into(), json!(attach.to_string()));
        }
    }
    p.insert("qmsgTTL".into(), json!(quote.ttl));
    p.insert("ttl".into(), json!(0));
    p
}

impl ZaloClient {
    /// Send `text` as a reply to `quote`.
    pub async fn send_reply(
        &self,
        to: Recipient<'_>,
        text: &str,
        quote: &QuotedMessage,
    ) -> Result<SentMessage> {
        let key = self.session_key()?;
        let client_id = now_millis();
        let params = reply_params(to, text, quote, client_id, &self.device().imei);
        let (service, path) = match to {
            Recipient::User(_) => (Service::Chat, "/api/message/quote"),
            Recipient::Group(_) => (Service::Group, "/api/group/quote"),
        };
        let url = format!(
            "{}{path}?{}",
            self.domains().primary(service),
            Self::common_params()
        );
        let form = format!("params={}", self.encrypt_params(&Value::Object(params))?);
        let data = self.post_form(&url, form, Some(key)).await?;
        Ok(SentMessage {
            msg_id: first_str(&data, &["msgId"]),
            client_id,
        })
    }

    /// Recall one of this account's messages, for everyone in the chat.
    pub async fn recall(&self, to: Recipient<'_>, msg_id: &str, cli_msg_id: &str) -> Result<()> {
        let key = self.session_key()?;
        let mut p = Map::new();
        p.insert("msgId".into(), json!(msg_id));
        p.insert("clientId".into(), json!(now_millis()));
        p.insert("cliMsgIdUndo".into(), json!(cli_msg_id));
        let (service, path) = match to {
            Recipient::User(uid) => {
                p.insert("toid".into(), json!(uid));
                (Service::Chat, "/api/message/undo")
            }
            Recipient::Group(grid) => {
                p.insert("grid".into(), json!(grid));
                p.insert("visibility".into(), json!(0));
                p.insert("imei".into(), json!(self.device().imei));
                (Service::Group, "/api/group/undomsg")
            }
        };
        let url = format!(
            "{}{path}?{}",
            self.domains().primary(service),
            Self::common_params()
        );
        let form = format!("params={}", self.encrypt_params(&Value::Object(p))?);
        self.post_form(&url, form, Some(key)).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quote(content: Value) -> QuotedMessage {
        QuotedMessage {
            owner_id: "100".into(),
            msg_id: "5005".into(),
            cli_msg_id: "6006".into(),
            msg_type: "chat.photo".into(),
            ts: 1_700_000_000_000,
            content,
            ttl: 0,
        }
    }

    #[test]
    fn replies_to_a_person_carry_the_quoted_text() {
        let mut q = quote(json!("đi cà phê nhé"));
        q.msg_type = "webchat".into();
        let p = reply_params(Recipient::User("100"), "ok", &q, 42, "imei-1");
        assert_eq!(p["toid"], "100");
        assert_eq!(p["message"], "ok");
        assert_eq!(
            (p["qmsgId"].as_str(), p["qmsgCliId"].as_str()),
            (Some("5005"), Some("6006"))
        );
        assert_eq!(
            (p["qmsgType"].as_i64(), p["qmsg"].as_str()),
            (Some(1), Some("đi cà phê nhé"))
        );
        assert_eq!(p["imei"], "imei-1");
        assert!(p.get("qmsgAttach").is_none() && p.get("grid").is_none());
    }

    #[test]
    fn replies_in_a_group_carry_the_quoted_media() {
        let q =
            quote(json!({ "href": "https://p.zdn.vn/a.jpg", "thumb": "https://p.zdn.vn/t.jpg" }));
        let p = reply_params(Recipient::Group("900"), "đẹp quá", &q, 42, "imei-1");
        assert_eq!(
            (p["grid"].as_str(), p["visibility"].as_i64()),
            (Some("900"), Some(0))
        );
        assert_eq!(
            (p["qmsgType"].as_i64(), p["qmsg"].as_str()),
            (Some(32), Some(""))
        );
        let attach: Value = serde_json::from_str(p["qmsgAttach"].as_str().unwrap()).unwrap();
        assert_eq!(attach["thumbUrl"], "https://p.zdn.vn/t.jpg");
        assert_eq!(attach["normalUrl"], "https://p.zdn.vn/a.jpg");
        assert!(p.get("imei").is_none());
    }

    #[test]
    fn message_types() {
        assert_eq!(client_message_type("webchat"), 1);
        assert_eq!(client_message_type("share.file"), 46);
        assert_eq!(client_message_type("chat.video.msg"), 44);
        assert_eq!(client_message_type("something.new"), 1);
    }
}
