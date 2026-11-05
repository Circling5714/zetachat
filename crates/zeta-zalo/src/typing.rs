//! "Đang soạn tin…": telling a chat that this account is typing, and reading
//! who else is.
//!
//! As in zca-js: `POST {chat}/api/message/typing` with `toid`, `destType: 3`
//! and `imei`; in a group `POST {group}/api/group/typing` with `grid` and
//! `imei`. Others typing come in frames `602`: `data.actions[]` with
//! `act_type: "typing"`, `act: "typing"` (1-1) or `"gtyping"` (group), and a
//! `data` holding `uid`, `ts`, `isPC` and, in a group, `gid`. zca-js reads
//! that `data` as JSON text without its braces; objects and complete JSON
//! text are accepted too.

use serde_json::{json, Map, Value};

use crate::api::{first_str, Recipient};
use crate::client::ZaloClient;
use crate::domains::Service;
use crate::error::Result;

/// Someone typing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Typing {
    pub uid: String,
    /// The group, for group chats.
    pub group: Option<String>,
}

/// The `data` of an action as an object.
fn action_data(data: Option<&Value>) -> Option<Value> {
    match data? {
        Value::Object(_) => data.cloned(),
        Value::String(text) => {
            let text = text.trim();
            serde_json::from_str::<Value>(text)
                .ok()
                .filter(Value::is_object)
                .or_else(|| serde_json::from_str(&format!("{{{text}}}")).ok())
        }
        _ => None,
    }
}

/// Typing announcements in a frame payload (`cmd 602`).
pub fn typing_events(payload: &Value) -> Vec<Typing> {
    let data = payload
        .get("data")
        .filter(|d| d.is_object())
        .unwrap_or(payload);
    let Some(actions) = data.get("actions").and_then(Value::as_array) else {
        return Vec::new();
    };
    actions
        .iter()
        .filter(|a| a.get("act_type").and_then(Value::as_str) == Some("typing"))
        .filter_map(|a| {
            let group = match a.get("act").and_then(Value::as_str) {
                Some("typing") => false,
                Some("gtyping") => true,
                _ => return None,
            };
            let data = action_data(a.get("data"))?;
            let uid = first_str(&data, &["uid"]);
            if uid.is_empty() {
                return None;
            }
            let group = if group {
                let gid = first_str(&data, &["gid", "grid"]);
                if gid.is_empty() {
                    return None;
                }
                Some(gid)
            } else {
                None
            };
            Some(Typing { uid, group })
        })
        .collect()
}

impl ZaloClient {
    /// Tell the chat that this account is typing (Zalo shows it for a few
    /// seconds; send again while typing goes on).
    pub async fn send_typing(&self, to: Recipient<'_>) -> Result<()> {
        let key = self.session_key()?;
        let mut params = Map::new();
        let (service, path) = match to {
            Recipient::User(uid) => {
                params.insert("toid".into(), json!(uid));
                params.insert("destType".into(), json!(3));
                (Service::Chat, "/api/message/typing")
            }
            Recipient::Group(grid) => {
                params.insert("grid".into(), json!(grid));
                (Service::Group, "/api/group/typing")
            }
        };
        params.insert("imei".into(), json!(self.device().imei));
        let url = format!(
            "{}{path}?{}",
            self.domains().primary(service),
            Self::common_params()
        );
        let form = format!("params={}", self.encrypt_params(&Value::Object(params))?);
        self.post_form(&url, form, Some(key)).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_in_every_shape_zalo_may_use() {
        let payload = json!({ "data": { "actions": [
            { "act_type": "typing", "act": "typing", "data": "\"uid\":\"100\",\"ts\":\"1\",\"isPC\":0" },
            { "act_type": "typing", "act": "gtyping", "data": "{\"uid\":\"200\",\"gid\":\"900\"}" },
            { "act_type": "typing", "act": "gtyping", "data": { "uid": 300, "gid": 901 } },
            { "act_type": "typing", "act": "gtyping", "data": "\"uid\":\"400\"" },
            { "act_type": "other", "act": "typing", "data": "\"uid\":\"500\"" },
            { "act_type": "typing", "act": "typing", "data": "not json" }
        ]}});
        assert_eq!(
            typing_events(&payload),
            [
                Typing {
                    uid: "100".into(),
                    group: None
                },
                Typing {
                    uid: "200".into(),
                    group: Some("900".into())
                },
                Typing {
                    uid: "300".into(),
                    group: Some("901".into())
                },
            ]
        );
        assert!(typing_events(&json!({ "data": {} })).is_empty());
    }
}
