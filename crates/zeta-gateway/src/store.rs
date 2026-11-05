//! SQLite store: threads, messages, the event log and client devices.
//!
//! One connection behind a mutex is plenty for a single-user gateway: every
//! operation is a short statement, and none is held across an `.await`.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension, Row};
use serde_json::Value;

use zeta_zalo::reaction::reaction_emoji;

use crate::auth;
use crate::media;
use crate::model::{
    now_ms, Device, Message, NewMessage, NewReaction, Person, ReactionCount, StoredEvent, Thread,
    ThreadKind, ThreadUpsert,
};

pub type Result<T> = rusqlite::Result<T>;

const SCHEMA_V1: &str = "
CREATE TABLE threads (
    id              TEXT PRIMARY KEY,
    network         TEXT NOT NULL DEFAULT 'zalo',
    kind            TEXT NOT NULL,
    remote_id       TEXT NOT NULL,
    name            TEXT NOT NULL DEFAULT '',
    avatar          TEXT NOT NULL DEFAULT '',
    last_message_at INTEGER NOT NULL DEFAULT 0,
    unread          INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX threads_recent ON threads (last_message_at DESC);

CREATE TABLE messages (
    seq         INTEGER PRIMARY KEY AUTOINCREMENT,
    thread_id   TEXT NOT NULL REFERENCES threads (id),
    remote_id   TEXT NOT NULL,
    client_id   TEXT NOT NULL DEFAULT '',
    sender_id   TEXT NOT NULL DEFAULT '',
    sender_name TEXT NOT NULL DEFAULT '',
    outgoing    INTEGER NOT NULL,
    kind        TEXT NOT NULL,
    text        TEXT NOT NULL DEFAULT '',
    sent_at     INTEGER NOT NULL,
    raw         TEXT NOT NULL DEFAULT '{}',
    UNIQUE (thread_id, remote_id)
);
CREATE INDEX messages_thread ON messages (thread_id, seq);
CREATE INDEX messages_client ON messages (thread_id, client_id);

CREATE TABLE events (
    seq        INTEGER PRIMARY KEY AUTOINCREMENT,
    kind       TEXT NOT NULL,
    payload    TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE devices (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL,
    token_hash   TEXT NOT NULL UNIQUE,
    created_at   INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL DEFAULT 0,
    revoked      INTEGER NOT NULL DEFAULT 0
);
";

/// Version 2: structured media (JSON) next to the text summary.
const SCHEMA_V2: &str = "ALTER TABLE messages ADD COLUMN media TEXT NOT NULL DEFAULT '';";

/// Version 3: the message a reply quotes (JSON).
const SCHEMA_V3: &str = "ALTER TABLE messages ADD COLUMN quote TEXT NOT NULL DEFAULT '';";

/// Version 4: reactions, one row per reaction and message reacted to. A row
/// with an empty icon takes back the user's earlier reactions to that message.
const SCHEMA_V4: &str = "
CREATE TABLE reactions (
    thread_id  TEXT NOT NULL,
    action_id  TEXT NOT NULL,
    message_id TEXT NOT NULL,
    client_id  TEXT NOT NULL DEFAULT '',
    user_id    TEXT NOT NULL,
    mine       INTEGER NOT NULL DEFAULT 0,
    icon       TEXT NOT NULL,
    sent_at    INTEGER NOT NULL,
    PRIMARY KEY (thread_id, action_id, message_id)
);
CREATE INDEX reactions_message ON reactions (thread_id, message_id);
CREATE UNIQUE INDEX reactions_client ON reactions (thread_id, user_id, client_id, message_id)
    WHERE client_id <> '';
";

/// Version 5: people (friends, group members, this account) with their names
/// and avatars, and the size of groups.
const SCHEMA_V5: &str = "
CREATE TABLE people (
    uid        TEXT PRIMARY KEY,
    name       TEXT NOT NULL DEFAULT '',
    zalo_name  TEXT NOT NULL DEFAULT '',
    avatar     TEXT NOT NULL DEFAULT '',
    friend     INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL DEFAULT 0
);
ALTER TABLE threads ADD COLUMN members INTEGER NOT NULL DEFAULT 0;
";

const THREAD_COLUMNS: &str = "id, kind, remote_id, name, avatar, last_message_at, unread, \
    COALESCE((SELECT m.text FROM messages m WHERE m.thread_id = threads.id \
              ORDER BY m.seq DESC LIMIT 1), ''), members";
const PERSON_COLUMNS: &str = "uid, name, zalo_name, avatar, friend, updated_at";

/// Most messages a search looks through, newest first.
const SEARCH_SCAN: i64 = 200_000;
const MESSAGE_COLUMNS: &str = "seq, thread_id, remote_id, client_id, sender_id, sender_name, \
    outgoing, kind, text, sent_at, media, quote, status";

/// Version 6: how far the account's messages got (delivered, seen), and an
/// index to find a message by Zalo's id alone, as receipts give it.
const SCHEMA_V6: &str = "
ALTER TABLE messages ADD COLUMN status INTEGER NOT NULL DEFAULT 0;
CREATE INDEX messages_remote ON messages (remote_id);
";

fn thread_from_row(row: &Row<'_>) -> Result<Thread> {
    let kind: String = row.get(1)?;
    Ok(Thread {
        id: row.get(0)?,
        kind: ThreadKind::parse(&kind).unwrap_or(ThreadKind::User),
        remote_id: row.get(2)?,
        name: row.get(3)?,
        avatar: row.get(4)?,
        last_message_at: row.get(5)?,
        unread: row.get(6)?,
        preview: row.get(7)?,
        members: row.get(8)?,
    })
}

fn person_from_row(row: &Row<'_>) -> Result<Person> {
    Ok(Person {
        uid: row.get(0)?,
        name: row.get(1)?,
        zalo_name: row.get(2)?,
        avatar: row.get(3)?,
        friend: row.get(4)?,
        updated_at: row.get(5)?,
    })
}

/// Lower case without Vietnamese marks, so "dang" finds "Đặng" (the web
/// client folds the same way).
pub fn fold(s: &str) -> String {
    s.chars()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            'à' | 'á' | 'ạ' | 'ả' | 'ã' | 'â' | 'ầ' | 'ấ' | 'ậ' | 'ẩ' | 'ẫ' | 'ă' | 'ằ' | 'ắ'
            | 'ặ' | 'ẳ' | 'ẵ' => 'a',
            'è' | 'é' | 'ẹ' | 'ẻ' | 'ẽ' | 'ê' | 'ề' | 'ế' | 'ệ' | 'ể' | 'ễ' => {
                'e'
            }
            'ì' | 'í' | 'ị' | 'ỉ' | 'ĩ' => 'i',
            'ò' | 'ó' | 'ọ' | 'ỏ' | 'õ' | 'ô' | 'ồ' | 'ố' | 'ộ' | 'ổ' | 'ỗ' | 'ơ' | 'ờ' | 'ớ'
            | 'ợ' | 'ở' | 'ỡ' => 'o',
            'ù' | 'ú' | 'ụ' | 'ủ' | 'ũ' | 'ư' | 'ừ' | 'ứ' | 'ự' | 'ử' | 'ữ' => {
                'u'
            }
            'ỳ' | 'ý' | 'ỵ' | 'ỷ' | 'ỹ' => 'y',
            'đ' => 'd',
            other => other,
        })
        .collect()
}

fn message_from_row(row: &Row<'_>) -> Result<Message> {
    Ok(Message {
        seq: row.get(0)?,
        thread_id: row.get(1)?,
        remote_id: row.get(2)?,
        client_id: row.get(3)?,
        sender_id: row.get(4)?,
        sender_name: row.get(5)?,
        outgoing: row.get(6)?,
        kind: row.get(7)?,
        text: row.get(8)?,
        sent_at: row.get(9)?,
        media: from_json(&row.get::<_, String>(10)?),
        quote: from_json(&row.get::<_, String>(11)?),
        reactions: Vec::new(),
        status: row.get(12)?,
    })
}

/// Count reactions given in order (`(user, mine, icon)`, oldest first): every
/// row adds one, an empty icon clears what that user gave before. Most given
/// icon first, then the earliest.
fn count_reactions(rows: &[(String, bool, String)]) -> Vec<ReactionCount> {
    let mut by_user: Vec<(&str, Vec<(bool, &str)>)> = Vec::new();
    for (user, mine, icon) in rows {
        let given = match by_user.iter_mut().find(|(u, _)| u == user) {
            Some((_, given)) => given,
            None => {
                by_user.push((user, Vec::new()));
                &mut by_user.last_mut().expect("just pushed").1
            }
        };
        if icon.is_empty() {
            given.clear();
        } else {
            given.push((*mine, icon));
        }
    }
    let mut counts: Vec<ReactionCount> = Vec::new();
    // Earliest first, over everyone.
    let mut order: Vec<&str> = Vec::new();
    for (_, _, icon) in rows {
        if !icon.is_empty() && !order.contains(&icon.as_str()) {
            order.push(icon);
        }
    }
    for icon in order {
        let given = by_user
            .iter()
            .flat_map(|(_, given)| given.iter())
            .filter(|(_, i)| *i == icon);
        let (count, mine) = given.fold((0u32, false), |(n, m), (mine, _)| (n + 1, m || *mine));
        if count > 0 {
            counts.push(ReactionCount {
                icon: icon.to_string(),
                emoji: reaction_emoji(icon).unwrap_or(icon).to_string(),
                count,
                mine,
            });
        }
    }
    // Stable: equal counts keep the earliest first.
    counts.sort_by_key(|c| std::cmp::Reverse(c.count));
    counts
}

/// Fill in the reactions of messages of one thread.
fn attach_reactions(conn: &Connection, thread_id: &str, messages: &mut [Message]) -> Result<()> {
    if messages.is_empty() {
        return Ok(());
    }
    let mut stmt = conn.prepare(
        "SELECT user_id, mine, icon FROM reactions WHERE thread_id = ?1 AND message_id = ?2
         ORDER BY sent_at, rowid",
    )?;
    for message in messages.iter_mut() {
        let rows = stmt.query_map(params![thread_id, message.remote_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        let rows: Vec<(String, bool, String)> = rows.collect::<Result<_>>()?;
        message.reactions = count_reactions(&rows);
    }
    Ok(())
}

/// A JSON column; empty means none.
fn from_json<T: serde::de::DeserializeOwned>(stored: &str) -> Option<T> {
    if stored.is_empty() {
        None
    } else {
        serde_json::from_str(stored).ok()
    }
}

fn to_json<T: serde::Serialize>(value: Option<&T>) -> String {
    value
        .and_then(|v| serde_json::to_string(v).ok())
        .unwrap_or_default()
}

/// Recompute text, media and quote of stored messages from their original Zalo
/// JSON, once the parsing has learned new kinds of content.
fn reparse_content(conn: &Connection) -> Result<usize> {
    let rows: Vec<(i64, String)> = {
        let mut stmt = conn.prepare("SELECT seq, raw FROM messages WHERE raw <> '{}'")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<Result<_>>()?
    };
    let mut update =
        conn.prepare("UPDATE messages SET text = ?2, media = ?3, quote = ?4 WHERE seq = ?1")?;
    let mut updated = 0;
    for (seq, raw) in rows {
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        let msg_type = value
            .get("msgType")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let content = value.get("content");
        let text = media::summarize(msg_type, content);
        let found = media::extract(msg_type, content);
        let quote = media::extract_quote(&value);
        updated += update.execute(params![
            seq,
            text,
            to_json(found.as_ref()),
            to_json(quote.as_ref())
        ])?;
    }
    Ok(updated)
}

/// Turn a message into a recalled one: its content is dropped for good.
/// Returns its `seq`, `None` when it is not stored or already recalled.
/// (Also used while upgrading old databases: it reads no column added later.)
fn recall_in(
    conn: &Connection,
    thread_id: &str,
    remote_id: &str,
    client_id: &str,
) -> Result<Option<i64>> {
    let found: Option<(i64, String)> = conn
        .query_row(
            "SELECT seq, remote_id FROM messages WHERE thread_id = ?1 AND kind <> 'chat.undo'
               AND ((?2 <> '' AND remote_id = ?2) OR (?3 <> '' AND client_id = ?3))
             ORDER BY seq LIMIT 1",
            params![thread_id, remote_id, client_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((seq, stored_id)) = found else {
        return Ok(None);
    };
    conn.execute(
        "UPDATE messages SET kind = 'chat.undo', text = ?2, media = '', quote = '', raw = '{}'
         WHERE seq = ?1",
        params![seq, media::RECALLED_TEXT],
    )?;
    // Reactions go with the content. (The table exists from version 4 on.)
    let has_reactions: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'reactions')",
        [],
        |r| r.get(0),
    )?;
    if has_reactions {
        conn.execute(
            "DELETE FROM reactions WHERE thread_id = ?1 AND message_id = ?2",
            params![thread_id, stored_id],
        )?;
    }
    Ok(Some(seq))
}

/// Recall notices stored as messages by earlier versions: apply them to the
/// message they recall and drop them.
fn apply_stored_recalls(conn: &Connection) -> Result<usize> {
    let notices: Vec<(i64, String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT seq, thread_id, raw FROM messages WHERE kind = 'chat.undo' AND raw <> '{}'",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<Result<_>>()?
    };
    let mut applied = 0;
    for (seq, thread_id, raw) in notices {
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        let Some((remote_id, client_id)) = media::recalled_ids(value.get("content")) else {
            continue;
        };
        if recall_in(conn, &thread_id, &remote_id, &client_id)?.is_some() {
            conn.execute("DELETE FROM messages WHERE seq = ?1", params![seq])?;
            applied += 1;
        }
    }
    Ok(applied)
}

fn upsert_person_in(conn: &Connection, p: &Person, friend: Option<bool>) -> Result<()> {
    conn.execute(
        "INSERT INTO people (uid, name, zalo_name, avatar, friend, updated_at)
         VALUES (?1, ?2, ?3, ?4, COALESCE(?5, 0), ?6)
         ON CONFLICT (uid) DO UPDATE SET
            name = CASE WHEN excluded.name <> '' THEN excluded.name ELSE people.name END,
            zalo_name = CASE WHEN excluded.zalo_name <> '' THEN excluded.zalo_name
                             ELSE people.zalo_name END,
            avatar = CASE WHEN excluded.avatar <> '' THEN excluded.avatar ELSE people.avatar END,
            friend = COALESCE(?5, people.friend),
            updated_at = excluded.updated_at",
        params![p.uid, p.name, p.zalo_name, p.avatar, friend, now_ms()],
    )?;
    Ok(())
}

/// The part of a message's text worth searching: all of a text message, but
/// not the label the summary of an attachment starts with (`[Hình ảnh] `,
/// `[Tập tin] `…).
fn searchable<'a>(kind: &str, text: &'a str) -> &'a str {
    if kind == "webchat" {
        return text;
    }
    match text.strip_prefix('[').and_then(|rest| rest.split_once(']')) {
        Some((label, rest)) if !label.contains('\n') && label.chars().count() <= 24 => rest,
        _ => text,
    }
}

fn device_from_row(row: &Row<'_>) -> Result<Device> {
    Ok(Device {
        id: row.get(0)?,
        name: row.get(1)?,
        created_at: row.get(2)?,
        last_seen_at: row.get(3)?,
        revoked: row.get(4)?,
    })
}

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self> {
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(Duration::from_secs(5))?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version < 1 {
            conn.execute_batch(SCHEMA_V1)?;
            conn.pragma_update(None, "user_version", 1)?;
        }
        if version < 3 {
            let tx = conn.transaction()?;
            if version < 2 {
                tx.execute_batch(SCHEMA_V2)?;
            }
            tx.execute_batch(SCHEMA_V3)?;
            let reparsed = reparse_content(&tx)?;
            let recalls = apply_stored_recalls(&tx)?;
            tx.pragma_update(None, "user_version", 3)?;
            tx.commit()?;
            if reparsed > 0 {
                tracing::info!(count = reparsed, recalls, "stored messages re-parsed");
            }
        }
        if version < 4 {
            let tx = conn.transaction()?;
            tx.execute_batch(SCHEMA_V4)?;
            tx.pragma_update(None, "user_version", 4)?;
            tx.commit()?;
        }
        if version < 5 {
            let tx = conn.transaction()?;
            tx.execute_batch(SCHEMA_V5)?;
            tx.pragma_update(None, "user_version", 5)?;
            tx.commit()?;
        }
        if version < 6 {
            let tx = conn.transaction()?;
            tx.execute_batch(SCHEMA_V6)?;
            tx.pragma_update(None, "user_version", 6)?;
            tx.commit()?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Create a thread or refresh it. Empty values never overwrite stored ones; a
    /// stored name is replaced only when `overwrite_name` is set (contact sync).
    pub fn upsert_thread(&self, t: &ThreadUpsert, overwrite_name: bool) -> Result<()> {
        self.conn().execute(
            "INSERT INTO threads (id, kind, remote_id, name, avatar) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (id) DO UPDATE SET
                name = CASE WHEN excluded.name <> '' AND (?6 OR threads.name = '')
                            THEN excluded.name ELSE threads.name END,
                avatar = CASE WHEN excluded.avatar <> '' THEN excluded.avatar ELSE threads.avatar END",
            params![t.id(), t.kind.as_str(), t.remote_id, t.name, t.avatar, overwrite_name],
        )?;
        Ok(())
    }

    /// Threads by most recent activity. Without `include_idle`, threads that never
    /// had a message (plain contacts) are left out.
    pub fn threads(&self, include_idle: bool, limit: i64, offset: i64) -> Result<Vec<Thread>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {THREAD_COLUMNS} FROM threads
             WHERE ?1 OR last_message_at > 0
             ORDER BY last_message_at DESC, name ASC LIMIT ?2 OFFSET ?3"
        ))?;
        let rows = stmt.query_map(params![include_idle, limit, offset], thread_from_row)?;
        rows.collect()
    }

    pub fn thread(&self, id: &str) -> Result<Option<Thread>> {
        self.conn()
            .query_row(
                &format!("SELECT {THREAD_COLUMNS} FROM threads WHERE id = ?1"),
                params![id],
                thread_from_row,
            )
            .optional()
    }

    /// Record the size of a group; 0 (unknown) keeps the stored size.
    pub fn set_members(&self, thread_id: &str, members: u64) -> Result<()> {
        if members > 0 {
            self.conn().execute(
                "UPDATE threads SET members = ?2 WHERE id = ?1",
                params![thread_id, i64::try_from(members).unwrap_or(i64::MAX)],
            )?;
        }
        Ok(())
    }

    /// Create or refresh a person: empty values keep what is stored, `friend`
    /// changes only when given. `updated_at` becomes now.
    pub fn upsert_person(&self, p: &Person, friend: Option<bool>) -> Result<()> {
        upsert_person_in(&self.conn(), p, friend)
    }

    /// The friend list as Zalo gave it: these people are friends, nobody else.
    pub fn set_friends(&self, friends: &[Person]) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute("UPDATE people SET friend = 0 WHERE friend <> 0", [])?;
        for p in friends {
            upsert_person_in(&tx, p, Some(true))?;
        }
        tx.commit()
    }

    /// The friend list.
    pub fn friends(&self) -> Result<Vec<Person>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {PERSON_COLUMNS} FROM people WHERE friend <> 0 ORDER BY name"
        ))?;
        let rows = stmt.query_map([], person_from_row)?;
        rows.collect()
    }

    /// The people among `uids` the store knows, in the order asked.
    pub fn people(&self, uids: &[String]) -> Result<Vec<Person>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {PERSON_COLUMNS} FROM people WHERE uid = ?1"
        ))?;
        let mut out = Vec::new();
        for uid in uids {
            if let Some(p) = stmt.query_row(params![uid], person_from_row).optional()? {
                out.push(p);
            }
        }
        Ok(out)
    }

    /// Note that Zalo was asked about these users at `at`, whether it answered
    /// for them or not, so they are not asked about again too soon.
    pub fn touch_people(&self, uids: &[String], at: i64) -> Result<()> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "INSERT INTO people (uid, updated_at) VALUES (?1, ?2)
             ON CONFLICT (uid) DO UPDATE SET updated_at = excluded.updated_at",
        )?;
        for uid in uids {
            stmt.execute(params![uid, at])?;
        }
        Ok(())
    }

    /// Messages whose text contains `query`, ignoring case and Vietnamese marks,
    /// newest first. Recalled messages and the labels of attachments
    /// (`[Hình ảnh]`…) are left out; a file name or a caption still counts.
    pub fn search_messages(&self, query: &str, limit: usize) -> Result<Vec<Message>> {
        let needle = fold(query.trim());
        if needle.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.conn();
        let mut found = Vec::new();
        {
            let mut stmt = conn.prepare(
                "SELECT seq, kind, text FROM messages WHERE kind <> 'chat.undo' AND text <> ''
                 ORDER BY seq DESC LIMIT ?1",
            )?;
            let mut rows = stmt.query(params![SEARCH_SCAN])?;
            while let Some(row) = rows.next()? {
                let kind: String = row.get(1)?;
                let text: String = row.get(2)?;
                if fold(searchable(&kind, &text)).contains(&needle) {
                    found.push(row.get::<_, i64>(0)?);
                    if found.len() >= limit {
                        break;
                    }
                }
            }
        }
        let mut stmt = conn.prepare(&format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages WHERE seq = ?1"
        ))?;
        found
            .iter()
            .map(|seq| stmt.query_row(params![seq], message_from_row))
            .collect()
    }

    /// The name `user_id` last went by in a thread (empty when unknown).
    pub fn sender_name(&self, thread_id: &str, user_id: &str) -> Result<String> {
        Ok(self
            .conn()
            .query_row(
                "SELECT sender_name FROM messages
                 WHERE thread_id = ?1 AND sender_id = ?2 AND sender_name <> ''
                 ORDER BY seq DESC LIMIT 1",
                params![thread_id, user_id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or_default())
    }

    /// Reset the unread counter. Returns false for an unknown thread.
    pub fn mark_read(&self, id: &str) -> Result<bool> {
        Ok(self
            .conn()
            .execute("UPDATE threads SET unread = 0 WHERE id = ?1", params![id])?
            > 0)
    }

    /// Original Zalo JSON of the newest incoming messages of a thread, newest
    /// first (for read receipts).
    pub fn incoming_raw(&self, thread_id: &str, limit: i64) -> Result<Vec<Value>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT raw FROM messages WHERE thread_id = ?1 AND outgoing = 0 AND raw <> '{}'
             ORDER BY seq DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![thread_id, limit], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for raw in rows {
            if let Ok(value) = serde_json::from_str(&raw?) {
                out.push(value);
            }
        }
        Ok(out)
    }

    /// Store a message once. Returns `None` when it is already known, either by its
    /// server id or by the client id of a message this gateway sent itself (Zalo
    /// echoes sent messages back over the socket).
    pub fn insert_message(&self, m: &NewMessage) -> Result<Option<Message>> {
        self.insert_message_with(m, true)
    }

    /// [`Store::insert_message`], choosing whether an incoming message raises the
    /// unread counter. History loaded at start-up does not: it may have been read
    /// on the phone already.
    pub fn insert_message_with(
        &self,
        m: &NewMessage,
        count_unread: bool,
    ) -> Result<Option<Message>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let known: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM messages WHERE thread_id = ?1 AND remote_id = ?2)",
            params![m.thread_id, m.remote_id],
            |r| r.get(0),
        )?;
        if known {
            return Ok(None);
        }
        if !m.client_id.is_empty() {
            let sent_here = tx.execute(
                "UPDATE messages SET remote_id = ?3
                 WHERE thread_id = ?1 AND client_id = ?2 AND remote_id LIKE 'local:%'",
                params![m.thread_id, m.client_id, m.remote_id],
            )?;
            let same_client: bool = tx.query_row(
                "SELECT EXISTS (SELECT 1 FROM messages WHERE thread_id = ?1 AND client_id = ?2)",
                params![m.thread_id, m.client_id],
                |r| r.get(0),
            )?;
            if sent_here > 0 || same_client {
                tx.commit()?;
                return Ok(None);
            }
        }
        tx.execute(
            "INSERT INTO messages
                (thread_id, remote_id, client_id, sender_id, sender_name, outgoing, kind, text,
                 sent_at, raw, media, quote)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                m.thread_id,
                m.remote_id,
                m.client_id,
                m.sender_id,
                m.sender_name,
                m.outgoing,
                m.kind,
                m.text,
                m.sent_at,
                m.raw,
                to_json(m.media.as_ref()),
                to_json(m.quote.as_ref())
            ],
        )?;
        let seq = tx.last_insert_rowid();
        tx.execute(
            "UPDATE threads SET last_message_at = MAX(last_message_at, ?2), unread = unread + ?3
             WHERE id = ?1",
            params![
                m.thread_id,
                m.sent_at,
                i64::from(count_unread && !m.outgoing)
            ],
        )?;
        tx.commit()?;
        Ok(Some(Message {
            seq,
            thread_id: m.thread_id.clone(),
            remote_id: m.remote_id.clone(),
            client_id: m.client_id.clone(),
            sender_id: m.sender_id.clone(),
            sender_name: m.sender_name.clone(),
            outgoing: m.outgoing,
            kind: m.kind.clone(),
            text: m.text.clone(),
            media: m.media.clone(),
            quote: m.quote.clone(),
            sent_at: m.sent_at,
            reactions: Vec::new(),
            status: 0,
        }))
    }

    /// Apply a recall: the message with `remote_id` (or, when it was sent from
    /// here, `client_id`) loses its content. Returns the updated message, `None`
    /// when it is not stored or already recalled.
    pub fn recall_message(
        &self,
        thread_id: &str,
        remote_id: &str,
        client_id: &str,
    ) -> Result<Option<Message>> {
        let recalled = recall_in(&self.conn(), thread_id, remote_id, client_id)?;
        match recalled {
            Some(seq) => self.message(seq),
            None => Ok(None),
        }
    }

    /// The message this gateway sent with `client_id`, if stored.
    pub fn message_by_client_id(
        &self,
        thread_id: &str,
        client_id: &str,
    ) -> Result<Option<Message>> {
        self.conn()
            .query_row(
                &format!("SELECT {MESSAGE_COLUMNS} FROM messages WHERE thread_id = ?1 AND client_id = ?2"),
                params![thread_id, client_id],
                message_from_row,
            )
            .optional()
    }

    /// Up to `limit` messages of a thread older than `before` (a `seq`), oldest first.
    pub fn messages(
        &self,
        thread_id: &str,
        before: Option<i64>,
        limit: i64,
    ) -> Result<Vec<Message>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages
             WHERE thread_id = ?1 AND seq < ?2 ORDER BY seq DESC LIMIT ?3"
        ))?;
        let rows = stmt.query_map(
            params![thread_id, before.unwrap_or(i64::MAX), limit],
            message_from_row,
        )?;
        let mut out: Vec<Message> = rows.collect::<Result<_>>()?;
        out.reverse();
        attach_reactions(&conn, thread_id, &mut out)?;
        Ok(out)
    }

    /// One of this account's messages (by any of Zalo's ids for it) reached the
    /// other side, `seen` or just delivered: it and the account's earlier
    /// messages in that conversation move up to that status. Returns the
    /// message when it is the conversation's newest of the account's and its
    /// status changed (clients show the status under that one).
    pub fn mark_receipt(&self, msg_ids: &[String], seen: bool) -> Result<Option<Message>> {
        let status: i64 = if seen { 2 } else { 1 };
        let newest = {
            let conn = self.conn();
            let mut found: Option<(i64, String)> = None;
            for id in msg_ids.iter().filter(|id| !id.is_empty()) {
                // Zalo's id first; the client id of a message sent from here
                // in case a receipt names that one.
                found = conn
                    .query_row(
                        "SELECT seq, thread_id FROM messages
                         WHERE outgoing = 1 AND (remote_id = ?1 OR client_id = ?1)
                         ORDER BY remote_id = ?1 DESC, seq DESC LIMIT 1",
                        params![id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                if found.is_some() {
                    break;
                }
            }
            let Some((seq, thread_id)) = found else {
                return Ok(None);
            };
            let newest: Option<(i64, i64)> = conn
                .query_row(
                    "SELECT seq, status FROM messages WHERE thread_id = ?1 AND outgoing = 1
                     ORDER BY seq DESC LIMIT 1",
                    params![thread_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            conn.execute(
                "UPDATE messages SET status = ?3
                 WHERE thread_id = ?1 AND outgoing = 1 AND seq <= ?2 AND status < ?3",
                params![thread_id, seq, status],
            )?;
            match newest {
                Some((newest, before)) if newest == seq && before < status => Some(seq),
                _ => None,
            }
        };
        match newest {
            Some(seq) => self.message(seq),
            None => Ok(None),
        }
    }

    /// The account read a thread up to `last_msg_id` on another device: its
    /// unread count drops to the incoming messages after that one (to none
    /// when that message is not stored here). Returns the new count when it
    /// dropped.
    pub fn read_elsewhere(&self, thread_id: &str, last_msg_id: &str) -> Result<Option<i64>> {
        let conn = self.conn();
        let current: Option<i64> = conn
            .query_row(
                "SELECT unread FROM threads WHERE id = ?1",
                params![thread_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(current) = current.filter(|n| *n > 0) else {
            return Ok(None);
        };
        let read_up_to: Option<i64> = if last_msg_id.is_empty() {
            None
        } else {
            conn.query_row(
                "SELECT seq FROM messages WHERE thread_id = ?1 AND remote_id = ?2",
                params![thread_id, last_msg_id],
                |r| r.get(0),
            )
            .optional()?
        };
        let left: i64 = match read_up_to {
            Some(seq) => conn.query_row(
                "SELECT COUNT(*) FROM messages WHERE thread_id = ?1 AND outgoing = 0 AND seq > ?2",
                params![thread_id, seq],
                |r| r.get(0),
            )?,
            None => 0,
        };
        let left = left.min(current);
        if left == current {
            return Ok(None);
        }
        conn.execute(
            "UPDATE threads SET unread = ?2 WHERE id = ?1",
            params![thread_id, left],
        )?;
        Ok(Some(left))
    }

    /// Remove a message (and its reactions) for good; returns what it was.
    pub fn delete_message(&self, seq: i64) -> Result<Option<Message>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let found = tx
            .query_row(
                &format!("SELECT {MESSAGE_COLUMNS} FROM messages WHERE seq = ?1"),
                params![seq],
                message_from_row,
            )
            .optional()?;
        if let Some(m) = &found {
            tx.execute(
                "DELETE FROM reactions WHERE thread_id = ?1 AND message_id = ?2",
                params![m.thread_id, m.remote_id],
            )?;
            tx.execute("DELETE FROM messages WHERE seq = ?1", params![seq])?;
        }
        tx.commit()?;
        Ok(found)
    }

    /// Messages of a thread whose `kind` (Zalo's `msgType`) is one of `kinds`,
    /// older than `seq = before`, newest first.
    pub fn messages_of_kinds(
        &self,
        thread_id: &str,
        kinds: &[&str],
        before: Option<i64>,
        limit: i64,
    ) -> Result<Vec<Message>> {
        if kinds.is_empty() {
            return Ok(Vec::new());
        }
        let marks: Vec<String> = (0..kinds.len()).map(|i| format!("?{}", i + 4)).collect();
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages
             WHERE thread_id = ?1 AND seq < ?2 AND kind IN ({})
             ORDER BY seq DESC LIMIT ?3",
            marks.join(", ")
        ))?;
        let mut values: Vec<rusqlite::types::Value> = vec![
            thread_id.to_string().into(),
            before.unwrap_or(i64::MAX).into(),
            limit.into(),
        ];
        values.extend(kinds.iter().map(|k| (*k).to_string().into()));
        let rows = stmt.query_map(rusqlite::params_from_iter(values), message_from_row)?;
        rows.collect()
    }

    /// One message, with its reactions.
    pub fn message(&self, seq: i64) -> Result<Option<Message>> {
        let conn = self.conn();
        let found = conn
            .query_row(
                &format!("SELECT {MESSAGE_COLUMNS} FROM messages WHERE seq = ?1"),
                params![seq],
                message_from_row,
            )
            .optional()?;
        let Some(message) = found else {
            return Ok(None);
        };
        let thread_id = message.thread_id.clone();
        let mut one = [message];
        attach_reactions(&conn, &thread_id, &mut one)?;
        let [message] = one;
        Ok(Some(message))
    }

    /// The original Zalo JSON of a message (`Null` for one sent from here).
    pub fn message_raw(&self, seq: i64) -> Result<Value> {
        let raw: Option<String> = self
            .conn()
            .query_row(
                "SELECT raw FROM messages WHERE seq = ?1",
                params![seq],
                |r| r.get(0),
            )
            .optional()?;
        Ok(raw
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .filter(|v| v.as_object().is_some_and(|o| !o.is_empty()))
            .unwrap_or(Value::Null))
    }

    /// Store a reaction once (by its id, or by its sender's client id). Returns
    /// the stored messages it changed, with their reactions; a reaction to a
    /// message that is not stored is kept for when it is.
    pub fn add_reaction(&self, r: &NewReaction) -> Result<Vec<Message>> {
        let conn = self.conn();
        let mut changed = Vec::new();
        for (global_id, client_id) in &r.targets {
            // The stored message, by server id or, for one sent from here, client id.
            let found: Option<i64> = conn
                .query_row(
                    "SELECT seq FROM messages WHERE thread_id = ?1
                       AND ((?2 <> '' AND ?2 <> '0' AND remote_id = ?2)
                         OR (?3 <> '' AND ?3 <> '0' AND client_id = ?3))
                     ORDER BY seq LIMIT 1",
                    params![r.thread_id, global_id, client_id],
                    |row| row.get(0),
                )
                .optional()?;
            let message_id: String = match found {
                Some(seq) => conn.query_row(
                    "SELECT remote_id FROM messages WHERE seq = ?1",
                    params![seq],
                    |row| row.get(0),
                )?,
                None => global_id.clone(),
            };
            let inserted = conn.execute(
                "INSERT OR IGNORE INTO reactions
                    (thread_id, action_id, message_id, client_id, user_id, mine, icon, sent_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    r.thread_id,
                    r.action_id,
                    message_id,
                    r.client_id,
                    r.user_id,
                    r.mine,
                    r.icon,
                    r.sent_at
                ],
            )?;
            if let (1, Some(seq)) = (inserted, found) {
                let message = conn.query_row(
                    &format!("SELECT {MESSAGE_COLUMNS} FROM messages WHERE seq = ?1"),
                    params![seq],
                    message_from_row,
                )?;
                changed.push(message);
            }
        }
        attach_reactions(&conn, &r.thread_id, &mut changed)?;
        Ok(changed)
    }

    /// Append to the event log; returns the event's sequence number.
    pub fn append_event(&self, kind: &str, payload: &Value, created_at: i64) -> Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO events (kind, payload, created_at) VALUES (?1, ?2, ?3)",
            params![kind, payload.to_string(), created_at],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Sequence number of the newest event, 0 when the log is empty.
    pub fn last_event_seq(&self) -> Result<i64> {
        self.conn()
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |r| r.get(0))
    }

    /// Drop events created before `before_ms`. Clients that were away longer
    /// reload their lists instead of replaying.
    pub fn prune_events(&self, before_ms: i64) -> Result<usize> {
        self.conn().execute(
            "DELETE FROM events WHERE created_at < ?1",
            params![before_ms],
        )
    }

    /// Events after `since`, oldest first.
    pub fn events_since(&self, since: i64, limit: i64) -> Result<Vec<StoredEvent>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT seq, kind, payload, created_at FROM events WHERE seq > ?1 ORDER BY seq LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![since, limit], |r| {
            let payload: String = r.get(2)?;
            Ok(StoredEvent {
                seq: r.get(0)?,
                kind: r.get(1)?,
                payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
                created_at: r.get(3)?,
            })
        })?;
        rows.collect()
    }

    /// Register a device; returns it with its token, which is shown only once.
    pub fn add_device(&self, name: &str) -> Result<(Device, String)> {
        let token = auth::new_token();
        let device = Device {
            id: auth::new_device_id(),
            name: name.to_string(),
            created_at: now_ms(),
            last_seen_at: 0,
            revoked: false,
        };
        self.conn().execute(
            "INSERT INTO devices (id, name, token_hash, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                device.id,
                device.name,
                auth::hash_token(&token),
                device.created_at
            ],
        )?;
        Ok((device, token))
    }

    /// The active device owning `token`, refreshing its last-seen time.
    pub fn device_for_token(&self, token: &str) -> Result<Option<Device>> {
        let conn = self.conn();
        let device = conn
            .query_row(
                "SELECT id, name, created_at, last_seen_at, revoked FROM devices
                 WHERE token_hash = ?1 AND revoked = 0",
                params![auth::hash_token(token)],
                device_from_row,
            )
            .optional()?;
        if let Some(d) = &device {
            conn.execute(
                "UPDATE devices SET last_seen_at = ?2 WHERE id = ?1",
                params![d.id, now_ms()],
            )?;
        }
        Ok(device)
    }

    pub fn devices(&self) -> Result<Vec<Device>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, name, created_at, last_seen_at, revoked FROM devices ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], device_from_row)?;
        rows.collect()
    }

    /// Revoke a device. Returns false for an unknown id.
    pub fn revoke_device(&self, id: &str) -> Result<bool> {
        Ok(self
            .conn()
            .execute("UPDATE devices SET revoked = 1 WHERE id = ?1", params![id])?
            > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{thread_id, Media, Quote};

    fn user(remote: &str, name: &str) -> ThreadUpsert {
        ThreadUpsert {
            kind: ThreadKind::User,
            remote_id: remote.into(),
            name: name.into(),
            avatar: String::new(),
        }
    }

    fn msg(thread: &str, remote: &str, client: &str, outgoing: bool, at: i64) -> NewMessage {
        NewMessage {
            thread_id: thread.into(),
            remote_id: remote.into(),
            client_id: client.into(),
            sender_id: "s".into(),
            sender_name: String::new(),
            outgoing,
            kind: "webchat".into(),
            text: format!("text {remote}"),
            media: None,
            quote: None,
            sent_at: at,
            raw: "{}".into(),
        }
    }

    #[test]
    fn incoming_raw_messages_newest_first() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_thread(&user("1", "A"), true).unwrap();
        let t = thread_id(ThreadKind::User, "1");
        for (i, outgoing) in [false, true, false, false].into_iter().enumerate() {
            let mut m = msg(&t, &format!("m{i}"), "", outgoing, i as i64);
            m.raw = serde_json::json!({ "msgId": format!("m{i}") }).to_string();
            s.insert_message(&m).unwrap();
        }
        // Sent from here: no original JSON, never part of a receipt.
        s.insert_message(&msg(&t, "m9", "", false, 9)).unwrap();
        let raws = s.incoming_raw(&t, 2).unwrap();
        let ids: Vec<&str> = raws.iter().map(|v| v["msgId"].as_str().unwrap()).collect();
        assert_eq!(ids, ["m3", "m2"]);
    }

    #[test]
    fn recalls_drop_the_content_of_a_message() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_thread(&user("1", "A"), true).unwrap();
        let t = thread_id(ThreadKind::User, "1");
        let mut reply = msg(&t, "m1", "c1", false, 1);
        reply.raw = r#"{"content":"secret"}"#.into();
        reply.quote = Some(Quote {
            sender_name: "B".into(),
            text: "q".into(),
            remote_id: "m0".into(),
        });
        let stored = s.insert_message(&reply).unwrap().unwrap();
        assert_eq!(stored.quote, reply.quote);
        s.insert_message(&msg(&t, "local:9", "9", true, 2)).unwrap();

        let recalled = s.recall_message(&t, "m1", "").unwrap().unwrap();
        assert_eq!(
            (recalled.kind.as_str(), recalled.text.as_str()),
            ("chat.undo", media::RECALLED_TEXT)
        );
        assert_eq!((recalled.media, recalled.quote), (None, None));
        let raw: String = s
            .conn()
            .query_row(
                "SELECT raw FROM messages WHERE seq = ?1",
                [stored.seq],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(raw, "{}");
        // Already recalled, unknown, or matched by the client id of a message sent here.
        assert!(s.recall_message(&t, "m1", "").unwrap().is_none());
        assert!(s.recall_message(&t, "m404", "").unwrap().is_none());
        assert!(s.recall_message(&t, "srv-9", "9").unwrap().is_some());
        assert_eq!(s.thread(&t).unwrap().unwrap().preview, media::RECALLED_TEXT);
    }

    #[test]
    fn reactions_add_up_per_icon_and_can_be_taken_back() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_thread(&user("1", "A"), true).unwrap();
        let t = thread_id(ThreadKind::User, "1");
        let first = s
            .insert_message(&msg(&t, "m1", "c1", false, 1))
            .unwrap()
            .unwrap();
        s.insert_message(&msg(&t, "local:7", "7", true, 2)).unwrap();
        let react = |action: &str, client: &str, user: &str, icon: &str, at: i64| NewReaction {
            thread_id: t.clone(),
            action_id: action.into(),
            client_id: client.into(),
            targets: vec![("m1".into(), "c1".into())],
            user_id: user.into(),
            mine: user == "me",
            icon: icon.into(),
            sent_at: at,
        };
        let counts = |m: &Message| {
            m.reactions
                .iter()
                .map(|r| (r.emoji.clone(), r.count, r.mine))
                .collect::<Vec<_>>()
        };

        let changed = s
            .add_reaction(&react("a1", "", "1", "/-heart", 10))
            .unwrap();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].seq, first.seq);
        s.add_reaction(&react("a2", "", "1", "/-heart", 11))
            .unwrap();
        s.add_reaction(&react("a3", "55", "me", "/-strong", 12))
            .unwrap();
        let both = s
            .add_reaction(&react("a4", "", "me", "/-heart", 13))
            .unwrap();
        assert_eq!(
            counts(&both[0]),
            [("❤️".to_string(), 3, true), ("👍".to_string(), 1, true)]
        );
        // Seen again (by id, or as the echo of our own client id): not counted.
        assert!(s
            .add_reaction(&react("a1", "", "1", "/-heart", 10))
            .unwrap()
            .is_empty());
        assert!(s
            .add_reaction(&react("echo", "55", "me", "/-strong", 12))
            .unwrap()
            .is_empty());

        // Taking back clears that user's reactions only, even if older ones come late.
        s.add_reaction(&react("a5", "", "me", "", 14)).unwrap();
        s.add_reaction(&react("a0", "", "me", ":>", 9)).unwrap();
        let listed = s.messages(&t, None, 10).unwrap();
        assert_eq!(counts(&listed[0]), [("❤️".to_string(), 2, false)]);
        assert!(listed[1].reactions.is_empty());
        assert_eq!(
            counts(&s.message(first.seq).unwrap().unwrap()),
            counts(&listed[0])
        );

        // A message sent from here is found by its client id; unknown messages
        // keep the reaction for later; a recall drops the reactions.
        let mut by_client = react("b1", "", "1", ":o", 20);
        by_client.targets = vec![("0".into(), "7".into()), ("m404".into(), "x".into())];
        let changed = s.add_reaction(&by_client).unwrap();
        assert_eq!(changed.len(), 1);
        assert_eq!(counts(&changed[0]), [("😮".to_string(), 1, false)]);
        s.recall_message(&t, "m1", "").unwrap();
        assert!(s.messages(&t, None, 10).unwrap()[0].reactions.is_empty());
    }

    #[test]
    fn media_is_stored_with_the_message() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_thread(&user("1", "A"), true).unwrap();
        let t = thread_id(ThreadKind::User, "1");
        let mut photo = msg(&t, "p1", "", false, 1);
        photo.media = Some(Media {
            kind: "image".into(),
            url: "https://photo.zdn.vn/a.jpg".into(),
            width: 4,
            height: 3,
            ..Media::default()
        });
        let stored = s.insert_message(&photo).unwrap().unwrap();
        assert_eq!(stored.media, photo.media);
        assert_eq!(s.messages(&t, None, 10).unwrap()[0].media, photo.media);
        s.insert_message(&msg(&t, "p2", "", false, 2)).unwrap();
        assert_eq!(s.messages(&t, None, 10).unwrap()[1].media, None);
    }

    #[test]
    fn version_1_databases_are_upgraded_from_the_stored_json() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        conn.execute(
            "INSERT INTO threads (id, kind, remote_id) VALUES ('zalo:u:1', 'user', '1')",
            [],
        )
        .unwrap();
        let photo = serde_json::json!({
            "msgType": "chat.photo",
            "content": {
                "href": "https://photo.zdn.vn/a.jpg",
                "title": "",
                "params": serde_json::json!({ "width": 800, "height": 600 }).to_string()
            }
        });
        let reply = serde_json::json!({
            "msgType": "webchat", "content": "ok",
            "quote": { "msg": "ăn chưa?", "fromD": "Lan", "globalMsgId": 5 }
        });
        let recalled = serde_json::json!({ "msgType": "webchat", "content": "oops" });
        let notice = serde_json::json!({
            "msgType": "chat.undo",
            "content": { "globalMsgId": 900, "cliMsgId": 901, "deleteMsg": 1 }
        });
        for (remote, kind, raw) in [
            ("m1", "chat.photo", photo),
            ("m2", "webchat", reply),
            ("900", "webchat", recalled),
            ("m4", "chat.undo", notice),
        ] {
            conn.execute(
                "INSERT INTO messages (thread_id, remote_id, outgoing, kind, text, sent_at, raw)
                 VALUES ('zalo:u:1', ?1, 0, ?2, '', 1, ?3)",
                params![remote, kind, raw.to_string()],
            )
            .unwrap();
        }
        let s = Store::init(conn).unwrap();
        let messages = s.messages("zalo:u:1", None, 10).unwrap();
        // The recall notice was applied to its message and dropped.
        assert_eq!(messages.len(), 3);
        let media = messages[0].media.as_ref().unwrap();
        assert_eq!(
            (media.kind.as_str(), media.width, media.height),
            ("image", 800, 600)
        );
        assert_eq!(messages[0].text, "[Hình ảnh]");
        let quote = messages[1].quote.as_ref().unwrap();
        assert_eq!(
            (quote.sender_name.as_str(), quote.text.as_str()),
            ("Lan", "ăn chưa?")
        );
        assert_eq!(
            (messages[2].kind.as_str(), messages[2].text.as_str()),
            ("chat.undo", media::RECALLED_TEXT)
        );
        let version: i64 = s
            .conn()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, 6);
        // Version 5 added people and group sizes, version 6 message status.
        assert!(s
            .messages("zalo:u:1", None, 10)
            .unwrap()
            .iter()
            .all(|m| m.status == 0));
        assert!(s.friends().unwrap().is_empty());
        assert_eq!(s.thread("zalo:u:1").unwrap().unwrap().members, 0);
    }

    fn person(uid: &str, name: &str, avatar: &str) -> Person {
        Person {
            uid: uid.into(),
            name: name.into(),
            avatar: avatar.into(),
            ..Person::default()
        }
    }

    #[test]
    fn friends_follow_the_latest_list_and_details_fill_in() {
        let s = Store::open_in_memory().unwrap();
        s.set_friends(&[person("1", "Lan", "https://a/1"), person("2", "Nam", "")])
            .unwrap();
        // A group member, who is no friend.
        s.upsert_person(&person("3", "Bố", ""), None).unwrap();
        let names = |list: Vec<Person>| list.into_iter().map(|p| p.name).collect::<Vec<_>>();
        assert_eq!(names(s.friends().unwrap()), ["Lan", "Nam"]);

        // Nam is no longer a friend; what was known of him stays.
        s.set_friends(&[person("1", "", "")]).unwrap();
        assert_eq!(names(s.friends().unwrap()), ["Lan"]);
        let asked = ["2".to_string(), "1".to_string(), "9".to_string()];
        let found = s.people(&asked).unwrap();
        assert_eq!(
            found
                .iter()
                .map(|p| (p.uid.as_str(), p.name.as_str(), p.avatar.as_str(), p.friend))
                .collect::<Vec<_>>(),
            [("2", "Nam", "", false), ("1", "Lan", "https://a/1", true)]
        );

        // Asked about but unknown to Zalo: remembered as asked, still nameless.
        s.touch_people(&["9".to_string()], 1234).unwrap();
        let nobody = s.people(&["9".to_string()]).unwrap();
        assert_eq!((nobody[0].name.as_str(), nobody[0].updated_at), ("", 1234));
        // Touching keeps names.
        s.touch_people(&["3".to_string()], 99).unwrap();
        assert_eq!(s.people(&["3".to_string()]).unwrap()[0].name, "Bố");
    }

    #[test]
    fn receipts_move_messages_up_and_report_the_newest() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_thread(&user("1", "Lan"), true).unwrap();
        for (remote, at) in [("o1", 1), ("o2", 2), ("o3", 3)] {
            s.insert_message(&msg("zalo:u:1", remote, "", true, at))
                .unwrap();
        }
        // An older message delivered: no change to show on the newest.
        assert!(s.mark_receipt(&["o2".into()], false).unwrap().is_none());
        // The newest seen, under one of its two ids.
        let shown = s
            .mark_receipt(&["9999".into(), "o3".into()], true)
            .unwrap()
            .unwrap();
        assert_eq!((shown.remote_id.as_str(), shown.status), ("o3", 2));
        let statuses: Vec<i64> = s
            .messages("zalo:u:1", None, 10)
            .unwrap()
            .iter()
            .map(|m| m.status)
            .collect();
        assert_eq!(statuses, [2, 2, 2]);
        // A late delivered receipt does not take "seen" back.
        assert!(s.mark_receipt(&["o3".into()], false).unwrap().is_none());
        assert!(s.mark_receipt(&["nobody".into()], true).unwrap().is_none());
        // A receipt naming the client id of a message sent from here.
        s.insert_message(&msg("zalo:u:1", "o4", "c4", true, 4))
            .unwrap();
        let shown = s.mark_receipt(&["c4".into()], false).unwrap().unwrap();
        assert_eq!((shown.remote_id.as_str(), shown.status), ("o4", 1));
    }

    #[test]
    fn reading_on_another_device_clears_unread_up_to_a_message() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_thread(&user("1", "Lan"), true).unwrap();
        for (remote, at) in [("i1", 1), ("i2", 2), ("i3", 3)] {
            s.insert_message(&msg("zalo:u:1", remote, "", false, at))
                .unwrap();
        }
        assert_eq!(s.thread("zalo:u:1").unwrap().unwrap().unread, 3);
        // Read up to i2 elsewhere: i3 is still unread.
        assert_eq!(s.read_elsewhere("zalo:u:1", "i2").unwrap(), Some(1));
        assert_eq!(s.read_elsewhere("zalo:u:1", "i2").unwrap(), None);
        // A message not stored here: everything was read.
        assert_eq!(s.read_elsewhere("zalo:u:1", "x").unwrap(), Some(0));
        assert_eq!(s.read_elsewhere("zalo:u:1", "").unwrap(), None);
        assert_eq!(s.read_elsewhere("zalo:u:9", "i1").unwrap(), None);
    }

    #[test]
    fn deleted_messages_take_their_reactions_with_them() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_thread(&user("1", "Lan"), true).unwrap();
        let first = s
            .insert_message(&msg("zalo:u:1", "a", "", false, 1))
            .unwrap()
            .unwrap();
        s.insert_message(&msg("zalo:u:1", "b", "", false, 2))
            .unwrap();
        s.add_reaction(&NewReaction {
            thread_id: "zalo:u:1".into(),
            action_id: "r1".into(),
            client_id: String::new(),
            targets: vec![("a".into(), String::new())],
            user_id: "1".into(),
            mine: false,
            icon: "/-heart".into(),
            sent_at: 3,
        })
        .unwrap();
        let gone = s.delete_message(first.seq).unwrap().unwrap();
        assert_eq!(gone.remote_id, "a");
        assert!(s.delete_message(first.seq).unwrap().is_none());
        let left = s.messages("zalo:u:1", None, 10).unwrap();
        assert_eq!(left.len(), 1);
        let reactions: i64 = s
            .conn()
            .query_row("SELECT COUNT(*) FROM reactions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(reactions, 0);
    }

    #[test]
    fn group_sizes_are_kept_unless_known_again() {
        let s = Store::open_in_memory().unwrap();
        let group = ThreadUpsert {
            kind: ThreadKind::Group,
            remote_id: "50".into(),
            name: "Nhà".into(),
            avatar: String::new(),
        };
        s.upsert_thread(&group, true).unwrap();
        s.set_members("zalo:g:50", 12).unwrap();
        s.set_members("zalo:g:50", 0).unwrap();
        assert_eq!(s.thread("zalo:g:50").unwrap().unwrap().members, 12);
    }

    #[test]
    fn search_ignores_case_marks_and_attachment_labels() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_thread(&user("1", "Lan"), true).unwrap();
        let add = |remote: &str, kind: &str, text: &str, at: i64| {
            let mut m = msg("zalo:u:1", remote, "", false, at);
            m.kind = kind.into();
            m.text = text.into();
            s.insert_message(&m).unwrap();
        };
        add("a", "webchat", "Đi ăn phở không?", 1);
        add("b", "chat.photo", "[Hình ảnh]", 2);
        add("c", "share.file", "[Tập tin] Hình nền.png", 3);
        add("d", "webchat", "[Hình ảnh] là chữ người gõ", 4);
        add("e", "webchat", "PHỞ bò nhé", 5);
        let texts = |q: &str| {
            s.search_messages(q, 10)
                .unwrap()
                .into_iter()
                .map(|m| m.remote_id)
                .collect::<Vec<_>>()
        };
        assert_eq!(texts("pho"), ["e", "a"]);
        assert_eq!(texts("hinh"), ["d", "c"]);
        assert_eq!(texts("  "), Vec::<String>::new());
        assert_eq!(s.search_messages("pho", 1).unwrap().len(), 1);
        assert_eq!(fold("Đặng Thị Ánh"), "dang thi anh");
    }

    #[test]
    fn thread_names_fill_in_but_only_contacts_overwrite() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_thread(&user("1", ""), false).unwrap();
        s.upsert_thread(&user("1", "From message"), false).unwrap();
        assert_eq!(s.thread("zalo:u:1").unwrap().unwrap().name, "From message");
        s.upsert_thread(&user("1", "Other"), false).unwrap();
        assert_eq!(s.thread("zalo:u:1").unwrap().unwrap().name, "From message");
        s.upsert_thread(&user("1", "Contact"), true).unwrap();
        s.upsert_thread(&user("1", ""), true).unwrap();
        assert_eq!(s.thread("zalo:u:1").unwrap().unwrap().name, "Contact");
    }

    #[test]
    fn messages_are_stored_once_and_update_the_thread() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_thread(&user("1", "A"), true).unwrap();
        let t = thread_id(ThreadKind::User, "1");
        assert!(s
            .insert_message(&msg(&t, "m1", "", false, 100))
            .unwrap()
            .is_some());
        assert!(s
            .insert_message(&msg(&t, "m1", "", false, 100))
            .unwrap()
            .is_none());
        assert!(s
            .insert_message(&msg(&t, "m2", "", true, 200))
            .unwrap()
            .is_some());
        let th = s.thread(&t).unwrap().unwrap();
        assert_eq!((th.last_message_at, th.unread), (200, 1));
        assert!(s.mark_read(&t).unwrap());
        assert_eq!(s.thread(&t).unwrap().unwrap().unread, 0);
        assert!(!s.mark_read("zalo:u:404").unwrap());
    }

    #[test]
    fn server_echo_of_a_sent_message_is_not_duplicated() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_thread(&user("1", "A"), true).unwrap();
        let t = thread_id(ThreadKind::User, "1");
        let sent = s
            .insert_message(&msg(&t, "local:777", "777", true, 1))
            .unwrap()
            .unwrap();
        // The echo carries the server id and the same client id.
        assert!(s
            .insert_message(&msg(&t, "srv-1", "777", true, 1))
            .unwrap()
            .is_none());
        let stored = s.message_by_client_id(&t, "777").unwrap().unwrap();
        assert_eq!((stored.seq, stored.remote_id.as_str()), (sent.seq, "srv-1"));
        assert_eq!(s.messages(&t, None, 10).unwrap().len(), 1);
    }

    #[test]
    fn messages_page_backwards_oldest_first() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_thread(&user("1", "A"), true).unwrap();
        let t = thread_id(ThreadKind::User, "1");
        for i in 0..5 {
            s.insert_message(&msg(&t, &format!("m{i}"), "", false, i))
                .unwrap();
        }
        let last_two = s.messages(&t, None, 2).unwrap();
        assert_eq!(
            last_two
                .iter()
                .map(|m| m.remote_id.as_str())
                .collect::<Vec<_>>(),
            ["m3", "m4"]
        );
        let before = s.messages(&t, Some(last_two[0].seq), 2).unwrap();
        assert_eq!(
            before
                .iter()
                .map(|m| m.remote_id.as_str())
                .collect::<Vec<_>>(),
            ["m1", "m2"]
        );
    }

    #[test]
    fn history_does_not_raise_unread() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_thread(&user("1", "A"), true).unwrap();
        let t = thread_id(ThreadKind::User, "1");
        s.insert_message_with(&msg(&t, "old", "", false, 5), false)
            .unwrap();
        let th = s.thread(&t).unwrap().unwrap();
        assert_eq!((th.unread, th.last_message_at), (0, 5));
    }

    #[test]
    fn idle_threads_are_hidden_by_default() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_thread(&user("1", "Contact"), true).unwrap();
        s.upsert_thread(&user("2", "Chat"), true).unwrap();
        s.insert_message(&msg("zalo:u:2", "m", "", false, 5))
            .unwrap();
        assert_eq!(s.threads(false, 50, 0).unwrap().len(), 1);
        assert_eq!(s.threads(true, 50, 0).unwrap().len(), 2);
    }

    #[test]
    fn event_log_replays_after_a_sequence() {
        let s = Store::open_in_memory().unwrap();
        let a = s
            .append_event("x", &serde_json::json!({ "n": 1 }), 1)
            .unwrap();
        let b = s
            .append_event("y", &serde_json::json!({ "n": 2 }), 2)
            .unwrap();
        let after_a = s.events_since(a, 10).unwrap();
        assert_eq!(after_a.len(), 1);
        assert_eq!((after_a[0].seq, after_a[0].kind.as_str()), (b, "y"));
    }

    #[test]
    fn devices_authenticate_until_revoked() {
        let s = Store::open_in_memory().unwrap();
        let (device, token) = s.add_device("Laptop").unwrap();
        assert_eq!(s.device_for_token(&token).unwrap().unwrap().id, device.id);
        assert!(s.device_for_token("zc_wrong").unwrap().is_none());
        assert!(s.revoke_device(&device.id).unwrap());
        assert!(s.device_for_token(&token).unwrap().is_none());
        assert!(s.devices().unwrap()[0].revoked);
    }
}
