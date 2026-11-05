//! Finding people by phone number and friend requests, as zca-js and ctdozlite
//! do it (both follow Zalo PC). All on the `friend` service:
//!
//! - Find: `GET /api/friend/profile/get` with `{ phone, avatar_size, language,
//!   imei, reqSrc: 40 }`; a Vietnamese number starting with 0 is sent with 84
//!   instead. Error 216: no account for that number, or its owner does not let
//!   people find them by number.
//! - Relationship: `GET /api/friend/reqstatus` with `{ fid, imei }` answers
//!   `is_friend`, `is_requested` (a request went from us to them) and
//!   `is_requesting` (one came from them).
//! - Send a request: `POST /api/friend/sendreq` with `{ toid, msg, reqsrc: 30,
//!   imei, language }`.
//! - Requests received: `GET /api/friend/recommendsv2/list` with `{ imei }`;
//!   items whose `dataInfo.recommType` is 2 (1 is a suggested friend).
//! - Accept: `POST /api/friend/accept` with `{ fid, language }`; decline:
//!   `POST /api/friend/reject` with `{ fid, imei }`.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::api::{first_str, Friend};
use crate::client::ZaloClient;
use crate::domains::Service;
use crate::error::{Error, Result};

/// Zalo's answer when nobody can be found by a phone number.
const NOT_FOUND: i64 = 216;

/// Where this account stands with a user.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Friendship {
    Friend,
    /// This account asked them; they have not answered.
    RequestSent,
    /// They asked this account.
    RequestReceived,
    None,
}

/// A friend request this account received.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FriendRequest {
    pub uid: String,
    pub display_name: String,
    pub zalo_name: String,
    pub avatar: String,
    /// What they wrote with it; may be empty.
    pub message: String,
    /// When they sent it (ms), 0 when unknown.
    pub time: i64,
}

/// A phone number as Zalo looks it up: digits only, a leading 0 becomes 84.
pub fn phone_for_lookup(raw: &str) -> String {
    let digits: String = raw.chars().filter(char::is_ascii_digit).collect();
    match digits.strip_prefix('0') {
        Some(rest) => format!("84{rest}"),
        None => digits,
    }
}

fn flag(v: &Value, keys: &[&str]) -> bool {
    keys.iter().any(|k| match v.get(*k) {
        Some(Value::Number(n)) => n.as_i64() == Some(1),
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "1" || s == "true",
        _ => false,
    })
}

/// The relationship in a `reqstatus` answer.
fn friendship_of(data: &Value) -> Friendship {
    if flag(data, &["is_friend", "isFriend"]) {
        Friendship::Friend
    } else if flag(data, &["is_requesting", "isRequesting"]) {
        Friendship::RequestReceived
    } else if flag(data, &["is_requested", "isRequested"]) {
        Friendship::RequestSent
    } else {
        Friendship::None
    }
}

/// The user of a `profile/get` answer.
fn found_user(data: &Value) -> Option<Friend> {
    let uid = first_str(data, &["uid", "userId", "id"]);
    if uid.is_empty() {
        return None;
    }
    Some(Friend {
        uid,
        display_name: first_str(
            data,
            &["display_name", "displayName", "zalo_name", "zaloName"],
        ),
        zalo_name: first_str(data, &["zalo_name", "zaloName"]),
        avatar: first_str(data, &["avatar", "avt"]),
    })
}

/// The requests among the items of a `recommendsv2/list` answer.
fn requests_of(data: &Value) -> Vec<FriendRequest> {
    let items = data
        .get("recommItems")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    items
        .iter()
        .filter_map(|item| {
            let info = item.get("dataInfo").unwrap_or(item);
            let kind = info.get("recommType").and_then(Value::as_i64).unwrap_or(0);
            let uid = first_str(info, &["userId", "uid"]);
            (kind == 2 && !uid.is_empty()).then(|| FriendRequest {
                uid,
                display_name: first_str(info, &["displayName", "zaloName"]),
                zalo_name: first_str(info, &["zaloName"]),
                avatar: first_str(info, &["avatar"]),
                message: info
                    .pointer("/recommInfo/message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string(),
                time: info.get("recommTime").and_then(Value::as_i64).unwrap_or(0),
            })
        })
        .collect()
}

impl ZaloClient {
    /// The Zalo account behind a phone number; `None` when there is none, or
    /// its owner does not let people find them by number.
    pub async fn find_user(&self, phone: &str) -> Result<Option<Friend>> {
        let phone = phone_for_lookup(phone);
        if phone.len() < 9 || phone.len() > 15 {
            return Err(Error::Protocol("số điện thoại không hợp lệ".into()));
        }
        let key = self.session_key()?;
        let params = self.encrypt_params(&json!({
            "phone": phone,
            "avatar_size": 240,
            "language": "vi",
            "imei": self.device().imei,
            "reqSrc": 40,
        }))?;
        let path = format!(
            "/api/friend/profile/get?{}&params={params}",
            Self::common_params()
        );
        match self.get_service(Service::Friend, &path, Some(key)).await {
            Ok(data) => Ok(found_user(&data)),
            Err(Error::Server { code, .. }) if code == NOT_FOUND => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Where this account stands with `uid`.
    pub async fn friendship(&self, uid: &str) -> Result<Friendship> {
        let key = self.session_key()?;
        let params = self.encrypt_params(&json!({ "fid": uid, "imei": self.device().imei }))?;
        let path = format!(
            "/api/friend/reqstatus?{}&params={params}",
            Self::common_params()
        );
        let data = self.get_service(Service::Friend, &path, Some(key)).await?;
        Ok(friendship_of(&data))
    }

    /// Ask `uid` to be friends, with a few words.
    pub async fn send_friend_request(&self, uid: &str, message: &str) -> Result<()> {
        let key = self.session_key()?;
        let params = self.encrypt_params(&json!({
            "toid": uid,
            "msg": message,
            "reqsrc": 30,
            "imei": self.device().imei,
            "language": "vi",
        }))?;
        self.post_service(
            Service::Friend,
            "/api/friend/sendreq",
            format!("params={params}"),
            Some(key),
        )
        .await?;
        Ok(())
    }

    /// Friend requests this account received and has not answered.
    pub async fn friend_requests(&self) -> Result<Vec<FriendRequest>> {
        let key = self.session_key()?;
        let params = self.encrypt_params(&json!({ "imei": self.device().imei }))?;
        let path = format!(
            "/api/friend/recommendsv2/list?{}&params={params}",
            Self::common_params()
        );
        let data = self.get_service(Service::Friend, &path, Some(key)).await?;
        Ok(requests_of(&data))
    }

    /// Accept the friend request of `uid`.
    pub async fn accept_friend_request(&self, uid: &str) -> Result<()> {
        let key = self.session_key()?;
        let params = self.encrypt_params(&json!({ "fid": uid, "language": "vi" }))?;
        self.post_service(
            Service::Friend,
            "/api/friend/accept",
            format!("params={params}"),
            Some(key),
        )
        .await?;
        Ok(())
    }

    /// Decline the friend request of `uid`.
    pub async fn reject_friend_request(&self, uid: &str) -> Result<()> {
        let key = self.session_key()?;
        let params = self.encrypt_params(&json!({ "fid": uid, "imei": self.device().imei }))?;
        self.post_service(
            Service::Friend,
            "/api/friend/reject",
            format!("params={params}"),
            Some(key),
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phone_numbers_as_zalo_looks_them_up() {
        assert_eq!(phone_for_lookup("0912 345 678"), "84912345678");
        assert_eq!(phone_for_lookup("+84 912.345.678"), "84912345678");
        assert_eq!(phone_for_lookup("abc"), "");
    }

    #[test]
    fn relationships() {
        assert_eq!(
            friendship_of(&json!({ "is_friend": 1 })),
            Friendship::Friend
        );
        assert_eq!(
            friendship_of(&json!({ "is_friend": 0, "is_requesting": 1 })),
            Friendship::RequestReceived
        );
        assert_eq!(
            friendship_of(&json!({ "is_requested": true })),
            Friendship::RequestSent
        );
        assert_eq!(friendship_of(&json!({})), Friendship::None);
    }

    #[test]
    fn found_users_and_requests() {
        let user = found_user(&json!({
            "uid": "77", "zalo_name": "Lan", "display_name": "Lan Anh", "avatar": "https://a/x.jpg"
        }))
        .unwrap();
        assert_eq!(
            (
                user.uid.as_str(),
                user.display_name.as_str(),
                user.zalo_name.as_str()
            ),
            ("77", "Lan Anh", "Lan")
        );
        assert!(found_user(&json!({})).is_none());

        let data = json!({ "recommItems": [
            { "recommItemType": 1, "dataInfo": { "userId": "1", "recommType": 1, "displayName": "Gợi ý" } },
            { "recommItemType": 1, "dataInfo": {
                "userId": "2", "recommType": 2, "displayName": "Nam", "avatar": "https://a/2.jpg",
                "recommTime": 1700000000000_i64, "recommInfo": { "message": " Chào bạn " }
            } }
        ]});
        let requests = requests_of(&data);
        assert_eq!(requests.len(), 1);
        assert_eq!(
            (
                requests[0].uid.as_str(),
                requests[0].message.as_str(),
                requests[0].time
            ),
            ("2", "Chào bạn", 1_700_000_000_000)
        );
        assert!(requests_of(&json!({})).is_empty());
    }
}
