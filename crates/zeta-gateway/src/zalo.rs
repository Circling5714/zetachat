//! Zalo connector: owns the single Zalo session, turns real-time frames into stored
//! messages, sends messages and runs the QR login on request.
//!
//! Message routing follows the Zalo client (as decoded by ctdozlite): `uidFrom = "0"`
//! is a message from this account; a 1-1 thread is the other party; group messages
//! arrive in `groupMsgs` (or carry `grid`) and belong to the group `idTo`; a message
//! with both ids `"0"` is a note to self.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, Semaphore};
use zeta_zalo::api::{LastOnline, Recipient, SessionHealth};
use zeta_zalo::message::QuotedMessage;
use zeta_zalo::socket::{self, RealtimeEvent};
use zeta_zalo::sticker::Sticker;
use zeta_zalo::upload::{self, FileLimits, FileSource, UploadedFile};
use zeta_zalo::{reaction, typing};
use zeta_zalo::{Device, SessionData, ZaloClient};

use crate::events::EventBus;
use crate::media;
use crate::media_cache::{MediaCache, MAX_FILE_BYTES};
use crate::model::{
    now_ms, thread_id, GroupMembers, Media, Member, Message, NewMessage, NewReaction, Person,
    Quote, ThreadKind, ThreadUpsert, PEOPLE_FRESH_MS,
};
use crate::stickers::{StickerCache, StickerQuery};
use crate::store::Store;
use crate::uploads::UploadJob;

/// How often the session is checked with Zalo (one light `me-v2` call).
const SESSION_CHECK_INTERVAL: Duration = Duration::from_secs(3600);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    #[default]
    Starting,
    /// No saved session: log in with a QR code.
    LoggedOut,
    Valid,
    /// Zalo refused the session: log in again.
    Rejected,
    /// No verdict yet (network problem, …).
    Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LoginState {
    #[default]
    Idle,
    /// Show `qr` as a QR code; it expires at `expires_at` (ms).
    WaitingScan {
        qr: String,
        expires_at: i64,
    },
    /// Scanned; waiting for the tap on "Đăng nhập".
    WaitingConfirm,
    Failed {
        reason: String,
    },
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ZaloStatus {
    pub session: SessionState,
    pub realtime: bool,
    pub display_name: String,
    /// This account's user id and avatar, once Zalo told them.
    pub uid: String,
    pub avatar: String,
    pub login: LoginState,
    /// Human-readable detail for `unknown` / `rejected`.
    pub detail: String,
}

/// Pictures Zalo can show as photos; everything else is sent as a file.
pub fn photo_type(mime: &str) -> bool {
    matches!(
        mime,
        "image/jpeg" | "image/png" | "image/webp" | "image/gif"
    )
}

/// A photo or file a device wants to send, waiting on disk.
#[derive(Clone, Debug)]
pub struct Attachment {
    pub name: String,
    pub mime: String,
    pub path: PathBuf,
    pub size: u64,
    /// MD5 of the content, hex.
    pub md5: String,
    /// Picture size, 0 when unknown.
    pub width: u32,
    pub height: u32,
    /// Shown under a photo; may be empty.
    pub caption: String,
    /// Sent as a photo rather than as a file.
    pub photo: bool,
}

pub enum ZaloCommand {
    SendText {
        kind: ThreadKind,
        remote_id: String,
        text: String,
        /// The message this one answers, if any.
        reply_to: Option<Box<ReplyTo>>,
        reply: oneshot::Sender<Result<Message, String>>,
    },
    /// Recall one of the account's messages for everyone; answers with the
    /// message as it is now (`None` when Zalo's echo already applied it).
    Recall {
        kind: ThreadKind,
        remote_id: String,
        message_id: String,
        message_client_id: String,
        reply: oneshot::Sender<Result<Option<Message>, String>>,
    },
    /// Send an attachment; the outcome goes to `job`, which also owns the file.
    SendAttachment {
        kind: ThreadKind,
        remote_id: String,
        attachment: Attachment,
        job: Arc<UploadJob>,
    },
    /// React with `icon` (empty: take back) to the message `message_id` /
    /// `message_client_id`; answers with the message and its reactions.
    React {
        kind: ThreadKind,
        remote_id: String,
        message_id: String,
        message_client_id: String,
        icon: String,
        reply: oneshot::Sender<Result<Option<Message>, String>>,
    },
    /// The account read these incoming messages (original Zalo JSON): send a
    /// read receipt, if enabled.
    MarkSeen {
        kind: ThreadKind,
        remote_id: String,
        messages: Vec<Value>,
    },
    /// The account is typing in this thread: tell Zalo, if enabled.
    Typing {
        kind: ThreadKind,
        remote_id: String,
    },
    /// Ask Zalo for the name and avatar of these users (at most 50) and store
    /// them; answers with what the store then knows of them.
    LookupPeople {
        uids: Vec<String>,
        reply: oneshot::Sender<Result<Vec<Person>, String>>,
    },
    /// Ask the sticker store: packs, a pack's stickers, a search (JSON for
    /// clients).
    Stickers {
        query: StickerQuery,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    /// Send a sticker; answers with the stored message.
    SendSticker {
        kind: ThreadKind,
        remote_id: String,
        sticker: Sticker,
        reply: oneshot::Sender<Result<Message, String>>,
    },
    /// Forward a text (or a link) to a conversation; answers with the stored
    /// message.
    Forward {
        kind: ThreadKind,
        remote_id: String,
        text: String,
        reply: oneshot::Sender<Result<Message, String>>,
    },
    /// Delete a message for this account only (on every device of it).
    DeleteForMe {
        kind: ThreadKind,
        remote_id: String,
        message_id: String,
        message_client_id: String,
        /// Who sent it: this account's uid for its own messages.
        owner_id: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// When a user was last on Zalo (asked at most once a minute per user).
    LastOnline {
        uid: String,
        reply: oneshot::Sender<Result<LastOnline, String>>,
    },
    /// Who is in a group, asked of Zalo now.
    GroupMembers {
        group_id: String,
        reply: oneshot::Sender<Result<GroupMembers, String>>,
    },
    /// Find people, ask them to be friends, answer their requests (JSON for
    /// clients).
    Friends {
        action: FriendAction,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    StartLogin {
        reply: oneshot::Sender<LoginState>,
    },
}

/// What to do on the friends side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FriendAction {
    /// The account behind a phone number: `{ "user": {…} | null }`.
    Find(String),
    /// Requests received and not answered yet: `[{…}]`.
    Requests,
    Send {
        uid: String,
        message: String,
    },
    Accept(String),
    Reject(String),
}

/// A stored message a new one replies to, with its original Zalo JSON (`Null`
/// for messages sent from here).
#[derive(Clone, Debug)]
pub struct ReplyTo {
    pub message: Message,
    pub raw: Value,
}

/// What Zalo needs to know of a quoted message.
fn quoted_message(r: &ReplyTo, self_uid: &str) -> QuotedMessage {
    let m = &r.message;
    let content = match r.raw.get("content") {
        Some(content) if !content.is_null() => content.clone(),
        // Sent from here: no original JSON, rebuild what the official client reads.
        _ => match &m.media {
            Some(media) => json!({
                "href": media.url,
                "thumb": if media.thumb.is_empty() { &media.url } else { &media.thumb },
                "title": if media.name.is_empty() { &media.description } else { &media.name },
            }),
            None => json!(m.text),
        },
    };
    QuotedMessage {
        owner_id: if m.outgoing {
            self_uid.to_string()
        } else {
            m.sender_id.clone()
        },
        msg_id: m.remote_id.clone(),
        cli_msg_id: if m.client_id.is_empty() {
            "0".into()
        } else {
            m.client_id.clone()
        },
        msg_type: m.kind.clone(),
        ts: m.sent_at,
        content,
        ttl: r.raw.get("ttl").and_then(Value::as_i64).unwrap_or(0),
    }
}

/// How long to wait for Zalo to announce an uploaded file on the socket: a
/// minute and a half, plus a second per 8 MB.
fn file_done_patience(size: u64) -> Duration {
    Duration::from_secs(90 + size / (8 * 1024 * 1024))
}

/// Times the last part is sent again while Zalo's announcement is late, as
/// Zalo PC does (`retry_upload_file_configs`).
const LAST_PART_RESENDS: u32 = 2;

/// Files Zalo finished storing, announced on the socket (`file_done`); senders
/// wait here for the URL of their upload.
#[derive(Default)]
struct UploadBoard {
    done: std::sync::Mutex<std::collections::HashMap<String, String>>,
    changed: tokio::sync::Notify,
}

impl UploadBoard {
    fn complete(&self, file_id: String, url: String) {
        {
            let mut done = self.done.lock().unwrap_or_else(|e| e.into_inner());
            // Announcements nobody waits for (uploads from other devices) pile up.
            if done.len() >= 64 {
                done.clear();
            }
            done.insert(file_id, url);
        }
        self.changed.notify_waiters();
    }

    /// The first of `file_ids` announced within `timeout`, with its URL.
    async fn wait_any(&self, file_ids: &[String], timeout: Duration) -> Option<(String, String)> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            // Registered before looking, so an announcement in between still wakes us.
            let changed = self.changed.notified();
            let found = {
                let mut done = self.done.lock().unwrap_or_else(|e| e.into_inner());
                file_ids
                    .iter()
                    .find_map(|id| done.remove(id).map(|url| (id.clone(), url)))
            };
            if found.is_some() {
                return found;
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                return None;
            }
        }
    }
}

/// Handle used by the API to talk to the connector.
#[derive(Clone)]
pub struct ZaloHandle {
    pub commands: mpsc::Sender<ZaloCommand>,
    pub status: Arc<RwLock<ZaloStatus>>,
    /// Upload limits; the server's once a session is up.
    pub limits: Arc<RwLock<FileLimits>>,
}

impl ZaloHandle {
    /// A handle around `commands`, with a fresh status and default limits.
    pub fn new(commands: mpsc::Sender<ZaloCommand>) -> Self {
        Self {
            commands,
            status: Arc::default(),
            limits: Arc::default(),
        }
    }

    pub fn status(&self) -> ZaloStatus {
        self.status
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn limits(&self) -> FileLimits {
        self.limits
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

pub struct ZaloConfig {
    pub session_path: PathBuf,
    /// Shown as "Ứng dụng" on the phone when logging in.
    pub computer_name: String,
    /// Where media of new messages is downloaded ahead of time, if anywhere.
    pub media: Option<Arc<MediaCache>>,
    /// Tell Zalo when messages are read here (senders see "Đã xem", the phone
    /// clears its unread marks).
    pub read_receipts: bool,
    /// Tell Zalo when the account is typing here (the other side sees "đang
    /// soạn tin…").
    pub typing: bool,
}

/// How long a user's last-seen time is kept before asking Zalo again.
const PRESENCE_FRESH: Duration = Duration::from_secs(60);

/// Last-seen times asked lately, per user.
type PresenceCache =
    Arc<std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, LastOnline)>>>;

/// Typing is told to Zalo at most this often per thread; Zalo shows it for a
/// few seconds each time.
const TYPING_EVERY: Duration = Duration::from_secs(4);

/// Start the connector task.
pub fn spawn(cfg: ZaloConfig, store: Arc<Store>, bus: EventBus) -> ZaloHandle {
    let (commands, rx) = mpsc::channel(32);
    let handle = ZaloHandle::new(commands);
    let connector = Connector {
        cfg,
        store,
        bus,
        status: handle.status.clone(),
        limits: handle.limits.clone(),
        client: None,
        self_uid: String::new(),
        login_running: false,
        uploads: Arc::new(UploadBoard::default()),
        // One attachment at a time, in the order they came.
        attachment_turns: Arc::new(Semaphore::new(1)),
        typing_sent: std::collections::HashMap::new(),
        stickers: Arc::default(),
        presence: Arc::default(),
    };
    tokio::spawn(connector.run(rx));
    handle
}

// ---------------------------------------------------------------------------
// Frame parsing

/// A chat message extracted from a real-time frame.
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedMessage {
    pub kind: ThreadKind,
    /// Remote id of the thread: the other user, the group, or self for notes.
    pub peer: String,
    pub remote_id: String,
    pub client_id: String,
    pub sender_id: String,
    pub sender_name: String,
    pub outgoing: bool,
    pub msg_type: String,
    pub text: String,
    pub media: Option<Media>,
    pub quote: Option<Quote>,
    pub sent_at: i64,
    pub raw: Value,
}

fn field(v: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|k| match v.get(*k) {
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            Some(Value::Number(n)) => Some(n.to_string()),
            _ => None,
        })
        .unwrap_or_default()
}

fn is_zero(id: &str) -> bool {
    id.is_empty() || id == "0"
}

/// Thread of something sent by `uid_from` to `id_to` (a message, a reaction):
/// kind, remote id of the thread, and whether this account sent it.
fn route(
    uid_from: &str,
    id_to: &str,
    grid: &str,
    group_bucket: bool,
    self_uid: &str,
) -> Option<(ThreadKind, String, bool)> {
    let outgoing = is_zero(uid_from) || uid_from == self_uid;
    let (kind, peer) = if group_bucket || !is_zero(grid) {
        let group = if is_zero(grid) { id_to } else { grid };
        (ThreadKind::Group, group.trim_start_matches("group_"))
    } else if is_zero(id_to) && is_zero(uid_from) {
        (ThreadKind::User, self_uid)
    } else if !outgoing && (is_zero(id_to) || id_to == self_uid) {
        (ThreadKind::User, uid_from)
    } else if !is_zero(id_to) {
        (ThreadKind::User, id_to)
    } else {
        (ThreadKind::User, uid_from)
    };
    (!is_zero(peer)).then(|| (kind, peer.to_string(), outgoing))
}

fn parse_message(it: &Value, group_bucket: bool, self_uid: &str) -> Option<ParsedMessage> {
    let remote_id = field(it, &["msgId", "globalMsgId", "cliMsgId"]);
    if remote_id.is_empty() {
        return None;
    }
    let uid_from = field(it, &["uidFrom"]);
    let id_to = field(it, &["idTo"]);
    let grid = field(it, &["grid", "groupId"]);
    let (kind, peer, outgoing) = route(&uid_from, &id_to, &grid, group_bucket, self_uid)?;

    let msg_type = field(it, &["msgType"]);
    Some(ParsedMessage {
        kind,
        peer,
        client_id: field(it, &["cliMsgId"]),
        sender_id: if outgoing {
            self_uid.to_string()
        } else {
            uid_from
        },
        sender_name: field(it, &["dName"]),
        outgoing,
        text: media::summarize(&msg_type, it.get("content")),
        media: media::extract(&msg_type, it.get("content")),
        quote: media::extract_quote(it),
        msg_type,
        sent_at: field(it, &["ts"]).parse().unwrap_or(0),
        raw: it.clone(),
        remote_id,
    })
}

/// All chat messages in a real-time frame payload (`msgs`, `groupMsgs`, `pageMsgs`).
pub fn parse_frame_messages(payload: &Value, self_uid: &str) -> Vec<ParsedMessage> {
    let data = payload
        .get("data")
        .filter(|d| d.is_object())
        .unwrap_or(payload);
    let mut out = Vec::new();
    for (key, group_bucket) in [("msgs", false), ("groupMsgs", true), ("pageMsgs", false)] {
        let Some(items) = data.get(key).and_then(Value::as_array) else {
            continue;
        };
        out.extend(
            items
                .iter()
                .filter_map(|it| parse_message(it, group_bucket, self_uid)),
        );
    }
    out
}

// ---------------------------------------------------------------------------
// Connector

enum LoginOutcome {
    Success(Box<ZaloClient>),
    Failed,
}

struct Connector {
    cfg: ZaloConfig,
    store: Arc<Store>,
    bus: EventBus,
    status: Arc<RwLock<ZaloStatus>>,
    limits: Arc<RwLock<FileLimits>>,
    client: Option<Arc<ZaloClient>>,
    self_uid: String,
    login_running: bool,
    uploads: Arc<UploadBoard>,
    attachment_turns: Arc<Semaphore>,
    /// When typing was last told to Zalo, per thread.
    typing_sent: std::collections::HashMap<String, std::time::Instant>,
    stickers: Arc<StickerCache>,
    presence: PresenceCache,
}

async fn next_realtime(
    rx: &mut Option<mpsc::UnboundedReceiver<RealtimeEvent>>,
) -> Option<RealtimeEvent> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

fn update_status(status: &RwLock<ZaloStatus>, f: impl FnOnce(&mut ZaloStatus)) {
    f(&mut status.write().unwrap_or_else(|e| e.into_inner()));
}

fn publish_status(status: &RwLock<ZaloStatus>, bus: &EventBus) {
    let snapshot = status.read().unwrap_or_else(|e| e.into_inner()).clone();
    if let Err(e) = bus.publish("status", json!(snapshot)) {
        tracing::error!(error = %e, "cannot publish status");
    }
}

/// Store a reaction; announce each stored message it changed. Returns the
/// first of them.
fn apply_reaction(store: &Store, bus: &EventBus, reaction: &NewReaction) -> Option<Message> {
    let changed = match store.add_reaction(reaction) {
        Ok(changed) => changed,
        Err(e) => {
            tracing::error!(error = %e, "cannot store a reaction");
            return None;
        }
    };
    for message in &changed {
        let thread = store.thread(&message.thread_id).ok().flatten();
        let payload = json!({ "message": message, "thread": thread });
        if let Err(e) = bus.publish("message_updated", payload) {
            tracing::error!(error = %e, "cannot publish a reaction");
        }
    }
    changed.into_iter().next()
}

fn publish_message(store: &Store, bus: &EventBus, message: &Message) {
    let thread = store.thread(&message.thread_id).ok().flatten();
    if let Err(e) = bus.publish("message", json!({ "message": message, "thread": thread })) {
        tracing::error!(error = %e, "cannot publish message");
    }
}

impl Connector {
    async fn run(mut self, mut commands: mpsc::Receiver<ZaloCommand>) {
        let (login_tx, mut login_rx) = mpsc::channel::<LoginOutcome>(1);
        let mut realtime = self.resume().await;
        let mut check = tokio::time::interval(SESSION_CHECK_INTERVAL);
        check.tick().await;
        loop {
            tokio::select! {
                cmd = commands.recv() => match cmd {
                    Some(cmd) => self.handle_command(cmd, &login_tx),
                    None => break,
                },
                Some(outcome) = login_rx.recv() => {
                    if let Some(rx) = self.finish_login(outcome).await {
                        realtime = Some(rx);
                    }
                }
                event = next_realtime(&mut realtime) => match event {
                    Some(event) => {
                        if !self.handle_realtime(event) {
                            realtime = None;
                        }
                    }
                    None => {
                        realtime = None;
                        update_status(&self.status, |s| s.realtime = false);
                        publish_status(&self.status, &self.bus);
                    }
                },
                _ = check.tick() => {
                    if !self.periodic_check().await {
                        realtime = None;
                    }
                }
            }
        }
    }

    fn set_session(&self, state: SessionState, detail: String) {
        update_status(&self.status, |s| {
            s.session = state;
            s.detail = detail;
        });
        publish_status(&self.status, &self.bus);
    }

    /// Load the saved session, if any, and bring it up.
    async fn resume(&mut self) -> Option<mpsc::UnboundedReceiver<RealtimeEvent>> {
        match SessionData::load(&self.cfg.session_path) {
            Ok(session) => match ZaloClient::from_session(session) {
                Ok(client) => self.activate(client).await,
                Err(e) => {
                    self.set_session(
                        SessionState::Unknown,
                        format!("phiên đã lưu không dùng được: {e}"),
                    );
                    None
                }
            },
            Err(zeta_zalo::Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                self.set_session(SessionState::LoggedOut, String::new());
                None
            }
            Err(e) => {
                self.set_session(SessionState::Unknown, format!("không đọc được phiên: {e}"));
                None
            }
        }
    }

    /// Verify a client with Zalo, sync contacts and start the real-time loop.
    async fn activate(
        &mut self,
        mut client: ZaloClient,
    ) -> Option<mpsc::UnboundedReceiver<RealtimeEvent>> {
        match client.refresh_file_limits().await {
            Ok(()) => {
                let limits = client.file_limits().clone();
                tracing::info!(
                    max_mb = limits.max_file_bytes / (1024 * 1024),
                    part_kb = limits.chunk_bytes / 1024,
                    "upload limits loaded"
                );
                *self.limits.write().unwrap_or_else(|e| e.into_inner()) = limits;
            }
            Err(e) => tracing::warn!(error = %e, "cannot load upload limits, using defaults"),
        }
        let health = client.check_session().await;
        match &health {
            SessionHealth::Rejected => {
                self.client = None;
                self.set_session(SessionState::Rejected, "Zalo từ chối phiên đã lưu".into());
                return None;
            }
            SessionHealth::Valid(profile) => self.note_profile(profile),
            SessionHealth::Unknown(reason) => update_status(&self.status, |s| {
                s.session = SessionState::Unknown;
                s.detail = reason.clone();
            }),
        }
        self.self_uid = client.session().map(|s| s.uid.clone()).unwrap_or_default();
        if matches!(health, SessionHealth::Valid(_)) {
            self.sync_contacts(&client).await;
            self.load_recent(&client).await;
        }
        let realtime = socket::RealtimeConfig::from_client(&client)
            .ok()
            .map(socket::spawn);
        self.client = Some(Arc::new(client));
        publish_status(&self.status, &self.bus);
        realtime
    }

    /// The account's own profile, as `me-v2` gave it: status and people.
    fn note_profile(&self, profile: &zeta_zalo::api::Profile) {
        update_status(&self.status, |s| {
            s.session = SessionState::Valid;
            s.display_name = profile.display_name.clone();
            s.uid = profile.uid.clone();
            if !profile.avatar.is_empty() {
                s.avatar = profile.avatar.clone();
            }
            s.detail.clear();
        });
        let me = Person {
            uid: profile.uid.clone(),
            name: profile.display_name.clone(),
            avatar: profile.avatar.clone(),
            ..Person::default()
        };
        if let Err(e) = self.store.upsert_person(&me, None) {
            tracing::error!(error = %e, "cannot store own profile");
        }
    }

    /// Friends become threads, so incoming messages have a name, and people, so
    /// they can be listed. Zalo rate-limits this call, so it only runs when a
    /// session comes up.
    async fn sync_contacts(&self, client: &ZaloClient) {
        match client.friends().await {
            Ok(friends) => {
                let count = friends.len();
                let mut people = Vec::with_capacity(count);
                for f in friends {
                    people.push(Person {
                        uid: f.uid.clone(),
                        name: f.display_name.clone(),
                        zalo_name: f.zalo_name,
                        avatar: f.avatar.clone(),
                        ..Person::default()
                    });
                    let thread = ThreadUpsert {
                        kind: ThreadKind::User,
                        remote_id: f.uid,
                        name: f.display_name,
                        avatar: f.avatar,
                    };
                    if let Err(e) = self.store.upsert_thread(&thread, true) {
                        tracing::error!(error = %e, "cannot store contact");
                    }
                }
                // An empty answer is more likely a hiccup than a lost friend list.
                if !people.is_empty() {
                    if let Err(e) = self.store.set_friends(&people) {
                        tracing::error!(error = %e, "cannot store the friend list");
                    }
                }
                tracing::info!(count, "contacts synced");
            }
            Err(e) => tracing::warn!(error = %e, "cannot load friends"),
        }
        match client.groups().await {
            Ok(groups) => {
                let count = groups.len();
                for g in groups {
                    let thread = ThreadUpsert {
                        kind: ThreadKind::Group,
                        remote_id: g.id,
                        name: g.name,
                        avatar: g.avatar,
                    };
                    let stored = self
                        .store
                        .upsert_thread(&thread, true)
                        .and_then(|()| self.store.set_members(&thread.id(), g.member_count));
                    if let Err(e) = stored {
                        tracing::error!(error = %e, "cannot store group");
                    }
                }
                tracing::info!(count, "groups synced");
            }
            Err(e) => tracing::warn!(error = %e, "cannot load groups"),
        }
    }

    fn handle_command(&mut self, cmd: ZaloCommand, login_tx: &mpsc::Sender<LoginOutcome>) {
        match cmd {
            ZaloCommand::SendText {
                kind,
                remote_id,
                text,
                reply_to,
                reply,
            } => {
                let Some(client) = self.client.clone() else {
                    let _ = reply.send(Err("Zalo chưa đăng nhập".into()));
                    return;
                };
                let store = self.store.clone();
                let bus = self.bus.clone();
                let me = Me {
                    uid: self.self_uid.clone(),
                    name: self
                        .status
                        .read()
                        .unwrap_or_else(|e| e.into_inner())
                        .display_name
                        .clone(),
                };
                tokio::spawn(async move {
                    let target = (kind, remote_id.as_str());
                    let result = send_and_store(
                        &client,
                        &store,
                        &bus,
                        &me,
                        target,
                        &text,
                        reply_to.as_deref(),
                    )
                    .await;
                    let _ = reply.send(result);
                });
            }
            ZaloCommand::Recall {
                kind,
                remote_id,
                message_id,
                message_client_id,
                reply,
            } => {
                let Some(client) = self.client.clone() else {
                    let _ = reply.send(Err("Zalo chưa đăng nhập".into()));
                    return;
                };
                let (store, bus) = (self.store.clone(), self.bus.clone());
                tokio::spawn(async move {
                    let to = recipient(kind, &remote_id);
                    let result = match client.recall(to, &message_id, &message_client_id).await {
                        Ok(()) => {
                            let thread = thread_id(kind, &remote_id);
                            match store.recall_message(&thread, &message_id, &message_client_id) {
                                Ok(Some(message)) => {
                                    let snapshot = store.thread(&thread).ok().flatten();
                                    let payload = json!({ "message": message, "thread": snapshot });
                                    if let Err(e) = bus.publish("message_updated", payload) {
                                        tracing::error!(error = %e, "cannot publish a recall");
                                    }
                                    Ok(Some(message))
                                }
                                Ok(None) => Ok(None),
                                Err(e) => Err(e.to_string()),
                            }
                        }
                        Err(e) => Err(e.to_string()),
                    };
                    let _ = reply.send(result);
                });
            }
            ZaloCommand::SendAttachment {
                kind,
                remote_id,
                attachment,
                job,
            } => {
                let Some(client) = self.client.clone() else {
                    job.finish(Err("Zalo chưa đăng nhập".into()));
                    tokio::spawn(async move { job.remove_file().await });
                    return;
                };
                let sender = AttachmentSender {
                    client,
                    store: self.store.clone(),
                    bus: self.bus.clone(),
                    media: self.cfg.media.clone(),
                    uploads: self.uploads.clone(),
                    self_uid: self.self_uid.clone(),
                };
                let turns = self.attachment_turns.clone();
                tokio::spawn(async move {
                    run_upload(&sender, &turns, kind, &remote_id, &attachment, &job).await;
                    job.remove_file().await;
                });
            }
            ZaloCommand::React {
                kind,
                remote_id,
                message_id,
                message_client_id,
                icon,
                reply,
            } => {
                let Some(client) = self.client.clone() else {
                    let _ = reply.send(Err("Zalo chưa đăng nhập".into()));
                    return;
                };
                let (store, bus) = (self.store.clone(), self.bus.clone());
                let self_uid = self.self_uid.clone();
                tokio::spawn(async move {
                    let to = recipient(kind, &remote_id);
                    let result = match client
                        .send_reaction(to, &message_id, &message_client_id, &icon)
                        .await
                    {
                        Ok(sent) => {
                            let reaction = NewReaction {
                                thread_id: thread_id(kind, &remote_id),
                                action_id: sent
                                    .msg_ids
                                    .first()
                                    .cloned()
                                    .unwrap_or_else(|| format!("c{}", sent.client_id)),
                                client_id: sent.client_id.to_string(),
                                targets: vec![(message_id, message_client_id)],
                                user_id: self_uid,
                                mine: true,
                                icon,
                                sent_at: now_ms(),
                            };
                            Ok(apply_reaction(&store, &bus, &reaction))
                        }
                        Err(e) => Err(e.to_string()),
                    };
                    let _ = reply.send(result);
                });
            }
            ZaloCommand::MarkSeen {
                kind,
                remote_id,
                messages,
            } => {
                let Some(client) = self.client.clone().filter(|_| self.cfg.read_receipts) else {
                    return;
                };
                tokio::spawn(async move {
                    let count = messages.len();
                    match client
                        .send_seen(recipient(kind, &remote_id), &messages)
                        .await
                    {
                        Ok(()) => tracing::debug!(count, "read receipt sent"),
                        Err(e) => tracing::warn!(error = %e, "read receipt not sent"),
                    }
                });
            }
            ZaloCommand::Typing { kind, remote_id } => {
                let Some(client) = self.client.clone().filter(|_| self.cfg.typing) else {
                    return;
                };
                let thread = thread_id(kind, &remote_id);
                let now = std::time::Instant::now();
                if self
                    .typing_sent
                    .get(&thread)
                    .is_some_and(|at| now.duration_since(*at) < TYPING_EVERY)
                {
                    return;
                }
                self.typing_sent
                    .retain(|_, at| now.duration_since(*at) < TYPING_EVERY);
                self.typing_sent.insert(thread, now);
                tokio::spawn(async move {
                    if let Err(e) = client.send_typing(recipient(kind, &remote_id)).await {
                        tracing::debug!(error = %e, "typing not sent");
                    }
                });
            }
            ZaloCommand::LookupPeople { uids, reply } => {
                let Some(client) = self.client.clone() else {
                    let _ = reply.send(Err("Zalo chưa đăng nhập".into()));
                    return;
                };
                let store = self.store.clone();
                tokio::spawn(async move {
                    let _ = reply.send(lookup_people(&client, &store, &uids).await);
                });
            }
            ZaloCommand::Forward {
                kind,
                remote_id,
                text,
                reply,
            } => {
                let Some(client) = self.client.clone() else {
                    let _ = reply.send(Err("Zalo chưa đăng nhập".into()));
                    return;
                };
                let (store, bus) = (self.store.clone(), self.bus.clone());
                let self_uid = self.self_uid.clone();
                tokio::spawn(async move {
                    let info = json!({ "message": text });
                    let result = match client.forward(recipient(kind, &remote_id), 1, &info).await {
                        Ok(sent) => {
                            let sent = Sent {
                                msg_id: sent.msg_id,
                                client_id: sent.client_id,
                                msg_type: "webchat",
                                text,
                                media: None,
                                quote: None,
                                raw: None,
                            };
                            store_sent(&store, &bus, &self_uid, kind, &remote_id, sent)
                        }
                        Err(e) => Err(e.to_string()),
                    };
                    let _ = reply.send(result);
                });
            }
            ZaloCommand::DeleteForMe {
                kind,
                remote_id,
                message_id,
                message_client_id,
                owner_id,
                reply,
            } => {
                let Some(client) = self.client.clone() else {
                    let _ = reply.send(Err("Zalo chưa đăng nhập".into()));
                    return;
                };
                let owner = if owner_id.is_empty() {
                    self.self_uid.clone()
                } else {
                    owner_id
                };
                tokio::spawn(async move {
                    let result = client
                        .delete_for_me(
                            recipient(kind, &remote_id),
                            &message_id,
                            &message_client_id,
                            &owner,
                        )
                        .await
                        .map_err(|e| e.to_string());
                    let _ = reply.send(result);
                });
            }
            ZaloCommand::LastOnline { uid, reply } => {
                let Some(client) = self.client.clone() else {
                    let _ = reply.send(Err("Zalo chưa đăng nhập".into()));
                    return;
                };
                let presence = self.presence.clone();
                tokio::spawn(async move {
                    let kept = presence
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(&uid)
                        .filter(|(at, _)| at.elapsed() < PRESENCE_FRESH)
                        .map(|(_, seen)| *seen);
                    let result = match kept {
                        Some(seen) => Ok(seen),
                        None => match client.last_online(&uid).await {
                            Ok(seen) => {
                                let mut map = presence.lock().unwrap_or_else(|e| e.into_inner());
                                if map.len() > 500 {
                                    map.clear();
                                }
                                map.insert(uid, (std::time::Instant::now(), seen));
                                Ok(seen)
                            }
                            Err(e) => Err(e.to_string()),
                        },
                    };
                    let _ = reply.send(result);
                });
            }
            ZaloCommand::GroupMembers { group_id, reply } => {
                let Some(client) = self.client.clone() else {
                    let _ = reply.send(Err("Zalo chưa đăng nhập".into()));
                    return;
                };
                let store = self.store.clone();
                tokio::spawn(async move {
                    let _ = reply.send(group_members(&client, &store, &group_id).await);
                });
            }
            ZaloCommand::Friends { action, reply } => {
                let Some(client) = self.client.clone() else {
                    let _ = reply.send(Err("Zalo chưa đăng nhập".into()));
                    return;
                };
                let (store, bus) = (self.store.clone(), self.bus.clone());
                tokio::spawn(async move {
                    let _ = reply.send(friend_action(&client, &store, &bus, action).await);
                });
            }
            ZaloCommand::Stickers { query, reply } => {
                let Some(client) = self.client.clone() else {
                    let _ = reply.send(Err("Zalo chưa đăng nhập".into()));
                    return;
                };
                let cache = self.stickers.clone();
                tokio::spawn(async move {
                    let _ = reply.send(cache.answer(&client, &query).await);
                });
            }
            ZaloCommand::SendSticker {
                kind,
                remote_id,
                sticker,
                reply,
            } => {
                let Some(client) = self.client.clone() else {
                    let _ = reply.send(Err("Zalo chưa đăng nhập".into()));
                    return;
                };
                let (store, bus) = (self.store.clone(), self.bus.clone());
                let media_cache = self.cfg.media.clone();
                let self_uid = self.self_uid.clone();
                tokio::spawn(async move {
                    let result = match client
                        .send_sticker(recipient(kind, &remote_id), &sticker)
                        .await
                    {
                        Ok(sent) => {
                            let found = sticker_media(&sticker);
                            if let (Some(cache), Some(found)) = (&media_cache, &found) {
                                cache.prefetch(found.prefetch_urls());
                            }
                            let sent = Sent {
                                msg_id: sent.msg_id,
                                client_id: sent.client_id,
                                msg_type: "chat.sticker",
                                text: "[Nhãn dán]".into(),
                                media: found,
                                quote: None,
                                raw: Some(json!({
                                    "msgType": "chat.sticker",
                                    "content": {
                                        "id": sticker.id,
                                        "catId": sticker.cate_id,
                                        "type": sticker.kind,
                                    },
                                })),
                            };
                            store_sent(&store, &bus, &self_uid, kind, &remote_id, sent)
                        }
                        Err(e) => Err(e.to_string()),
                    };
                    let _ = reply.send(result);
                });
            }
            ZaloCommand::StartLogin { reply } => {
                if self.login_running {
                    let current = self
                        .status
                        .read()
                        .unwrap_or_else(|e| e.into_inner())
                        .login
                        .clone();
                    let _ = reply.send(current);
                    return;
                }
                self.login_running = true;
                // Keep the device identity of the previous session: same IMEI, same computer.
                let device = SessionData::load(&self.cfg.session_path)
                    .map(|s| s.device)
                    .unwrap_or_else(|_| Device::generate(self.cfg.computer_name.clone()));
                tokio::spawn(login_task(
                    device,
                    self.cfg.session_path.clone(),
                    self.status.clone(),
                    self.bus.clone(),
                    reply,
                    login_tx.clone(),
                ));
            }
        }
    }

    async fn finish_login(
        &mut self,
        outcome: LoginOutcome,
    ) -> Option<mpsc::UnboundedReceiver<RealtimeEvent>> {
        self.login_running = false;
        match outcome {
            LoginOutcome::Success(client) => {
                update_status(&self.status, |s| s.login = LoginState::Idle);
                self.activate(*client).await
            }
            LoginOutcome::Failed => None,
        }
    }

    /// Returns false when the real-time loop has to stop.
    fn handle_realtime(&mut self, event: RealtimeEvent) -> bool {
        match event {
            RealtimeEvent::Connected { endpoint } => {
                tracing::info!(%endpoint, "real-time connected");
                update_status(&self.status, |s| s.realtime = true);
                publish_status(&self.status, &self.bus);
                true
            }
            RealtimeEvent::Disconnected { reason } => {
                tracing::info!(%reason, "real-time connection lost, reconnecting");
                update_status(&self.status, |s| s.realtime = false);
                publish_status(&self.status, &self.bus);
                true
            }
            RealtimeEvent::SessionRejected => {
                self.mark_rejected("Zalo từ chối phiên khi kết nối thời gian thực");
                false
            }
            RealtimeEvent::Frame(frame) if frame.is_message() => {
                let messages = parse_frame_messages(&frame.payload, &self.self_uid);
                tracing::debug!(
                    cmd = frame.cmd,
                    sub = frame.sub_cmd,
                    messages = messages.len(),
                    "message frame"
                );
                for message in messages {
                    self.store_incoming(message);
                }
                true
            }
            RealtimeEvent::Frame(frame) => {
                // Keep-alive answers are not worth a line every 15 seconds.
                if frame.cmd != socket::cmd::PING {
                    let shape = socket::payload_shape(&frame.payload);
                    tracing::debug!(cmd = frame.cmd, sub = frame.sub_cmd, %shape, "frame");
                }
                if frame.is_control() {
                    for (file_id, url) in upload::file_done_events(&frame.payload) {
                        tracing::debug!(%file_id, "file stored by Zalo");
                        self.uploads.complete(file_id, url);
                    }
                }
                // 612 pushes reactions; 610 and 611 bring the ones missed.
                if (610..=612).contains(&frame.cmd) {
                    self.store_reactions(&frame.payload);
                }
                if frame.cmd == socket::cmd::CTRL_ACTION {
                    self.announce_typing(&frame.payload);
                }
                true
            }
        }
    }

    /// Others typing: a transient `typing` event (not logged, nothing stored).
    fn announce_typing(&self, payload: &Value) {
        for t in typing::typing_events(payload) {
            if t.uid == self.self_uid {
                continue;
            }
            let thread = match &t.group {
                Some(gid) => thread_id(ThreadKind::Group, gid.trim_start_matches("group_")),
                None => thread_id(ThreadKind::User, &t.uid),
            };
            let name = if t.group.is_some() {
                self.store.sender_name(&thread, &t.uid).unwrap_or_default()
            } else {
                self.store
                    .thread(&thread)
                    .ok()
                    .flatten()
                    .map(|th| th.name)
                    .unwrap_or_default()
            };
            tracing::debug!(group = t.group.is_some(), "someone is typing");
            self.bus.notify(
                "typing",
                json!({ "thread_id": thread, "user_id": t.uid, "name": name }),
            );
        }
    }

    fn store_reactions(&self, payload: &Value) {
        let found = reaction::frame_reactions(payload);
        if found.is_empty() {
            return;
        }
        let total = found.len();
        let mut applied = 0;
        for r in found {
            let Some((kind, peer, mine)) =
                route(&r.uid_from, &r.id_to, "", r.group, &self.self_uid)
            else {
                continue;
            };
            let new = NewReaction {
                thread_id: thread_id(kind, &peer),
                action_id: r.action_id,
                client_id: r.client_id,
                targets: r.targets,
                user_id: if mine {
                    self.self_uid.clone()
                } else {
                    r.uid_from
                },
                mine,
                icon: r.icon,
                sent_at: if r.ts > 0 { r.ts } else { now_ms() },
            };
            if apply_reaction(&self.store, &self.bus, &new).is_some() {
                applied += 1;
            }
        }
        tracing::debug!(total, applied, "reactions");
    }

    /// Recent messages (`get-last-msgs`), so the conversation list is not empty
    /// after a fresh start. Stored quietly: no unread counter, no notification;
    /// clients just reload their lists.
    async fn load_recent(&self, client: &ZaloClient) {
        match client.recent_messages().await {
            Ok(data) => {
                let mut parsed = parse_frame_messages(&data, &self.self_uid);
                // Oldest first, so a recall comes after the message it recalls.
                parsed.sort_by_key(|m| m.sent_at);
                let stored = parsed
                    .into_iter()
                    .filter(|m| self.store_parsed(m.clone(), false).is_some())
                    .count();
                tracing::info!(stored, "recent messages loaded");
                if stored > 0 {
                    if let Err(e) = self.bus.publish("threads_changed", json!({})) {
                        tracing::error!(error = %e, "cannot publish threads_changed");
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "cannot load recent messages"),
        }
    }

    fn store_incoming(&self, m: ParsedMessage) {
        if let Some(message) = self.store_parsed(m, true) {
            publish_message(&self.store, &self.bus, &message);
        }
    }

    /// A recall changes the recalled message instead of adding a message.
    fn apply_recall(&self, m: &ParsedMessage) {
        let Some((remote_id, client_id)) = media::recalled_ids(m.raw.get("content")) else {
            return;
        };
        let thread = thread_id(m.kind, &m.peer);
        match self.store.recall_message(&thread, &remote_id, &client_id) {
            Ok(Some(message)) => {
                let snapshot = self.store.thread(&thread).ok().flatten();
                let payload = json!({ "message": message, "thread": snapshot });
                if let Err(e) = self.bus.publish("message_updated", payload) {
                    tracing::error!(error = %e, "cannot publish a recall");
                }
            }
            Ok(None) => tracing::debug!("recall of a message that is not stored"),
            Err(e) => tracing::error!(error = %e, "cannot apply a recall"),
        }
    }

    /// Store a parsed message; returns it when it was new. `live` messages count as
    /// unread when incoming.
    fn store_parsed(&self, m: ParsedMessage, live: bool) -> Option<Message> {
        if m.msg_type == "chat.undo" {
            self.apply_recall(&m);
            return None;
        }
        // A 1-1 sender's display name fills in an unnamed thread; contacts win.
        let name = if m.kind == ThreadKind::User && !m.outgoing {
            m.sender_name.clone()
        } else {
            String::new()
        };
        let thread = ThreadUpsert {
            kind: m.kind,
            remote_id: m.peer.clone(),
            name,
            avatar: String::new(),
        };
        let new = NewMessage {
            thread_id: thread.id(),
            remote_id: m.remote_id,
            client_id: m.client_id,
            sender_id: m.sender_id,
            sender_name: m.sender_name,
            outgoing: m.outgoing,
            kind: m.msg_type,
            text: m.text,
            media: m.media,
            quote: m.quote,
            sent_at: m.sent_at,
            raw: m.raw.to_string(),
        };
        match self
            .store
            .upsert_thread(&thread, false)
            .and_then(|_| self.store.insert_message_with(&new, live))
        {
            Ok(message) => {
                let found = message.as_ref().and_then(|m| m.media.as_ref());
                if let (Some(cache), Some(found)) = (&self.cfg.media, found) {
                    cache.prefetch(found.prefetch_urls());
                }
                message
            }
            Err(e) => {
                tracing::error!(error = %e, "cannot store message");
                None
            }
        }
    }

    fn mark_rejected(&mut self, detail: &str) {
        self.client = None;
        update_status(&self.status, |s| s.realtime = false);
        self.set_session(SessionState::Rejected, detail.to_string());
    }

    /// Returns false when Zalo rejected the session.
    async fn periodic_check(&mut self) -> bool {
        let Some(client) = self.client.clone() else {
            return true;
        };
        let before = self
            .status
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .session;
        match client.check_session().await {
            SessionHealth::Valid(profile) => {
                self.note_profile(&profile);
                if before != SessionState::Valid {
                    publish_status(&self.status, &self.bus);
                }
                true
            }
            SessionHealth::Rejected => {
                self.mark_rejected("Zalo từ chối phiên");
                false
            }
            SessionHealth::Unknown(reason) => {
                update_status(&self.status, |s| {
                    s.session = SessionState::Unknown;
                    s.detail = reason;
                });
                if before != SessionState::Unknown {
                    publish_status(&self.status, &self.bus);
                }
                true
            }
        }
    }
}

/// Zalo's own words for an error it gave (Vietnamese), else the error.
fn friend_error(e: zeta_zalo::Error) -> String {
    match e {
        zeta_zalo::Error::Server { message, .. } if !message.trim().is_empty() => message,
        other => other.to_string(),
    }
}

/// A person as Zalo described them, for the store.
fn person_of(uid: &str, name: &str, zalo_name: &str, avatar: &str) -> Person {
    Person {
        uid: uid.to_string(),
        name: name.to_string(),
        zalo_name: zalo_name.to_string(),
        avatar: avatar.to_string(),
        ..Person::default()
    }
}

/// Carry out a friends-side action. People Zalo tells about are kept, so a chat
/// with them has a name and a picture; an accepted request makes a friend, with
/// a conversation.
async fn friend_action(
    client: &ZaloClient,
    store: &Store,
    bus: &EventBus,
    action: FriendAction,
) -> Result<Value, String> {
    let keep = |p: &Person, friend: Option<bool>| {
        if let Err(e) = store.upsert_person(p, friend) {
            tracing::error!(error = %e, "cannot store a person");
        }
    };
    match action {
        FriendAction::Find(phone) => {
            let Some(user) = client.find_user(&phone).await.map_err(friend_error)? else {
                return Ok(json!({ "user": null }));
            };
            keep(
                &person_of(&user.uid, &user.display_name, &user.zalo_name, &user.avatar),
                None,
            );
            let friendship = match client.friendship(&user.uid).await {
                Ok(f) => f,
                Err(e) => {
                    tracing::debug!(error = %e, "friendship not known");
                    zeta_zalo::friend::Friendship::None
                }
            };
            Ok(json!({ "user": {
                "uid": user.uid,
                "name": user.display_name,
                "zalo_name": user.zalo_name,
                "avatar": user.avatar,
                "friendship": friendship,
            }}))
        }
        FriendAction::Requests => {
            let list = client.friend_requests().await.map_err(friend_error)?;
            for r in &list {
                keep(
                    &person_of(&r.uid, &r.display_name, &r.zalo_name, &r.avatar),
                    None,
                );
            }
            Ok(json!(list))
        }
        FriendAction::Send { uid, message } => {
            client
                .send_friend_request(&uid, &message)
                .await
                .map_err(friend_error)?;
            Ok(json!({ "ok": true }))
        }
        FriendAction::Accept(uid) => {
            client
                .accept_friend_request(&uid)
                .await
                .map_err(friend_error)?;
            let person = store
                .people(std::slice::from_ref(&uid))
                .ok()
                .and_then(|list| list.into_iter().next())
                .unwrap_or_else(|| person_of(&uid, "", "", ""));
            keep(&person, Some(true));
            let thread = ThreadUpsert {
                kind: ThreadKind::User,
                remote_id: uid,
                name: person.name,
                avatar: person.avatar,
            };
            if let Err(e) = store.upsert_thread(&thread, true) {
                tracing::error!(error = %e, "cannot store the new friend's chat");
            }
            if let Err(e) = bus.publish("threads_changed", json!({})) {
                tracing::error!(error = %e, "cannot publish threads_changed");
            }
            Ok(json!({ "ok": true }))
        }
        FriendAction::Reject(uid) => {
            client
                .reject_friend_request(&uid)
                .await
                .map_err(friend_error)?;
            Ok(json!({ "ok": true }))
        }
    }
}

/// Members of a group listed at most (Zalo PC pages through larger groups).
const MEMBERS_SHOWN: usize = 300;

/// Who is in a group: Zalo's member list now, with names and pictures from the
/// store, members it does not know looked up first. The group's thread is
/// refreshed on the way.
async fn group_members(
    client: &ZaloClient,
    store: &Store,
    group_id: &str,
) -> Result<GroupMembers, String> {
    let group = client
        .group_info(&[group_id.to_string()])
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .next()
        .ok_or_else(|| "Zalo không trả thông tin nhóm".to_string())?;
    let thread = ThreadUpsert {
        kind: ThreadKind::Group,
        remote_id: group.id.clone(),
        name: group.name.clone(),
        avatar: group.avatar.clone(),
    };
    store
        .upsert_thread(&thread, true)
        .and_then(|()| store.set_members(&thread.id(), group.member_count))
        .map_err(|e| e.to_string())?;
    let shown: Vec<String> = group
        .member_ids
        .iter()
        .take(MEMBERS_SHOWN)
        .cloned()
        .collect();
    let known = store.people(&shown).map_err(|e| e.to_string())?;
    let now = now_ms();
    let unknown: Vec<String> = shown
        .iter()
        .filter(|uid| {
            !known.iter().any(|p| {
                &p.uid == *uid && !p.name.is_empty() && now - p.updated_at < PEOPLE_FRESH_MS
            })
        })
        .cloned()
        .collect();
    for chunk in unknown.chunks(50) {
        if let Err(e) = lookup_people(client, store, chunk).await {
            tracing::debug!(error = %e, "group members not looked up");
            break;
        }
    }
    let people = store.people(&shown).map_err(|e| e.to_string())?;
    let members = shown
        .iter()
        .map(|uid| {
            let person = people.iter().find(|p| &p.uid == uid);
            Member {
                uid: uid.clone(),
                name: person.map(|p| p.name.clone()).unwrap_or_default(),
                avatar: person.map(|p| p.avatar.clone()).unwrap_or_default(),
                friend: person.is_some_and(|p| p.friend),
                role: if *uid == group.creator_id {
                    "owner"
                } else if group.admin_ids.contains(uid) {
                    "admin"
                } else {
                    "member"
                },
            }
        })
        .collect::<Vec<_>>();
    Ok(GroupMembers {
        total: group.member_count.max(members.len() as u64),
        members,
    })
}

/// The media of a sticker message, as parsed from Zalo's own messages.
pub fn sticker_media(sticker: &Sticker) -> Option<Media> {
    let content = json!({ "id": sticker.id, "catId": sticker.cate_id, "type": sticker.kind });
    media::extract("chat.sticker", Some(&content))
}

/// Ask Zalo about users and store what it says; users it does not answer for
/// are noted as asked, so they are not asked again at once.
async fn lookup_people(
    client: &ZaloClient,
    store: &Store,
    uids: &[String],
) -> Result<Vec<Person>, String> {
    let asked: Vec<String> = uids.iter().take(50).cloned().collect();
    let found = client
        .user_profiles(&asked)
        .await
        .map_err(|e| e.to_string())?;
    tracing::debug!(asked = asked.len(), found = found.len(), "people looked up");
    for f in found {
        let person = Person {
            uid: f.uid,
            name: f.display_name,
            zalo_name: f.zalo_name,
            avatar: f.avatar,
            ..Person::default()
        };
        store
            .upsert_person(&person, None)
            .map_err(|e| e.to_string())?;
    }
    store
        .touch_people(&asked, now_ms())
        .and_then(|()| store.people(&asked))
        .map_err(|e| e.to_string())
}

fn recipient(kind: ThreadKind, remote_id: &str) -> Recipient<'_> {
    match kind {
        ThreadKind::User => Recipient::User(remote_id),
        ThreadKind::Group => Recipient::Group(remote_id),
    }
}

/// A message Zalo accepted, to be stored as outgoing.
struct Sent {
    msg_id: String,
    client_id: u64,
    msg_type: &'static str,
    text: String,
    media: Option<Media>,
    quote: Option<Quote>,
    /// Zalo-shaped JSON of what was sent, when worth keeping (a sticker's
    /// ids, so it can be forwarded).
    raw: Option<Value>,
}

/// This account, as a sender.
struct Me {
    uid: String,
    /// Display name, shown when someone quotes our message.
    name: String,
}

/// Store a message this gateway sent and announce it. When the socket echo was
/// faster than the HTTP answer, the copy stored from the echo is returned.
fn store_sent(
    store: &Store,
    bus: &EventBus,
    self_uid: &str,
    kind: ThreadKind,
    remote_id: &str,
    sent: Sent,
) -> Result<Message, String> {
    let thread = ThreadUpsert {
        kind,
        remote_id: remote_id.to_string(),
        name: String::new(),
        avatar: String::new(),
    };
    let id = thread.id();
    let client_id = sent.client_id.to_string();
    let new = NewMessage {
        thread_id: id.clone(),
        remote_id: if sent.msg_id.is_empty() {
            format!("local:{client_id}")
        } else {
            sent.msg_id
        },
        client_id: client_id.clone(),
        sender_id: self_uid.to_string(),
        sender_name: String::new(),
        outgoing: true,
        kind: sent.msg_type.into(),
        text: sent.text,
        media: sent.media,
        quote: sent.quote,
        sent_at: now_ms(),
        raw: sent.raw.map_or_else(|| "{}".to_string(), |v| v.to_string()),
    };
    store
        .upsert_thread(&thread, false)
        .map_err(|e| e.to_string())?;
    match store.insert_message(&new).map_err(|e| e.to_string())? {
        Some(message) => {
            publish_message(store, bus, &message);
            Ok(message)
        }
        None => store
            .message_by_client_id(&id, &client_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "tin nhắn đã gửi nhưng không tìm thấy trong kho".to_string()),
    }
}

async fn send_and_store(
    client: &ZaloClient,
    store: &Store,
    bus: &EventBus,
    me: &Me,
    (kind, remote_id): (ThreadKind, &str),
    text: &str,
    reply_to: Option<&ReplyTo>,
) -> Result<Message, String> {
    let to = recipient(kind, remote_id);
    let err = |e: zeta_zalo::Error| e.to_string();
    let (sent, quote) = match reply_to {
        Some(r) => {
            let sent = client
                .send_reply(to, text, &quoted_message(r, &me.uid))
                .await
                .map_err(err)?;
            let quote = Quote {
                sender_name: if r.message.outgoing {
                    me.name.clone()
                } else {
                    r.message.sender_name.clone()
                },
                text: r.message.text.clone(),
                remote_id: r.message.remote_id.clone(),
            };
            (sent, Some(quote))
        }
        None => (client.send_text(to, text).await.map_err(err)?, None),
    };
    let sent = Sent {
        msg_id: sent.msg_id,
        client_id: sent.client_id,
        msg_type: "webchat",
        text: text.to_string(),
        media: None,
        quote,
        raw: None,
    };
    store_sent(store, bus, &me.uid, kind, remote_id, sent)
}

/// Everything a background send of a photo or file needs.
struct AttachmentSender {
    client: Arc<ZaloClient>,
    store: Arc<Store>,
    bus: EventBus,
    media: Option<Arc<MediaCache>>,
    uploads: Arc<UploadBoard>,
    self_uid: String,
}

/// What an upload cancelled from a device ends with (never shown: a cancelled
/// upload stays cancelled).
const CANCELLED: &str = "đã hủy";

/// Wait for the turn of an upload, then send it, unless it is cancelled first.
async fn run_upload(
    sender: &AttachmentSender,
    turns: &Semaphore,
    kind: ThreadKind,
    remote_id: &str,
    attachment: &Attachment,
    job: &UploadJob,
) {
    let turn = tokio::select! {
        turn = turns.acquire() => turn,
        () = job.cancelled() => return,
    };
    let Ok(_turn) = turn else {
        job.finish(Err("gateway đang dừng".into()));
        return;
    };
    if !job.start() {
        return;
    }
    let result = tokio::select! {
        result = sender.send(kind, remote_id, attachment, job) => result,
        // Dropping the send stops it between two requests to Zalo; once the
        // message itself goes out, cancelling is refused.
        () = job.cancelled() => return,
    };
    if let Err(e) = &result {
        tracing::warn!(error = %e, "attachment not sent");
    }
    job.finish(result);
}

impl AttachmentSender {
    async fn send(
        &self,
        kind: ThreadKind,
        remote_id: &str,
        a: &Attachment,
        job: &UploadJob,
    ) -> Result<Message, String> {
        let to = recipient(kind, remote_id);
        let err = |e: zeta_zalo::Error| e.to_string();
        let caption = a.caption.trim().to_string();
        let size = a.size;
        let src = FileSource {
            path: &a.path,
            size: a.size,
            file_name: &a.name,
            mime: &a.mime,
        };
        let progress = |stored: u64| job.progress(stored);
        let sent = if a.photo {
            let uploaded = self
                .client
                .upload_photo(to, src, &progress)
                .await
                .map_err(err)?;
            if !job.begin_finishing() {
                return Err(CANCELLED.into());
            }
            let photo = self
                .client
                .send_uploaded_photo(to, &uploaded, a.width, a.height, &caption)
                .await
                .map_err(err)?;
            self.keep(&[&photo.hd_url, &photo.thumb_url], a).await;
            Sent {
                msg_id: photo.msg_id,
                client_id: photo.client_id,
                msg_type: "chat.photo",
                text: if caption.is_empty() {
                    "[Hình ảnh]".into()
                } else {
                    format!("[Hình ảnh] {caption}")
                },
                media: Some(Media {
                    kind: "image".into(),
                    url: photo.hd_url,
                    thumb: photo.thumb_url,
                    width: a.width,
                    height: a.height,
                    description: caption,
                    size,
                    ..Media::default()
                }),
                quote: None,
                raw: None,
            }
        } else {
            let uploaded = self
                .client
                .upload_file(to, src, &a.md5, &progress)
                .await
                .map_err(err)?;
            let (file_id, url) = self.stored_file(to, src, &uploaded).await?;
            let uploaded = UploadedFile {
                file_id,
                ..uploaded
            };
            if !job.begin_finishing() {
                return Err(CANCELLED.into());
            }
            let file = self
                .client
                .send_file(to, &uploaded, &url)
                .await
                .map_err(err)?;
            // Very large files are not kept: the cache would not keep them either.
            if size <= MAX_FILE_BYTES {
                self.keep(&[&url], a).await;
            }
            Sent {
                msg_id: file.msg_id,
                client_id: file.client_id,
                msg_type: "share.file",
                text: format!("[Tập tin] {}", a.name),
                media: Some(Media {
                    kind: "file".into(),
                    url,
                    name: a.name.clone(),
                    size,
                    ..Media::default()
                }),
                quote: None,
                raw: None,
            }
        };
        store_sent(
            &self.store,
            &self.bus,
            &self.self_uid,
            kind,
            remote_id,
            sent,
        )
    }

    /// Wait until Zalo announces the stored file; returns its id and URL. While
    /// the announcement is late, the last part goes again, like Zalo PC does.
    async fn stored_file(
        &self,
        to: Recipient<'_>,
        src: FileSource<'_>,
        file: &UploadedFile,
    ) -> Result<(String, String), String> {
        let patience = file_done_patience(file.size);
        let mut ids = vec![file.file_id.clone()];
        for resend in 0..=LAST_PART_RESENDS {
            if resend > 0 {
                tracing::info!(
                    resend,
                    "file not announced yet, sending its last part again"
                );
                match self.client.resend_last_part(to, src, file).await {
                    Ok(id) if !id.is_empty() && !ids.contains(&id) => ids.push(id),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "cannot send the last part again"),
                }
            }
            if let Some(found) = self.uploads.wait_any(&ids, patience).await {
                return Ok(found);
            }
        }
        Err("Zalo chưa xác nhận đã nhận tệp, hãy thử lại".into())
    }

    /// Keep a copy of the sent file in the media cache under the URLs Zalo gave
    /// it, so the sender sees it at once without downloading it back.
    async fn keep(&self, urls: &[&String], a: &Attachment) {
        let Some(cache) = &self.media else {
            return;
        };
        let mut seen: Vec<&String> = Vec::new();
        for url in urls.iter().copied() {
            if url.is_empty() || seen.contains(&url) {
                continue;
            }
            seen.push(url);
            if let Err(e) = cache.insert_file(url, &a.path, &a.mime).await {
                tracing::debug!(error = %e, "cannot cache a sent file");
            }
        }
    }
}

fn set_login(status: &RwLock<ZaloStatus>, bus: &EventBus, state: LoginState) {
    update_status(status, |s| s.login = state);
    publish_status(status, bus);
}

async fn login_task(
    device: Device,
    session_path: PathBuf,
    status: Arc<RwLock<ZaloStatus>>,
    bus: EventBus,
    reply: oneshot::Sender<LoginState>,
    done: mpsc::Sender<LoginOutcome>,
) {
    let mut reply = Some(reply);
    let outcome = match run_login(device, &session_path, &status, &bus, &mut reply).await {
        Ok(client) => LoginOutcome::Success(Box::new(client)),
        Err(reason) => {
            tracing::warn!(%reason, "QR login failed");
            let failed = LoginState::Failed { reason };
            set_login(&status, &bus, failed.clone());
            if let Some(reply) = reply.take() {
                let _ = reply.send(failed);
            }
            LoginOutcome::Failed
        }
    };
    let _ = done.send(outcome).await;
}

async fn run_login(
    device: Device,
    session_path: &std::path::Path,
    status: &RwLock<ZaloStatus>,
    bus: &EventBus,
    reply: &mut Option<oneshot::Sender<LoginState>>,
) -> Result<ZaloClient, String> {
    let err = |e: zeta_zalo::Error| e.to_string();
    let mut client = ZaloClient::new(device).map_err(err)?;
    if let Err(e) = client.refresh_server_info().await {
        tracing::warn!(error = %e, "getServerInfo failed, using default hosts");
    }
    let ticket = client.request_qr("vi").await.map_err(err)?;
    let waiting = LoginState::WaitingScan {
        qr: ticket.payload.clone(),
        expires_at: now_ms() + ticket.timeout.as_millis() as i64,
    };
    set_login(status, bus, waiting.clone());
    if let Some(reply) = reply.take() {
        let _ = reply.send(waiting);
    }
    let scan = client.wait_for_scan(&ticket).await.map_err(err)?;
    set_login(status, bus, LoginState::WaitingConfirm);
    let zpw_sek = client.wait_for_confirm(&ticket).await.map_err(err)?;
    let session = client
        .finish_login(zpw_sek, Some(&scan))
        .await
        .map_err(err)?;
    session.save(session_path).map_err(err)?;
    tracing::info!("QR login succeeded, session saved");
    Ok(client)
}

/// Thread id of a parsed message (handy for tests and callers).
pub fn parsed_thread_id(m: &ParsedMessage) -> String {
    thread_id(m.kind, &m.peer)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: &str = "900";

    #[test]
    fn routes_incoming_outgoing_note_and_group_messages() {
        let payload = json!({ "data": {
            "msgs": [
                { "msgId": "1", "uidFrom": "100", "idTo": "0", "dName": "Lan", "ts": "1700000000001",
                  "msgType": "webchat", "content": "chào" },
                { "msgId": "2", "cliMsgId": "555", "uidFrom": "0", "idTo": "100", "ts": "1700000000002",
                  "msgType": "webchat", "content": "chào Lan" },
                { "msgId": "3", "uidFrom": "0", "idTo": "0", "ts": "3", "msgType": "webchat", "content": "ghi chú" },
                { "uidFrom": "100", "idTo": "0", "content": "no id, skipped" }
            ],
            "groupMsgs": [
                { "msgId": "4", "uidFrom": "200", "idTo": "group_777", "dName": "Minh", "ts": "4",
                  "msgType": "chat.photo",
                  "content": { "title": "ảnh đẹp", "href": "https://photo.zdn.vn/a.jpg" } }
            ]
        }});
        let got = parse_frame_messages(&payload, ME);
        assert_eq!(got.len(), 4);

        assert_eq!(
            (got[0].kind, got[0].peer.as_str(), got[0].outgoing),
            (ThreadKind::User, "100", false)
        );
        assert_eq!(
            (got[0].sender_name.as_str(), got[0].text.as_str()),
            ("Lan", "chào")
        );
        assert_eq!(got[0].sent_at, 1_700_000_000_001);

        assert_eq!((got[1].peer.as_str(), got[1].outgoing), ("100", true));
        assert_eq!(
            (got[1].sender_id.as_str(), got[1].client_id.as_str()),
            (ME, "555")
        );

        assert_eq!((got[2].peer.as_str(), got[2].outgoing), (ME, true));

        assert_eq!(
            (got[3].kind, got[3].peer.as_str()),
            (ThreadKind::Group, "777")
        );
        assert_eq!(got[3].text, "[Hình ảnh] ảnh đẹp");
        let photo = got[3].media.as_ref().unwrap();
        assert_eq!(
            (photo.kind.as_str(), photo.url.as_str()),
            ("image", "https://photo.zdn.vn/a.jpg")
        );
        assert!(got[0].media.is_none());
        assert_eq!(parsed_thread_id(&got[3]), "zalo:g:777");
    }

    #[tokio::test]
    async fn upload_announcements_reach_their_sender() {
        let board = Arc::new(UploadBoard::default());
        let ids = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let found = |id: &str| Some((id.to_string(), format!("https://f.zdn.vn/{id}")));
        // Announced before anyone waits (the socket can beat the HTTP answer).
        board.complete("1".into(), "https://f.zdn.vn/1".into());
        assert_eq!(
            board
                .wait_any(&ids(&["1"]), Duration::from_millis(10))
                .await,
            found("1")
        );
        // Announced while waiting, for the second of two ids.
        let waiter = {
            let board = board.clone();
            let wanted = ids(&["2", "4"]);
            tokio::spawn(async move { board.wait_any(&wanted, Duration::from_secs(5)).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        board.complete("3".into(), "https://f.zdn.vn/3".into());
        board.complete("4".into(), "https://f.zdn.vn/4".into());
        assert_eq!(waiter.await.unwrap(), found("4"));
        assert_eq!(
            board
                .wait_any(&ids(&["9"]), Duration::from_millis(10))
                .await,
            None
        );
    }

    #[test]
    fn photos_and_files() {
        assert!(photo_type("image/png"));
        assert!(!photo_type("image/svg+xml"));
        assert!(!photo_type("application/pdf"));
        // A minute and a half, plus a second per 8 MB.
        assert_eq!(file_done_patience(1000), Duration::from_secs(90));
        assert_eq!(
            file_done_patience(1024 * 1024 * 1024),
            Duration::from_secs(218)
        );
    }

    #[test]
    fn explicit_group_id_wins_outside_the_group_bucket() {
        let payload = json!({ "data": { "msgs": [
            { "msgId": "9", "uidFrom": "200", "idTo": "0", "grid": "888", "content": "x" }
        ]}});
        let got = parse_frame_messages(&payload, ME);
        assert_eq!(
            (got[0].kind, got[0].peer.as_str()),
            (ThreadKind::Group, "888")
        );
    }
}
