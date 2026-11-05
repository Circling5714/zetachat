//! Polls ("bình chọn"), notes ("ghi chú") and reminders ("nhắc hẹn"): what a
//! group keeps on its board, and the reminders of a 1-1 chat. As zca-js:
//!
//! Polls, groups only, on the `group` service:
//! - Create: `POST /api/poll/create` with `{ group_id, question, options: [text],
//!   expired_time (ms, 0 for never), pinAct: false, allow_multi_choices,
//!   allow_add_new_option, is_hide_vote_preview, is_anonymous, poll_type: 0,
//!   src: 1, imei }` answers the poll.
//! - Read: `POST /api/poll/detail` with `{ poll_id, imei }`.
//! - Vote: `GET /api/poll/vote` with `{ poll_id, option_ids, imei }` (no option
//!   takes the vote back), answers `{ options }`.
//! - More options: `GET /api/poll/option/add` with `{ poll_id, new_options: JSON
//!   text of [{ voted, content }], voted_option_ids }`, answers `{ options }`.
//! - Close: `POST /api/poll/end` with `{ poll_id, imei }`.
//!
//! A poll: `{ poll_id, creator, question, options: [{ option_id, content, votes,
//! voted, voters }], joined, closed, allow_multi_choices, allow_add_new_option,
//! is_anonymous, is_hide_vote_preview, created_time, expired_time, num_vote }`.
//!
//! Notes and reminders, on the `group_board` service:
//! - Group note: `POST /api/board/topic/createv2` with `{ grid, type: 0, color:
//!   -16777216, emoji: "", startTime: -1, duration: -1, params: JSON text of
//!   { title }, repeat: 0, src: 1, imei, pinAct: 0 | 1 }`; changed with
//!   `/api/board/topic/updatev2` and `topicId` (`pinAct` 2 leaves the pin).
//! - Group reminder: the same call with `startTime` (ms), `emoji` "⏰", `color`
//!   -16245706 and `repeat` (0 once, 1 daily, 2 weekly, 3 monthly).
//! - 1-1 reminder: `POST /api/board/oneone/create` with `{ objectData: JSON text
//!   of { toUid, type: 0, color, emoji, startTime, duration: -1, params:
//!   { title }, needPin: false, repeat, creatorUid, src: 1 }, imei }`; changed
//!   with `/api/board/oneone/update` and `reminderId`.
//! - The board: `GET /api/board/list` with `{ group_id, board_type: 0, page,
//!   count, last_id: 0, last_type: 0, imei }` answers `{ items: [{ boardType (1
//!   note, 2 pinned message, 3 poll), data }], count }`.
//! - Reminders: `GET /api/board/listReminder` with `{ objectData: JSON text of
//!   { group_id, board_type: 1, page, count, last_id: 0, last_type: 0 }, imei }`,
//!   or `GET /api/board/oneone/list` with `{ objectData: { uid, … } }` for a 1-1
//!   chat; both answer `data` as JSON text. Seen live: a chat without
//!   reminders answers error -1 instead of an empty list.
//! - Delete: `POST /api/board/topic/remove` with `{ grid, topicId, imei }`, or
//!   `/api/board/oneone/remove` with `{ uid, reminderId }`.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::api::{first_str, unwrap_json_string, Recipient};
use crate::client::ZaloClient;
use crate::domains::Service;
use crate::error::{Error, Result};

/// Colour Zalo's clients give reminders.
const REMINDER_COLOR: i64 = -16_245_706;
/// Colour of notes.
const NOTE_COLOR: i64 = -16_777_216;

/// One choice of a poll.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollOption {
    pub id: u64,
    pub text: String,
    pub votes: u32,
    /// This account chose it.
    pub voted: bool,
    /// Who chose it (empty for anonymous polls).
    pub voters: Vec<String>,
}

/// A group poll.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Poll {
    pub id: u64,
    pub creator: String,
    pub question: String,
    pub options: Vec<PollOption>,
    /// Several options can be chosen.
    pub multiple: bool,
    /// Members can add options.
    pub add_options: bool,
    /// Voters are not shown.
    pub anonymous: bool,
    /// Results show only after voting.
    pub hide_results: bool,
    pub closed: bool,
    /// Milliseconds since the epoch.
    pub created_at: i64,
    /// When voting ends, 0 for never.
    pub expires_at: i64,
    /// People who voted.
    pub voters: u32,
}

/// What a new poll asks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NewPoll {
    pub question: String,
    pub options: Vec<String>,
    pub multiple: bool,
    pub add_options: bool,
    pub anonymous: bool,
    pub hide_results: bool,
    /// Milliseconds since the epoch, 0 for never.
    pub expires_at: i64,
}

/// A note or a reminder.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Topic {
    pub id: String,
    /// The note's text, or what the reminder is about.
    pub title: String,
    pub emoji: String,
    pub creator: String,
    /// Milliseconds since the epoch.
    pub created_at: i64,
    pub edited_at: i64,
    /// When a reminder is due; 0 for notes.
    pub start_at: i64,
    /// 0 once, 1 daily, 2 weekly, 3 monthly.
    pub repeat: u8,
    /// Group reminders: members who said they will come, or will not.
    pub accepted: u32,
    pub rejected: u32,
}

/// What a new reminder says.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NewReminder {
    pub title: String,
    /// Shown before the title ("⏰" when empty).
    pub emoji: String,
    /// Milliseconds since the epoch.
    pub start_at: i64,
    /// 0 once, 1 daily, 2 weekly, 3 monthly.
    pub repeat: u8,
}

/// One thing on a group's board.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum BoardItem {
    Note(Topic),
    /// A pinned message ("tin ghim"); its title is the message's text.
    Pinned(Topic),
    Poll(Poll),
}

fn int(v: &Value, keys: &[&str]) -> i64 {
    keys.iter()
        .find_map(|k| match v.get(*k) {
            Some(Value::Number(n)) => n.as_i64(),
            Some(Value::String(s)) => s.trim().parse().ok(),
            _ => None,
        })
        .unwrap_or(0)
}

fn flag(v: &Value, key: &str) -> bool {
    match v.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0) != 0,
        _ => false,
    }
}

fn ids(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|x| match x {
                    Value::String(s) if !s.is_empty() => Some(s.clone()),
                    Value::Number(n) => Some(n.to_string()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn options_of(v: &Value) -> Vec<PollOption> {
    v.get("options")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|o| {
                    let id = u64::try_from(int(o, &["option_id", "id"])).ok()?;
                    Some(PollOption {
                        id,
                        text: first_str(o, &["content"]),
                        votes: u32::try_from(int(o, &["votes"])).unwrap_or(0),
                        voted: flag(o, "voted"),
                        voters: ids(o.get("voters")),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A poll as Zalo describes it.
pub fn poll_of(v: &Value) -> Option<Poll> {
    let id = u64::try_from(int(v, &["poll_id", "pollId", "id"]))
        .ok()
        .filter(|id| *id > 0)?;
    Some(Poll {
        id,
        creator: first_str(v, &["creator", "creatorId"]),
        question: first_str(v, &["question"]),
        options: options_of(v),
        multiple: flag(v, "allow_multi_choices"),
        add_options: flag(v, "allow_add_new_option"),
        anonymous: flag(v, "is_anonymous"),
        hide_results: flag(v, "is_hide_vote_preview"),
        closed: flag(v, "closed"),
        created_at: int(v, &["created_time"]),
        expires_at: int(v, &["expired_time"]).max(0),
        voters: u32::try_from(int(v, &["num_vote"])).unwrap_or(0),
    })
}

/// `params` of a topic: an object, or JSON text of one.
fn topic_params(v: &Value) -> Value {
    match v.get("params") {
        Some(Value::String(s)) => serde_json::from_str(s).unwrap_or(Value::Null),
        Some(p @ Value::Object(_)) => p.clone(),
        _ => Value::Null,
    }
}

/// A note or reminder as Zalo describes it (group topics, 1-1 reminders).
pub fn topic_of(v: &Value) -> Option<Topic> {
    let id = first_str(v, &["id", "topicId", "reminderId"]);
    if id.is_empty() {
        return None;
    }
    let params = topic_params(v);
    let title = first_str(&params, &["title"]);
    let start = int(v, &["startTime"]);
    let responses = v.get("responseMem").cloned().unwrap_or(Value::Null);
    Some(Topic {
        id,
        title,
        emoji: first_str(v, &["emoji"]),
        creator: first_str(v, &["creatorId", "creatorUid"]),
        created_at: int(v, &["createTime"]),
        edited_at: int(v, &["editTime"]),
        start_at: start.max(0),
        repeat: u8::try_from(int(v, &["repeat"])).unwrap_or(0),
        accepted: u32::try_from(int(&responses, &["acceptMember"])).unwrap_or(0),
        rejected: u32::try_from(int(&responses, &["rejectMember"])).unwrap_or(0),
    })
}

/// The items of a `board/list` answer.
fn board_of(data: &Value) -> Vec<BoardItem> {
    let Some(items) = data.get("items").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let data = item.get("data")?;
            match int(item, &["boardType"]) {
                1 => topic_of(data).map(BoardItem::Note),
                2 => topic_of(data).map(BoardItem::Pinned),
                3 => poll_of(data).map(BoardItem::Poll),
                _ => None,
            }
        })
        .collect()
}

/// The reminders of a `listReminder` / `oneone/list` answer (JSON text, or a
/// list, or `{ items }`).
fn reminders_of(data: Value) -> Vec<Topic> {
    let data = unwrap_json_string(data);
    let list = match &data {
        Value::Array(list) => list.as_slice(),
        other => other
            .get("items")
            .or_else(|| other.get("data"))
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default(),
    };
    list.iter().filter_map(topic_of).collect()
}

/// A reminder's title must say something, and its time must be known.
fn check_reminder(r: &NewReminder) -> Result<()> {
    if r.title.trim().is_empty() {
        return Err(Error::Protocol("nhắc hẹn cần có tiêu đề".into()));
    }
    if r.start_at <= 0 || r.repeat > 3 {
        return Err(Error::Protocol(
            "thời gian hoặc kiểu lặp không hợp lệ".into(),
        ));
    }
    Ok(())
}

impl ZaloClient {
    async fn post_encrypted(&self, service: Service, path: &str, params: &Value) -> Result<Value> {
        let key = self.session_key()?;
        let form = format!("params={}", self.encrypt_params(params)?);
        self.post_service(service, path, form, Some(key)).await
    }

    fn self_uid(&self) -> String {
        self.session().map(|s| s.uid.clone()).unwrap_or_default()
    }

    /// Ask a group a question.
    pub async fn create_poll(&self, group_id: &str, poll: &NewPoll) -> Result<Poll> {
        let options: Vec<&str> = poll
            .options
            .iter()
            .map(|o| o.trim())
            .filter(|o| !o.is_empty())
            .collect();
        if poll.question.trim().is_empty() || options.len() < 2 {
            return Err(Error::Protocol(
                "bình chọn cần câu hỏi và ít nhất hai lựa chọn".into(),
            ));
        }
        let params = json!({
            "group_id": group_id,
            "question": poll.question.trim(),
            "options": options,
            "expired_time": poll.expires_at.max(0),
            "pinAct": false,
            "allow_multi_choices": poll.multiple,
            "allow_add_new_option": poll.add_options,
            "is_hide_vote_preview": poll.hide_results,
            "is_anonymous": poll.anonymous,
            "poll_type": 0,
            "src": 1,
            "imei": self.device().imei,
        });
        let data = self
            .post_encrypted(Service::Group, "/api/poll/create", &params)
            .await?;
        poll_of(&data).ok_or_else(|| Error::Protocol("Zalo không trả về bình chọn vừa tạo".into()))
    }

    /// A poll as it stands now.
    pub async fn poll(&self, poll_id: u64) -> Result<Poll> {
        let params = json!({ "poll_id": poll_id, "imei": self.device().imei });
        let data = self
            .post_encrypted(Service::Group, "/api/poll/detail", &params)
            .await?;
        poll_of(&data).ok_or_else(|| Error::Protocol("Zalo không trả về bình chọn".into()))
    }

    /// Vote for these options (none: take the vote back); answers the options
    /// with their counts.
    pub async fn vote_poll(&self, poll_id: u64, option_ids: &[u64]) -> Result<Vec<PollOption>> {
        let params = json!({
            "poll_id": poll_id,
            "option_ids": option_ids,
            "imei": self.device().imei,
        });
        let data = self
            .get_encrypted(Service::Group, "/api/poll/vote", &params)
            .await?;
        Ok(options_of(&data))
    }

    /// Add options to a poll that allows it; `voted` are the options this
    /// account keeps its vote on.
    pub async fn add_poll_options(
        &self,
        poll_id: u64,
        options: &[String],
        voted: &[u64],
    ) -> Result<Vec<PollOption>> {
        let new: Vec<Value> = options
            .iter()
            .map(|o| o.trim())
            .filter(|o| !o.is_empty())
            .map(|o| json!({ "voted": false, "content": o }))
            .collect();
        if new.is_empty() {
            return Err(Error::Protocol("chưa có lựa chọn nào để thêm".into()));
        }
        let params = json!({
            "poll_id": poll_id,
            "new_options": Value::Array(new).to_string(),
            "voted_option_ids": voted,
        });
        let data = self
            .get_encrypted(Service::Group, "/api/poll/option/add", &params)
            .await?;
        Ok(options_of(&data))
    }

    /// End a poll: no more votes.
    pub async fn close_poll(&self, poll_id: u64) -> Result<()> {
        let params = json!({ "poll_id": poll_id, "imei": self.device().imei });
        match self
            .post_encrypted(Service::Group, "/api/poll/end", &params)
            .await
        {
            Ok(_) | Err(Error::EmptyResponse { .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// What a group's board holds: notes, pinned messages, polls (newest
    /// first, as Zalo lists them; at most 50).
    pub async fn board(&self, group_id: &str) -> Result<Vec<BoardItem>> {
        let params = json!({
            "group_id": group_id,
            "board_type": 0,
            "page": 1,
            "count": 50,
            "last_id": 0,
            "last_type": 0,
            "imei": self.device().imei,
        });
        let data = self
            .get_encrypted(Service::GroupBoard, "/api/board/list", &params)
            .await?;
        Ok(board_of(&unwrap_json_string(data)))
    }

    /// Write a note on a group's board; `pin` also pins it on top of the chat.
    pub async fn create_note(&self, group_id: &str, text: &str, pin: bool) -> Result<Topic> {
        self.write_note(group_id, None, text, pin).await
    }

    /// Change a note.
    pub async fn update_note(
        &self,
        group_id: &str,
        topic_id: &str,
        text: &str,
        pin: bool,
    ) -> Result<Topic> {
        self.write_note(group_id, Some(topic_id), text, pin).await
    }

    async fn write_note(
        &self,
        group_id: &str,
        topic_id: Option<&str>,
        text: &str,
        pin: bool,
    ) -> Result<Topic> {
        if text.trim().is_empty() {
            return Err(Error::Protocol("ghi chú đang trống".into()));
        }
        let mut params = json!({
            "grid": group_id,
            "type": 0,
            "color": NOTE_COLOR,
            "emoji": "",
            "startTime": -1,
            "duration": -1,
            "params": json!({ "title": text.trim() }).to_string(),
            "repeat": 0,
            "imei": self.device().imei,
        });
        let path = match topic_id {
            None => {
                params["src"] = json!(1);
                params["pinAct"] = json!(u8::from(pin));
                "/api/board/topic/createv2"
            }
            Some(id) => {
                params["topicId"] = json!(id);
                params["pinAct"] = json!(if pin { 1 } else { 2 });
                "/api/board/topic/updatev2"
            }
        };
        let data = self
            .post_encrypted(Service::GroupBoard, path, &params)
            .await?;
        topic_of(&unwrap_json_string(data))
            .ok_or_else(|| Error::Protocol("Zalo không trả về ghi chú vừa lưu".into()))
    }

    /// Delete a note or a group reminder.
    pub async fn remove_topic(&self, group_id: &str, topic_id: &str) -> Result<()> {
        let params = json!({ "grid": group_id, "topicId": topic_id, "imei": self.device().imei });
        match self
            .post_encrypted(Service::GroupBoard, "/api/board/topic/remove", &params)
            .await
        {
            Ok(_) | Err(Error::EmptyResponse { .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// The reminders of a chat (at most 50).
    pub async fn reminders(&self, chat: Recipient<'_>) -> Result<Vec<Topic>> {
        let (path, params) = match chat {
            Recipient::User(uid) => (
                "/api/board/oneone/list",
                json!({ "objectData": json!({
                    "uid": uid, "board_type": 1, "page": 1, "count": 50,
                    "last_id": 0, "last_type": 0,
                }).to_string() }),
            ),
            Recipient::Group(grid) => (
                "/api/board/listReminder",
                json!({
                    "objectData": json!({
                        "group_id": grid, "board_type": 1, "page": 1, "count": 50,
                        "last_id": 0, "last_type": 0,
                    }).to_string(),
                    "imei": self.device().imei,
                }),
            ),
        };
        match self
            .get_encrypted(Service::GroupBoard, path, &params)
            .await
        {
            Ok(data) => Ok(reminders_of(data)),
            // How Zalo says "none" (seen live on chats without reminders).
            Err(Error::Server { code: -1, .. }) => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// Set a reminder in a chat; everyone in it is reminded when it is due.
    pub async fn create_reminder(&self, chat: Recipient<'_>, r: &NewReminder) -> Result<Topic> {
        self.write_reminder(chat, None, r).await
    }

    /// Change a reminder.
    pub async fn update_reminder(
        &self,
        chat: Recipient<'_>,
        id: &str,
        r: &NewReminder,
    ) -> Result<Topic> {
        self.write_reminder(chat, Some(id), r).await
    }

    async fn write_reminder(
        &self,
        chat: Recipient<'_>,
        id: Option<&str>,
        r: &NewReminder,
    ) -> Result<Topic> {
        check_reminder(r)?;
        let emoji = if r.emoji.trim().is_empty() {
            "⏰"
        } else {
            r.emoji.trim()
        };
        let (path, params) = match chat {
            Recipient::User(uid) => {
                let mut object = json!({
                    "toUid": uid,
                    "type": 0,
                    "color": REMINDER_COLOR,
                    "emoji": emoji,
                    "startTime": r.start_at,
                    "duration": -1,
                    "params": { "title": r.title.trim() },
                    "needPin": false,
                    "repeat": r.repeat,
                });
                let path = match id {
                    None => {
                        object["creatorUid"] = json!(self.self_uid());
                        object["src"] = json!(1);
                        "/api/board/oneone/create"
                    }
                    Some(id) => {
                        object["reminderId"] = json!(id);
                        "/api/board/oneone/update"
                    }
                };
                let mut params = json!({ "objectData": object.to_string() });
                if id.is_none() {
                    params["imei"] = json!(self.device().imei);
                }
                (path, params)
            }
            Recipient::Group(grid) => {
                let mut params = json!({
                    "grid": grid,
                    "type": 0,
                    "color": REMINDER_COLOR,
                    "emoji": emoji,
                    "startTime": r.start_at,
                    "duration": -1,
                    "params": json!({ "title": r.title.trim() }).to_string(),
                    "repeat": r.repeat,
                    "imei": self.device().imei,
                });
                let path = match id {
                    None => {
                        params["src"] = json!(1);
                        params["pinAct"] = json!(0);
                        "/api/board/topic/createv2"
                    }
                    Some(id) => {
                        params["topicId"] = json!(id);
                        params["pinAct"] = json!(2);
                        "/api/board/topic/updatev2"
                    }
                };
                (path, params)
            }
        };
        let data = self
            .post_encrypted(Service::GroupBoard, path, &params)
            .await?;
        topic_of(&unwrap_json_string(data))
            .ok_or_else(|| Error::Protocol("Zalo không trả về nhắc hẹn vừa lưu".into()))
    }

    /// Delete a reminder.
    pub async fn remove_reminder(&self, chat: Recipient<'_>, id: &str) -> Result<()> {
        match chat {
            Recipient::Group(grid) => self.remove_topic(grid, id).await,
            Recipient::User(uid) => {
                let params = json!({ "uid": uid, "reminderId": id });
                match self
                    .post_encrypted(Service::GroupBoard, "/api/board/oneone/remove", &params)
                    .await
                {
                    Ok(_) | Err(Error::EmptyResponse { .. }) => Ok(()),
                    Err(e) => Err(e),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn poll_json() -> Value {
        json!({
            "creator": "7", "question": "Đi đâu?", "joined": true, "closed": false,
            "poll_id": 501, "allow_multi_choices": true, "allow_add_new_option": false,
            "is_anonymous": false, "poll_type": 0, "created_time": 1000, "updated_time": 1000,
            "expired_time": 0, "is_hide_vote_preview": false, "num_vote": 3,
            "options": [
                { "content": "Biển", "votes": 2, "voted": true, "voters": ["7", 8], "option_id": 1 },
                { "content": "Núi", "votes": 1, "voted": false, "voters": ["9"], "option_id": 2 }
            ]
        })
    }

    #[test]
    fn polls() {
        let p = poll_of(&poll_json()).unwrap();
        assert_eq!((p.id, p.question.as_str(), p.voters), (501, "Đi đâu?", 3));
        assert!(p.multiple && !p.closed);
        assert_eq!(p.options.len(), 2);
        assert_eq!(p.options[0].voters, ["7", "8"]);
        assert!(p.options[0].voted);
        assert!(poll_of(&json!({ "question": "?" })).is_none());
    }

    #[test]
    fn topics_read_params_as_text_or_object() {
        let note = json!({
            "id": "t1", "type": 0, "color": -16777216, "emoji": "", "startTime": -1,
            "duration": -1, "params": "{\"title\":\"Họp lúc 9h\"}", "creatorId": "7",
            "editorId": "7", "createTime": 5, "editTime": 6, "repeat": 0
        });
        let t = topic_of(&note).unwrap();
        assert_eq!(
            (t.id.as_str(), t.title.as_str(), t.start_at),
            ("t1", "Họp lúc 9h", 0)
        );
        let reminder = json!({
            "reminderId": "r9", "creatorUid": "7", "toUid": "8", "emoji": "⏰",
            "startTime": 1_800_000_000_000_i64, "repeat": 2,
            "params": { "title": "Gọi mẹ", "setTitle": true }
        });
        let r = topic_of(&reminder).unwrap();
        assert_eq!(
            (r.id.as_str(), r.creator.as_str(), r.repeat),
            ("r9", "7", 2)
        );
        assert_eq!(r.title, "Gọi mẹ");
    }

    #[test]
    fn board_items_and_reminder_lists() {
        let board = board_of(&json!({ "count": 3, "items": [
            { "boardType": 1, "data": { "id": "n1", "params": "{\"title\":\"Ghi chú\"}" } },
            { "boardType": 3, "data": poll_json() },
            { "boardType": 2, "data": { "id": "p1", "params": {} } },
            { "boardType": 9, "data": {} }
        ] }));
        assert_eq!(board.len(), 3);
        assert!(matches!(&board[0], BoardItem::Note(t) if t.title == "Ghi chú"));
        assert!(matches!(&board[1], BoardItem::Poll(p) if p.id == 501));
        let tagged = serde_json::to_value(&board[0]).unwrap();
        assert_eq!(tagged["type"], "note");

        let text = json!([{ "id": "r1", "startTime": 9, "params": { "title": "A" },
            "responseMem": { "acceptMember": 2, "rejectMember": 1, "myResp": 0 } }])
        .to_string();
        let list = reminders_of(Value::String(text));
        assert_eq!(list.len(), 1);
        assert_eq!((list[0].accepted, list[0].rejected), (2, 1));
        assert!(reminders_of(json!({})).is_empty());
    }

    #[test]
    fn reminders_need_a_title_and_a_time() {
        let ok = NewReminder {
            title: "Họp".into(),
            start_at: 1,
            ..NewReminder::default()
        };
        assert!(check_reminder(&ok).is_ok());
        assert!(check_reminder(&NewReminder {
            title: " ".into(),
            ..ok.clone()
        })
        .is_err());
        assert!(check_reminder(&NewReminder {
            start_at: 0,
            ..ok.clone()
        })
        .is_err());
        assert!(check_reminder(&NewReminder { repeat: 4, ..ok }).is_err());
    }
}
