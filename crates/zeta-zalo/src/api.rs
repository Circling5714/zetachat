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

/// When a user was last on Zalo, as far as they let others see.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastOnline {
    /// They show when they are online (a privacy setting).
    pub visible: bool,
    /// Milliseconds since the epoch; 0 when unknown or hidden.
    pub at: i64,
}

/// `lastOnline` answers: `{ settings: { show_online_status }, lastOnline }`,
/// the time in seconds or milliseconds.
fn last_online_of(data: &Value) -> LastOnline {
    let visible = data
        .pointer("/settings/show_online_status")
        .and_then(|v| v.as_bool().or_else(|| v.as_i64().map(|n| n != 0)))
        .unwrap_or(true);
    let raw = match data.get("lastOnline") {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    };
    tracing::debug!(
        visible,
        sign = raw.signum(),
        digits = raw.unsigned_abs().to_string().len(),
        "last online answer"
    );
    let at = match raw {
        r if r <= 0 || !visible => 0,
        r if r < 10_000_000_000 => r * 1000,
        r => r,
    };
    LastOnline { visible, at }
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

/// Someone named in a group message: `len` UTF-16 units from `pos` (Zalo's
/// clients count like JavaScript strings). `uid` "-1" names everyone.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mention {
    pub uid: String,
    pub pos: u32,
    pub len: u32,
}

/// `mentionInfo` as zca-js sends it: JSON text of `[{ pos, uid, len, type }]`,
/// `type` 1 for everyone.
pub(crate) fn mention_info(mentions: &[Mention]) -> String {
    let list: Vec<Value> = mentions
        .iter()
        .map(|m| {
            json!({
                "pos": m.pos,
                "uid": m.uid,
                "len": m.len,
                "type": if m.uid == "-1" { 1 } else { 0 },
            })
        })
        .collect();
    Value::Array(list).to_string()
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
    /// Members as far as Zalo lists them (`memVerList`): the creator first,
    /// then the admins, then the others.
    #[serde(default)]
    pub member_ids: Vec<String>,
    #[serde(default)]
    pub admin_ids: Vec<String>,
    #[serde(default)]
    pub creator_id: String,
}

/// User ids in a list of `"<uid>"`, `<uid>` or `"<uid>_<version>"`.
fn id_list(v: Option<&Value>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in v.and_then(Value::as_array).into_iter().flatten() {
        let id = match item {
            Value::String(s) => s.split('_').next().unwrap_or_default().trim().to_string(),
            Value::Number(n) => n.to_string(),
            _ => continue,
        };
        if !id.is_empty() && !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

impl Group {
    pub(crate) fn from_info(id: &str, info: &Value) -> Self {
        let avatar = first_str(info, &["fullAvt", "avt", "avatar"]);
        let creator_id = first_str(info, &["creatorId"]);
        let admin_ids = id_list(info.get("adminIds"));
        let listed = match id_list(info.get("memVerList")) {
            ids if ids.is_empty() => id_list(info.get("memberIds")),
            ids => ids,
        };
        let rank = |uid: &String| {
            if *uid == creator_id {
                0
            } else if admin_ids.contains(uid) {
                1
            } else {
                2
            }
        };
        let mut member_ids = listed;
        // Stable: within a rank, Zalo's order stays.
        member_ids.sort_by_key(rank);
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
            member_ids,
            admin_ids,
            creator_id,
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

/// `profiles` of a `social/group/members` answer: an object keyed by user id.
fn profiles_of(data: &Value) -> Vec<Friend> {
    let profiles = data
        .get("profiles")
        .or_else(|| inner_data(data).get("profiles"))
        .and_then(Value::as_object);
    let Some(profiles) = profiles else {
        return Vec::new();
    };
    profiles
        .iter()
        .filter(|(uid, p)| !uid.is_empty() && p.is_object())
        .map(|(uid, p)| Friend {
            uid: uid.clone(),
            display_name: first_str(p, &["displayName", "zaloName", "dName"]),
            zalo_name: first_str(p, &["zaloName"]),
            avatar: first_str(p, &["avatar", "avt"]),
        })
        .collect()
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

    /// Name and avatar of users, friends or not: `social/group/members`, the call
    /// Zalo PC makes for group members it does not know (ctdozlite notes that
    /// `getminiprofiles` answers 114 for plain ids). At most 50 ids are asked;
    /// users Zalo does not answer for are left out.
    pub async fn user_profiles(&self, uids: &[String]) -> Result<Vec<Friend>> {
        let asked: Vec<String> = uids
            .iter()
            .filter(|uid| !uid.is_empty())
            .take(50)
            .map(|uid| format!("{uid}_0"))
            .collect();
        if asked.is_empty() {
            return Ok(Vec::new());
        }
        let key = self.session_key()?;
        let params = self.encrypt_params(&json!({ "friend_pversion_map": asked }))?;
        let url = format!(
            "{}/api/social/group/members?{}",
            self.domains().primary(Service::Profile),
            Self::common_params()
        );
        let data = unwrap_json_string(
            self.post_form(&url, format!("params={params}"), Some(key))
                .await?,
        );
        Ok(profiles_of(&data))
    }

    /// When `uid` was last on Zalo (`profile/lastOnline`, as zca-js), unless
    /// they hide it.
    pub async fn last_online(&self, uid: &str) -> Result<LastOnline> {
        let key = self.session_key()?;
        let params = self.encrypt_params(&json!({
            "uid": uid,
            "conv_type": 1,
            "imei": self.device().imei,
        }))?;
        let path = format!(
            "/api/social/profile/lastOnline?{}&params={params}",
            Self::common_params()
        );
        let data = unwrap_json_string(self.get_service(Service::Profile, &path, Some(key)).await?);
        Ok(last_online_of(inner_data(&data)))
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
        let ids = self.group_ids().await?;
        self.group_info(&ids).await
    }

    /// Details of the groups `ids` (`getmg-v2`, 50 at a time): name, avatar,
    /// size, members, creator, admins. A version of 0 makes Zalo answer for
    /// every group asked.
    pub async fn group_info(&self, ids: &[String]) -> Result<Vec<Group>> {
        let key = self.session_key()?;
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
        self.send_text_with(to, text, &[]).await
    }

    /// Send a text naming people (groups only, as zca-js): with mentions it
    /// goes to `group/mention` with `mentionInfo`, else as [`Self::send_text`].
    pub async fn send_text_with(
        &self,
        to: Recipient<'_>,
        text: &str,
        mentions: &[Mention],
    ) -> Result<SentMessage> {
        let key = self.session_key()?;
        let client_id = now_millis();
        let imei = &self.device().imei;
        let (service, path, payload) = match to {
            Recipient::User(uid) => (
                Service::Chat,
                "/api/message/sms",
                json!({ "toid": uid, "message": text, "clientId": client_id, "imei": imei }),
            ),
            Recipient::Group(grid) if !mentions.is_empty() => (
                Service::Group,
                "/api/group/mention",
                json!({
                    "grid": grid, "message": text, "clientId": client_id,
                    "mentionInfo": mention_info(mentions),
                    "imei": imei, "ttl": 0, "visibility": 0
                }),
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
        assert!(unknown.member_ids.is_empty());
    }

    #[test]
    fn group_members_put_the_creator_and_admins_first() {
        let g = Group::from_info(
            "7",
            &json!({
                "creatorId": "30",
                "adminIds": ["20", 40],
                "memVerList": ["10_0", "20_3", "30_1", "40_0", "10_2", "", 5],
            }),
        );
        assert_eq!(g.member_ids, ["30", "20", "40", "10", "5"]);
        assert_eq!((g.creator_id.as_str(), g.admin_ids.len()), ("30", 2));
        // Without versions, `memberIds` is used.
        let plain = Group::from_info("8", &json!({ "memberIds": ["1", "2"] }));
        assert_eq!(plain.member_ids, ["1", "2"]);
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

    #[test]
    fn mention_info_as_zca_js_sends_it() {
        let info = mention_info(&[
            Mention {
                uid: "7".into(),
                pos: 0,
                len: 4,
            },
            Mention {
                uid: "-1".into(),
                pos: 5,
                len: 8,
            },
        ]);
        let v: Value = serde_json::from_str(&info).unwrap();
        assert_eq!(
            v,
            json!([
                { "pos": 0, "uid": "7", "len": 4, "type": 0 },
                { "pos": 5, "uid": "-1", "len": 8, "type": 1 }
            ])
        );
    }

    #[test]
    fn last_online_in_seconds_or_milliseconds() {
        let seen = |v: Value| last_online_of(&v);
        assert_eq!(
            seen(
                json!({ "settings": { "show_online_status": true }, "lastOnline": 1_700_000_000 })
            ),
            LastOnline {
                visible: true,
                at: 1_700_000_000_000
            }
        );
        assert_eq!(
            seen(json!({ "lastOnline": "1700000000123" })).at,
            1_700_000_000_123
        );
        // Hidden: no time, even if one came.
        assert_eq!(
            seen(json!({ "settings": { "show_online_status": false }, "lastOnline": 5 })),
            LastOnline {
                visible: false,
                at: 0
            }
        );
        assert_eq!(
            seen(json!({})),
            LastOnline {
                visible: true,
                at: 0
            }
        );
    }

    #[test]
    fn member_profiles_are_keyed_by_user_id() {
        let data = json!({
            "profiles": {
                "11": { "displayName": "Mẹ", "zaloName": "Me Lan", "avatar": "https://a/1.jpg" },
                "12": { "zaloName": "Minh" },
                "13": "?"
            },
            "unchangeds_profile": []
        });
        let mut found = profiles_of(&data);
        found.sort_by(|a, b| a.uid.cmp(&b.uid));
        let names: Vec<_> = found
            .iter()
            .map(|f| (f.uid.as_str(), f.display_name.as_str(), f.avatar.as_str()))
            .collect();
        assert_eq!(names, [("11", "Mẹ", "https://a/1.jpg"), ("12", "Minh", "")]);
        // Some answers wrap everything in one more `data`.
        assert_eq!(profiles_of(&json!({ "data": data })).len(), 2);
        assert!(profiles_of(&json!({})).is_empty());
    }
}
