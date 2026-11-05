//! Quick messages ("tin nhắn nhanh"): texts saved under a keyword, which the
//! official clients offer when one types `/keyword`. Zalo keeps them on the
//! `quick_message` service, so the phone shows the same list. As zca-js:
//!
//! - List: `GET /api/quickmessage/list` with `{ version: 0, lang: 0, imei }`
//!   answers `{ cursor, version, items: [{ id, keyword, type, message: { title,
//!   params }, media, createdTime, lastModified }] }`.
//! - Add: `GET /api/quickmessage/create` with `{ keyword, message: { title,
//!   params: "" }, type: 0, imei }` answers `{ item, version }`.
//! - Change: `GET /api/quickmessage/update` with `{ itemId, keyword, message,
//!   type: 0 }`, the same answer.
//! - Delete: `GET /api/quickmessage/delete` with `{ itemIds: [id] }`.
//!
//! Items of `type` 1 also carry a photo (`media.items[0]`). Changing one with
//! [`ZaloClient::update_quick_message`] sends `type` 0, which would drop the
//! photo, so callers leave those to the phone.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::api::first_str;
use crate::client::ZaloClient;
use crate::domains::Service;
use crate::error::{Error, Result};

/// One quick message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuickMessage {
    pub id: u64,
    /// What follows `/` to call it.
    pub keyword: String,
    pub text: String,
    /// A photo saved with it, on Zalo's CDN; empty for most.
    pub photo: String,
    /// Milliseconds since the epoch, 0 when unknown.
    pub updated_at: i64,
}

fn number(v: &Value, key: &str) -> i64 {
    match v.get(key) {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

fn item_of(v: &Value) -> Option<QuickMessage> {
    let id = u64::try_from(number(v, "id")).ok().filter(|id| *id > 0)?;
    let keyword = first_str(v, &["keyword"]).trim().to_string();
    let text = v
        .get("message")
        .map(|m| first_str(m, &["title"]))
        .unwrap_or_default();
    let photo = v
        .pointer("/media/items/0")
        .map(|p| first_str(p, &["normalUrl", "hdUrl", "rawUrl", "thumbUrl"]))
        .unwrap_or_default()
        .replace("http://", "https://");
    let updated_at = [number(v, "lastModified"), number(v, "createdTime")]
        .into_iter()
        .find(|t| *t > 0)
        .unwrap_or(0);
    (!keyword.is_empty()).then_some(QuickMessage {
        id,
        keyword,
        text,
        photo,
        updated_at,
    })
}

fn items_of(data: &Value) -> Vec<QuickMessage> {
    data.get("items")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(item_of).collect())
        .unwrap_or_default()
}

/// The item of an add or change answer (`{ item, version }`).
fn saved_item(data: &Value) -> Result<QuickMessage> {
    data.get("item")
        .and_then(item_of)
        .ok_or_else(|| Error::Protocol("Zalo không trả về tin nhắn nhanh vừa lưu".into()))
}

/// A keyword as Zalo's clients take it: no spaces, at most 20 characters.
pub fn keyword(input: &str) -> Option<String> {
    let k = input.trim().trim_start_matches('/').trim();
    let ok = !k.is_empty() && k.chars().count() <= 20 && !k.chars().any(char::is_whitespace);
    ok.then(|| k.to_string())
}

impl ZaloClient {
    /// Every quick message of the account.
    pub async fn quick_messages(&self) -> Result<Vec<QuickMessage>> {
        let params = json!({ "version": 0, "lang": 0, "imei": self.device().imei });
        let data = self
            .get_encrypted(Service::QuickMessage, "/api/quickmessage/list", &params)
            .await?;
        Ok(items_of(&data))
    }

    /// Save a quick message.
    pub async fn add_quick_message(&self, keyword: &str, text: &str) -> Result<QuickMessage> {
        let params = json!({
            "keyword": keyword,
            "message": { "title": text, "params": "" },
            "type": 0,
            "imei": self.device().imei,
        });
        let data = self
            .get_encrypted(Service::QuickMessage, "/api/quickmessage/create", &params)
            .await?;
        saved_item(&data)
    }

    /// Change a quick message's keyword and text.
    pub async fn update_quick_message(
        &self,
        id: u64,
        keyword: &str,
        text: &str,
    ) -> Result<QuickMessage> {
        let params = json!({
            "itemId": id,
            "keyword": keyword,
            "message": { "title": text, "params": "" },
            "type": 0,
        });
        let data = self
            .get_encrypted(Service::QuickMessage, "/api/quickmessage/update", &params)
            .await?;
        saved_item(&data)
    }

    /// Delete a quick message.
    pub async fn remove_quick_message(&self, id: u64) -> Result<()> {
        let params = json!({ "itemIds": [id] });
        match self
            .get_encrypted(Service::QuickMessage, "/api/quickmessage/delete", &params)
            .await
        {
            Ok(_) | Err(Error::EmptyResponse { .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_items() {
        let data = json!({ "cursor": 0, "version": 3, "items": [
            { "id": 11, "keyword": "chao", "type": 0, "createdTime": 5, "lastModified": 9,
              "message": { "title": "Xin chào, em có thể giúp gì?", "params": null }, "media": null },
            { "id": "12", "keyword": "stk", "type": 1, "createdTime": 6,
              "message": { "title": "STK của em" },
              "media": { "items": [{ "normalUrl": "http://p/1.jpg" }] } },
            { "id": 0, "keyword": "x" },
            { "id": 13, "keyword": " " }
        ] });
        let items = items_of(&data);
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0],
            QuickMessage {
                id: 11,
                keyword: "chao".into(),
                text: "Xin chào, em có thể giúp gì?".into(),
                photo: String::new(),
                updated_at: 9,
            }
        );
        assert_eq!(items[1].photo, "https://p/1.jpg");
        assert_eq!(items[1].updated_at, 6);
        assert!(saved_item(&json!({ "item": data["items"][0] })).is_ok());
        assert!(saved_item(&json!({})).is_err());
    }

    #[test]
    fn keywords() {
        assert_eq!(keyword(" /chao ").as_deref(), Some("chao"));
        assert_eq!(keyword("cảm-ơn").as_deref(), Some("cảm-ơn"));
        assert_eq!(keyword("hai từ"), None);
        assert_eq!(keyword("/"), None);
        assert_eq!(keyword(&"a".repeat(21)), None);
    }
}
