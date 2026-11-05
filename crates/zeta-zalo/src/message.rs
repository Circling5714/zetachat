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

use crate::api::{first_str, mention_info, Mention, Recipient, SentMessage, TextExtras};
use crate::client::ZaloClient;
use crate::domains::Service;
use crate::error::{Error, Result};
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
    mentions: &[Mention],
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
            if !mentions.is_empty() {
                p.insert("mentionInfo".into(), json!(mention_info(mentions)));
            }
        }
    }
    p.insert("qmsgTTL".into(), json!(quote.ttl));
    p.insert("ttl".into(), json!(0));
    p
}

/// What `mforward` answered for our one recipient: `success: [{ clientId,
/// msgId }]` or `fail: [{ clientId, error_code }]`.
fn forward_answer(data: &Value, client_id: u64) -> Result<SentMessage> {
    let list = |key: &str| {
        data.get(key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    if let Some(failed) = list("fail").first() {
        let code = match failed.get("error_code") {
            Some(Value::Number(n)) => n.as_i64().unwrap_or(-1),
            Some(Value::String(s)) => s.trim().parse().unwrap_or(-1),
            _ => -1,
        };
        return Err(Error::from_code(
            code,
            "Zalo không chuyển tiếp được tin này",
        ));
    }
    let msg_id = list("success")
        .first()
        .map(|s| first_str(s, &["msgId"]))
        .unwrap_or_default();
    Ok(SentMessage { msg_id, client_id })
}

impl ZaloClient {
    /// Send `text` as a reply to `quote`.
    pub async fn send_reply(
        &self,
        to: Recipient<'_>,
        text: &str,
        quote: &QuotedMessage,
    ) -> Result<SentMessage> {
        self.send_reply_with(to, text, quote, &TextExtras::default())
            .await
    }

    /// [`Self::send_reply`] with extras: people named (groups only), styled
    /// runs, urgency.
    pub async fn send_reply_with(
        &self,
        to: Recipient<'_>,
        text: &str,
        quote: &QuotedMessage,
        extras: &TextExtras,
    ) -> Result<SentMessage> {
        let key = self.session_key()?;
        let client_id = now_millis();
        let imei = &self.device().imei;
        let mut params = reply_params(to, text, quote, client_id, imei, &extras.mentions);
        extras.apply(&mut params);
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

    /// Forward a message to one conversation: `POST {file}/api/message/mforward`
    /// or `{file}/api/group/mforward`, as zca-js `forwardMessage`. `msg_type`
    /// numbers kinds as for quotes ([`client_message_type`]); `msg_info` is
    /// `{ "message": text }` for text.
    pub async fn forward(
        &self,
        to: Recipient<'_>,
        msg_type: i64,
        msg_info: &Value,
    ) -> Result<SentMessage> {
        let key = self.session_key()?;
        let client_id = now_millis();
        let mut p = Map::new();
        let path = match to {
            Recipient::User(uid) => {
                p.insert(
                    "toIds".into(),
                    json!([{ "clientId": client_id.to_string(), "toUid": uid, "ttl": 0 }]),
                );
                p.insert("imei".into(), json!(self.device().imei));
                "/api/message/mforward"
            }
            Recipient::Group(grid) => {
                p.insert(
                    "grids".into(),
                    json!([{ "clientId": client_id.to_string(), "grid": grid, "ttl": 0 }]),
                );
                "/api/group/mforward"
            }
        };
        p.insert("ttl".into(), json!(0));
        p.insert("msgType".into(), json!(msg_type.to_string()));
        p.insert("totalIds".into(), json!(1));
        p.insert("msgInfo".into(), json!(msg_info.to_string()));
        p.insert("decorLog".into(), json!("null"));
        let form = format!("params={}", self.encrypt_params(&Value::Object(p))?);
        let data = self
            .post_service(Service::File, path, form, Some(key))
            .await?;
        forward_answer(&data, client_id)
    }

    /// Delete a message for this account only, as zca-js `deleteMessage` with
    /// `onlyMe`: `POST {chat}/api/message/delete` or `{group}/api/group/deletemsg`.
    /// `owner_id` is who sent it (this account's uid for its own messages).
    pub async fn delete_for_me(
        &self,
        to: Recipient<'_>,
        msg_id: &str,
        cli_msg_id: &str,
        owner_id: &str,
    ) -> Result<()> {
        let key = self.session_key()?;
        let dest = match to {
            Recipient::User(uid) => uid,
            Recipient::Group(grid) => grid,
        };
        let mut p = Map::new();
        let (service, path) = match to {
            Recipient::User(uid) => {
                p.insert("toid".into(), json!(uid));
                (Service::Chat, "/api/message/delete")
            }
            Recipient::Group(grid) => {
                p.insert("grid".into(), json!(grid));
                (Service::Group, "/api/group/deletemsg")
            }
        };
        p.insert("cliMsgId".into(), json!(now_millis()));
        p.insert(
            "msgs".into(),
            json!([{
                "cliMsgId": cli_msg_id,
                "globalMsgId": msg_id,
                "ownerId": owner_id,
                "destId": dest,
            }]),
        );
        p.insert("onlyMe".into(), json!(1));
        if matches!(to, Recipient::User(_)) {
            p.insert("imei".into(), json!(self.device().imei));
        }
        let url = format!(
            "{}{path}?{}",
            self.domains().primary(service),
            Self::common_params()
        );
        let form = format!("params={}", self.encrypt_params(&Value::Object(p))?);
        self.post_form(&url, form, Some(key)).await?;
        Ok(())
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
        let p = reply_params(Recipient::User("100"), "ok", &q, 42, "imei-1", &[]);
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
        let p = reply_params(Recipient::Group("900"), "đẹp quá", &q, 42, "imei-1", &[]);
        assert!(p.get("mentionInfo").is_none());
        let named = [Mention {
            uid: "7".into(),
            pos: 0,
            len: 3,
        }];
        let p = reply_params(Recipient::Group("900"), "@An đẹp", &q, 42, "imei-1", &named);
        assert!(p["mentionInfo"].as_str().unwrap().contains("\"uid\":\"7\""));
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
    fn forward_answers() {
        let ok = json!({ "success": [{ "clientId": "5", "msgId": "900" }], "fail": [] });
        assert_eq!(forward_answer(&ok, 5).unwrap().msg_id, "900");
        let failed = json!({ "success": [], "fail": [{ "clientId": "5", "error_code": "114" }] });
        assert!(matches!(
            forward_answer(&failed, 5),
            Err(Error::Server { code: 114, .. })
        ));
        assert_eq!(forward_answer(&json!({}), 5).unwrap().msg_id, "");
    }

    #[test]
    fn message_types() {
        assert_eq!(client_message_type("webchat"), 1);
        assert_eq!(client_message_type("share.file"), 46);
        assert_eq!(client_message_type("chat.video.msg"), 44);
        assert_eq!(client_message_type("something.new"), 1);
    }
}
