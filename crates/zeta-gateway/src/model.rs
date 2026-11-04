//! Data model shared by the store, the API and the connectors.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One conversation: a 1-1 chat or a group.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThreadKind {
    User,
    Group,
}

impl ThreadKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Group => "group",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "user" => Some(Self::User),
            "group" => Some(Self::Group),
            _ => None,
        }
    }

    fn code(self) -> &'static str {
        match self {
            Self::User => "u",
            Self::Group => "g",
        }
    }
}

/// Gateway-wide thread id: `zalo:u:<user id>` or `zalo:g:<group id>`. The network
/// prefix leaves room for other connectors (Telegram) later.
pub fn thread_id(kind: ThreadKind, remote_id: &str) -> String {
    format!("zalo:{}:{remote_id}", kind.code())
}

/// Inverse of [`thread_id`].
pub fn split_thread_id(id: &str) -> Option<(ThreadKind, &str)> {
    let rest = id.strip_prefix("zalo:")?;
    let (code, remote) = rest.split_once(':')?;
    let kind = match code {
        "u" => ThreadKind::User,
        "g" => ThreadKind::Group,
        _ => return None,
    };
    (!remote.is_empty()).then_some((kind, remote))
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Thread {
    pub id: String,
    pub kind: ThreadKind,
    pub remote_id: String,
    pub name: String,
    pub avatar: String,
    /// Milliseconds since the epoch, 0 when the thread has no message yet.
    pub last_message_at: i64,
    pub unread: i64,
    /// Text of the newest message, empty without messages.
    pub preview: String,
}

/// Create-or-update input for a thread.
#[derive(Clone, Debug)]
pub struct ThreadUpsert {
    pub kind: ThreadKind,
    pub remote_id: String,
    pub name: String,
    pub avatar: String,
}

impl ThreadUpsert {
    pub fn id(&self) -> String {
        thread_id(self.kind, &self.remote_id)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Message {
    /// Gateway sequence number, increasing with insertion order.
    pub seq: i64,
    pub thread_id: String,
    /// Zalo message id, or `local:<client id>` until the server id is known.
    pub remote_id: String,
    pub client_id: String,
    pub sender_id: String,
    pub sender_name: String,
    pub outgoing: bool,
    /// Zalo `msgType` (`webchat`, `chat.photo`, …).
    pub kind: String,
    /// Text, or a short summary for media (`[Hình ảnh]`, …).
    pub text: String,
    /// Milliseconds since the epoch.
    pub sent_at: i64,
}

/// Input for [`crate::store::Store::insert_message`].
#[derive(Clone, Debug)]
pub struct NewMessage {
    pub thread_id: String,
    pub remote_id: String,
    pub client_id: String,
    pub sender_id: String,
    pub sender_name: String,
    pub outgoing: bool,
    pub kind: String,
    pub text: String,
    pub sent_at: i64,
    /// Original JSON from Zalo, kept for richer rendering later.
    pub raw: String,
}

/// A client device allowed to use the API.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub created_at: i64,
    pub last_seen_at: i64,
    pub revoked: bool,
}

/// One entry of the event log that clients replay to stay in sync.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StoredEvent {
    pub seq: i64,
    pub kind: String,
    pub payload: Value,
    pub created_at: i64,
}

/// Milliseconds since the epoch.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_ids_roundtrip() {
        let id = thread_id(ThreadKind::Group, "123");
        assert_eq!(id, "zalo:g:123");
        assert_eq!(split_thread_id(&id), Some((ThreadKind::Group, "123")));
        assert_eq!(split_thread_id("zalo:u:9"), Some((ThreadKind::User, "9")));
        assert_eq!(split_thread_id("zalo:x:9"), None);
        assert_eq!(split_thread_id("zalo:u:"), None);
        assert_eq!(split_thread_id("telegram:u:9"), None);
    }
}
