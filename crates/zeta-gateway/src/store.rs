//! SQLite store: threads, messages, the event log and client devices.
//!
//! One connection behind a mutex is plenty for a single-user gateway: every
//! operation is a short statement, and none is held across an `.await`.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension, Row};
use serde_json::Value;

use crate::auth;
use crate::media;
use crate::model::{
    now_ms, Device, Message, NewMessage, StoredEvent, Thread, ThreadKind, ThreadUpsert,
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

const THREAD_COLUMNS: &str = "id, kind, remote_id, name, avatar, last_message_at, unread, \
    COALESCE((SELECT m.text FROM messages m WHERE m.thread_id = threads.id \
              ORDER BY m.seq DESC LIMIT 1), '')";
const MESSAGE_COLUMNS: &str = "seq, thread_id, remote_id, client_id, sender_id, sender_name, \
    outgoing, kind, text, sent_at, media, quote";

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
    })
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
    })
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
fn recall_in(
    conn: &Connection,
    thread_id: &str,
    remote_id: &str,
    client_id: &str,
) -> Result<Option<Message>> {
    let seq: Option<i64> = conn
        .query_row(
            "SELECT seq FROM messages WHERE thread_id = ?1 AND kind <> 'chat.undo'
               AND ((?2 <> '' AND remote_id = ?2) OR (?3 <> '' AND client_id = ?3))
             ORDER BY seq LIMIT 1",
            params![thread_id, remote_id, client_id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(seq) = seq else {
        return Ok(None);
    };
    conn.execute(
        "UPDATE messages SET kind = 'chat.undo', text = ?2, media = '', quote = '', raw = '{}'
         WHERE seq = ?1",
        params![seq, media::RECALLED_TEXT],
    )?;
    conn.query_row(
        &format!("SELECT {MESSAGE_COLUMNS} FROM messages WHERE seq = ?1"),
        params![seq],
        message_from_row,
    )
    .optional()
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

    /// Reset the unread counter. Returns false for an unknown thread.
    pub fn mark_read(&self, id: &str) -> Result<bool> {
        Ok(self
            .conn()
            .execute("UPDATE threads SET unread = 0 WHERE id = ?1", params![id])?
            > 0)
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
        recall_in(&self.conn(), thread_id, remote_id, client_id)
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
        Ok(out)
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
        assert_eq!(version, 3);
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
