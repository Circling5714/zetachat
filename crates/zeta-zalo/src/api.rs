//! First set of authenticated calls. Each needs a logged-in [`ZaloClient`].

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::client::ZaloClient;
use crate::domains::Service;
use crate::error::{Error, Result};
use crate::now_millis;

/// The logged-in account.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Profile {
    pub uid: String,
    pub display_name: String,
    pub avatar: String,
}

/// One entry of the friend list.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Friend {
    pub uid: String,
    pub display_name: String,
    pub zalo_name: String,
    pub avatar: String,
}

/// Verdict of [`ZaloClient::check_session`].
#[derive(Clone, Debug)]
pub enum SessionHealth {
    /// Zalo answered with the profile: the session works.
    Valid(Profile),
    /// Zalo refused the session (`600` / `-102`): log in again.
    Rejected,
    /// No verdict (network, unexpected answer). Never discard a session for this.
    Unknown(String),
}

/// Who receives a message.
#[derive(Clone, Copy, Debug)]
pub enum Recipient<'a> {
    User(&'a str),
    Group(&'a str),
}

/// Result of a send.
#[derive(Clone, Debug)]
pub struct SentMessage {
    /// Server message id, empty if the answer had none.
    pub msg_id: String,
    /// Client id sent with the message, to match the server echo.
    pub client_id: u64,
}

/// First non-empty string (or number) among `keys`.
pub(crate) fn first_str(v: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|k| match v.get(*k) {
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            Some(Value::Number(n)) => Some(n.to_string()),
            _ => None,
        })
        .unwrap_or_default()
}

impl Profile {
    /// `me-v2` data: `{profile: {...}}` or the profile itself.
    pub(crate) fn from_data(data: &Value) -> Option<Self> {
        let p = data.get("profile").unwrap_or(data);
        let uid = first_str(p, &["userId", "uid"]);
        if uid.is_empty() {
            return None;
        }
        Some(Self {
            uid,
            display_name: first_str(p, &["displayName", "zaloName"]),
            avatar: first_str(p, &["avatar", "avt"]),
        })
    }
}

/// One group the account belongs to.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Group {
    pub id: String,
    pub name: String,
    pub avatar: String,
    pub member_count: u64,
}

impl Group {
    pub(crate) fn from_info(id: &str, info: &Value) -> Self {
        let avatar = first_str(info, &["fullAvt", "avt", "avatar"]);
        Self {
            id: id.to_string(),
            name: first_str(info, &["name", "groupName", "displayName"]),
            // Zalo returns a placeholder URL ending in /default for groups without a photo.
            avatar: if avatar.ends_with("/default") {
                String::new()
            } else {
                avatar
            },
            member_count: info.get("totalMember").and_then(Value::as_u64).unwrap_or(0),
        }
    }
}

/// Group endpoints sometimes double-encode `data` as a JSON string.
fn unwrap_json_string(v: Value) -> Value {
    match &v {
        Value::String(s) => serde_json::from_str(s.trim()).unwrap_or(v),
        _ => v,
    }
}

/// `data.data` when present, else the value itself.
fn inner_data(v: &Value) -> &Value {
    v.get("data").filter(|d| d.is_object()).unwrap_or(v)
}

impl Friend {
    pub(crate) fn from_value(v: &Value) -> Option<Self> {
        let uid = first_str(v, &["userId", "uid"]);
        if uid.is_empty() {
            return None;
        }
        Some(Self {
            uid,
            display_name: first_str(v, &["displayName", "zaloName"]),
            zalo_name: first_str(v, &["zaloName"]),
            avatar: first_str(v, &["avatar", "avt"]),
        })
    }
}

impl ZaloClient {
    /// `me-v2`: the logged-in profile. Success requires an actual profile in the
    /// answer, so a session Zalo silently ignores is never reported as working.
    pub async fn me(&self) -> Result<Profile> {
        let key = self.session_key()?;
        let params =
            self.encrypt_params(&json!({ "avatar_size": 160, "imei": self.device().imei }))?;
        let path = format!(
            "/api/social/profile/me-v2?{}&params={params}&os=8",
            Self::common_params()
        );
        let data = self.get_service(Service::Profile, &path, Some(key)).await?;
        Profile::from_data(&data)
            .ok_or_else(|| Error::Protocol("me-v2 answered without a profile".into()))
    }

    /// Ask Zalo whether the session still works.
    pub async fn check_session(&self) -> SessionHealth {
        match self.me().await {
            Ok(profile) => SessionHealth::Valid(profile),
            Err(e) if e.is_session_rejected() => SessionHealth::Rejected,
            Err(e) => SessionHealth::Unknown(e.to_string()),
        }
    }

    /// Friend list (`getfriends`). Zalo rate-limits this call: do not poll it.
    pub async fn friends(&self) -> Result<Vec<Friend>> {
        let key = self.session_key()?;
        let params = self.encrypt_params(&json!({
            "incInvalid": 1,
            "page": 1,
            "count": 20000,
            "avatar_size": 160,
            "actiontime": 0,
            "imei": self.device().imei,
        }))?;
        let path = format!(
            "/api/social/friend/getfriends?{}&params={params}",
            Self::common_params()
        );
        let data = self.get_service(Service::Friend, &path, Some(key)).await?;
        let items = match &data {
            Value::Array(items) => items.clone(),
            Value::Object(o) => o
                .get("friends")
                .or_else(|| o.get("data"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        Ok(items.iter().filter_map(Friend::from_value).collect())
    }

    /// Ids of the groups the account belongs to (`getlg/v4`).
    pub async fn group_ids(&self) -> Result<Vec<String>> {
        let key = self.session_key()?;
        let path = format!("/api/group/getlg/v4?{}", Self::common_params());
        let data = unwrap_json_string(self.get_service(Service::Group, &path, Some(key)).await?);
        Ok(inner_data(&data)
            .get("gridVerMap")
            .and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default())
    }

    /// Groups with name, avatar and size (`getlg/v4`, then `getmg-v2` in batches).
    pub async fn groups(&self) -> Result<Vec<Group>> {
        let key = self.session_key()?;
        let ids = self.group_ids().await?;
        let url = format!(
            "{}/api/group/getmg-v2?{}",
            self.domains().primary(Service::Group),
            Self::common_params()
        );
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(50) {
            let versions: serde_json::Map<String, Value> =
                chunk.iter().map(|id| (id.clone(), json!(0))).collect();
            // The bundle sends the map as a JSON string inside the payload.
            let params = self.encrypt_params(&json!({
                "gridVerMap": Value::Object(versions).to_string(),
                "imei": self.device().imei,
            }))?;
            let data = unwrap_json_string(
                self.post_form(&url, format!("params={params}"), Some(key))
                    .await?,
            );
            let infos = inner_data(&data)
                .get("gridInfoMap")
                .and_then(Value::as_object);
            for id in chunk {
                let info = infos.and_then(|m| m.get(id)).unwrap_or(&Value::Null);
                out.push(Group::from_info(id, info));
            }
        }
        Ok(out)
    }

    /// Latest messages of the recent conversations
    /// (`preloadconvers/get-last-msgs`), the call Zalo PC makes at start-up to
    /// paint its conversation list. An empty `threadIdLocalMsgId` map asks for all
    /// recent conversations. The payload is shaped like real-time frames
    /// (`msgs`, `groupMsgs`).
    pub async fn recent_messages(&self) -> Result<Value> {
        let key = self.session_key()?;
        let params = self.encrypt_params(&json!({
            "threadIdLocalMsgId": "{}",
            "imei": self.device().imei,
        }))?;
        let path = format!(
            "/api/preloadconvers/get-last-msgs?{}&params={params}",
            Self::common_params()
        );
        Ok(unwrap_json_string(
            self.get_service(Service::Conversation, &path, Some(key))
                .await?,
        ))
    }

    /// Tell Zalo the account has seen these incoming messages (`seenv2`, as in
    /// zca-js): their senders see "Đã xem" and the account's other devices clear
    /// their unread marks. `messages` are message objects as Zalo sent them; at
    /// most 50 are used.
    pub async fn send_seen(&self, to: Recipient<'_>, messages: &[Value]) -> Result<()> {
        if messages.is_empty() {
            return Ok(());
        }
        let key = self.session_key()?;
        let own_uid = self.session().map(|s| s.uid.clone()).unwrap_or_default();
        let field = |m: &Value, k: &str| m.get(k).cloned().unwrap_or(Value::Null);
        let data: Vec<Value> = messages
            .iter()
            .take(50)
            .map(|m| {
                let id_to = first_str(m, &["idTo"]);
                json!({
                    "cmi": first_str(m, &["cliMsgId"]),
                    "gmi": first_str(m, &["msgId"]),
                    "si": first_str(m, &["uidFrom"]),
                    "di": if id_to == own_uid { "0".to_string() } else { id_to },
                    "mt": field(m, "msgType"),
                    "st": field(m, "st"),
                    "at": field(m, "at"),
                    "cmd": field(m, "cmd"),
                    "ts": field(m, "ts"),
                })
            })
            .collect();
        let (service, path, params) = match to {
            Recipient::User(uid) => (
                Service::Chat,
                "/api/message/seenv2",
                json!({ "msgInfos": json!({ "data": data }).to_string(), "senderId": uid }),
            ),
            Recipient::Group(grid) => (
                Service::Group,
                "/api/group/seenv2",
                json!({
                    "msgInfos": json!({ "data": data, "grid": grid }).to_string(),
                    "imei": self.device().imei,
                }),
            ),
        };
        let url = format!(
            "{}{path}?{}&nretry=0",
            self.domains().primary(service),
            Self::common_params()
        );
        let form = format!("params={}", self.encrypt_params(&params)?);
        self.post_form(&url, form, Some(key)).await?;
        Ok(())
    }

    /// Send a text message.
    ///
    /// Sent to the primary host only: a send that timed out may still have been
    /// delivered, so retrying on another host could duplicate it.
    pub async fn send_text(&self, to: Recipient<'_>, text: &str) -> Result<SentMessage> {
        let key = self.session_key()?;
        let client_id = now_millis();
        let imei = &self.device().imei;
        let (service, path, payload) = match to {
            Recipient::User(uid) => (
                Service::Chat,
                "/api/message/sms",
                json!({ "toid": uid, "message": text, "clientId": client_id, "imei": imei }),
            ),
            Recipient::Group(grid) => (
                Service::Group,
                "/api/group/sendmsg",
                json!({
                    "grid": grid, "message": text, "clientId": client_id,
                    "imei": imei, "visibility": 0
                }),
            ),
        };
        let url = format!(
            "{}{path}?{}",
            self.domains().primary(service),
            Self::common_params()
        );
        let form = format!("params={}", self.encrypt_params(&payload)?);
        let data = self.post_form(&url, form, Some(key)).await?;
        Ok(SentMessage {
            msg_id: first_str(&data, &["msgId"]),
            client_id,
        })
    }
}

#[cfg(test)]
mod group_tests {
    use super::*;

    #[test]
    fn group_info_and_placeholders() {
        let g = Group::from_info(
            "7",
            &json!({ "name": "Nhà", "fullAvt": "https://x/default", "totalMember": 4 }),
        );
        assert_eq!(
            (g.name.as_str(), g.avatar.as_str(), g.member_count),
            ("Nhà", "", 4)
        );
        let unknown = Group::from_info("8", &Value::Null);
        assert_eq!((unknown.id.as_str(), unknown.name.as_str()), ("8", ""));
    }

    #[test]
    fn double_encoded_data_is_unwrapped() {
        let v = unwrap_json_string(json!(r#"{"data":{"gridVerMap":{"1":0}}}"#));
        assert!(inner_data(&v).get("gridVerMap").is_some());
        assert_eq!(unwrap_json_string(json!("plain")), json!("plain"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_needs_an_id() {
        let p = Profile::from_data(&json!({ "profile": { "userId": 12, "displayName": "A" } }))
            .unwrap();
        assert_eq!(p.uid, "12");
        assert_eq!(p.display_name, "A");
        assert!(Profile::from_data(&json!({})).is_none());
        assert!(Profile::from_data(&Value::Null).is_none());
    }

    #[test]
    fn friend_fields_fall_back() {
        let f = Friend::from_value(&json!({ "uid": "9", "zaloName": "Z", "avt": "u" })).unwrap();
        assert_eq!(
            (f.uid.as_str(), f.display_name.as_str(), f.avatar.as_str()),
            ("9", "Z", "u")
        );
        assert!(Friend::from_value(&json!({ "displayName": "nobody" })).is_none());
    }
}
