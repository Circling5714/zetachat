//! Data model shared by the store, the API and the connectors.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use crate::media::{Media, Quote};
pub use zeta_zalo::api::{Mention, TextStyle};

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
    /// Members of a group, 0 when unknown (and for 1-1 chats).
    pub members: i64,
    /// For a 1-1 chat, the name the account gave the other person ("tên gợi
    /// nhớ"); clients show it instead of `name` when set.
    pub alias: String,
}

/// How long what Zalo said about a user is trusted before asking again (ms).
pub const PEOPLE_FRESH_MS: i64 = 3 * 24 * 3600 * 1000;

/// A member of a group, for clients.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Member {
    pub uid: String,
    /// Empty when Zalo would not say.
    pub name: String,
    pub avatar: String,
    pub friend: bool,
    /// `owner` (who made the group), `admin` or `member`.
    pub role: &'static str,
}

/// Who is in a group: as many members as Zalo lists, owner and admins first.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GroupMembers {
    /// Size of the group, which can be more than the members listed.
    pub total: u64,
    pub members: Vec<Member>,
}

/// A Zalo user: a friend, a group member, this account.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Person {
    pub uid: String,
    /// The name Zalo shows for them; empty when unknown.
    pub name: String,
    pub zalo_name: String,
    pub avatar: String,
    /// In this account's friend list.
    pub friend: bool,
    /// When Zalo last told about them (ms); 0 for never.
    pub updated_at: i64,
    /// The name the account gave them ("tên gợi nhớ"), shown instead of
    /// theirs when set.
    pub alias: String,
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
    /// Photo, video, sticker, file… carried by the message.
    pub media: Option<Media>,
    /// The message this one replies to.
    pub quote: Option<Quote>,
    /// Milliseconds since the epoch.
    pub sent_at: i64,
    /// Reactions, most given first.
    pub reactions: Vec<ReactionCount>,
    /// For this account's messages: 0 sent, 1 delivered to the other side,
    /// 2 seen there.
    pub status: i64,
    /// People named in the text (`@Tên`), positions in UTF-16 units.
    pub mentions: Vec<Mention>,
    /// How runs of the text are drawn: `{ start, len, st }` in UTF-16 units,
    /// `st` one of Zalo's codes (`b`, `i`, `u`, `s`, `c_db342e`, `f_18`…).
    pub styles: Vec<TextStyle>,
    /// 0 normal, 1 important ("Quan trọng"), 2 urgent ("Khẩn cấp").
    pub urgency: u8,
}

/// Reactions to a message with one icon.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReactionCount {
    /// Zalo's code (`/-heart`…): send it back to react the same way.
    pub icon: String,
    /// How Zalo draws it; the code itself when unknown.
    pub emoji: String,
    /// Reactions given (a person can give several).
    pub count: u32,
    /// This account gave at least one of them.
    pub mine: bool,
}

/// Input for [`crate::store::Store::add_reaction`]: one reaction, which can
/// cover several messages.
#[derive(Clone, Debug)]
pub struct NewReaction {
    pub thread_id: String,
    /// Zalo's id of the reaction itself.
    pub action_id: String,
    /// The `clientId` its sender chose, so an echo is not counted twice.
    pub client_id: String,
    /// The messages: server id and client id of each.
    pub targets: Vec<(String, String)>,
    pub user_id: String,
    pub mine: bool,
    /// Empty: the user took back their reactions.
    pub icon: String,
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
    pub media: Option<Media>,
    pub quote: Option<Quote>,
    pub sent_at: i64,
    /// Original JSON from Zalo, kept so content can be parsed again later.
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
