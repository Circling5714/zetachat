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
use tokio::sync::{mpsc, oneshot};
use zeta_zalo::api::{Recipient, SessionHealth};
use zeta_zalo::socket::{self, RealtimeEvent};
use zeta_zalo::upload::{self, Photo};
use zeta_zalo::{Device, SessionData, ZaloClient};

use crate::events::EventBus;
use crate::media;
use crate::media_cache::MediaCache;
use crate::model::{now_ms, thread_id, Media, Message, NewMessage, ThreadKind, ThreadUpsert};
use crate::store::Store;

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
    pub login: LoginState,
    /// Human-readable detail for `unknown` / `rejected`.
    pub detail: String,
}

/// A photo or file a device wants to send.
#[derive(Clone, Debug)]
pub struct Attachment {
    pub name: String,
    pub mime: String,
    pub bytes: Vec<u8>,
    /// Picture size, 0 when unknown.
    pub width: u32,
    pub height: u32,
    /// Shown under a photo; may be empty.
    pub caption: String,
}

impl Attachment {
    /// Pictures Zalo shows as photos; everything else is sent as a file.
    pub fn is_photo(&self) -> bool {
        matches!(
            self.mime.as_str(),
            "image/jpeg" | "image/png" | "image/webp" | "image/gif"
        )
    }
}

pub enum ZaloCommand {
    SendText {
        kind: ThreadKind,
        remote_id: String,
        text: String,
        reply: oneshot::Sender<Result<Message, String>>,
    },
    SendAttachment {
        kind: ThreadKind,
        remote_id: String,
        attachment: Attachment,
        reply: oneshot::Sender<Result<Message, String>>,
    },
    StartLogin {
        reply: oneshot::Sender<LoginState>,
    },
}

/// How long to wait for Zalo to announce an uploaded file on the socket.
const FILE_DONE_TIMEOUT: Duration = Duration::from_secs(90);

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

    async fn wait(&self, file_id: &str, timeout: Duration) -> Option<String> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            // Registered before looking, so an announcement in between still wakes us.
            let changed = self.changed.notified();
            let found = self
                .done
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(file_id);
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
}

impl ZaloHandle {
    pub fn status(&self) -> ZaloStatus {
        self.status
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
}

/// Start the connector task.
pub fn spawn(cfg: ZaloConfig, store: Arc<Store>, bus: EventBus) -> ZaloHandle {
    let (commands, rx) = mpsc::channel(32);
    let status = Arc::new(RwLock::new(ZaloStatus::default()));
    let connector = Connector {
        cfg,
        store,
        bus,
        status: status.clone(),
        client: None,
        self_uid: String::new(),
        login_running: false,
        uploads: Arc::new(UploadBoard::default()),
    };
    tokio::spawn(connector.run(rx));
    ZaloHandle { commands, status }
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

fn parse_message(it: &Value, group_bucket: bool, self_uid: &str) -> Option<ParsedMessage> {
    let remote_id = field(it, &["msgId", "globalMsgId", "cliMsgId"]);
    if remote_id.is_empty() {
        return None;
    }
    let uid_from = field(it, &["uidFrom"]);
    let id_to = field(it, &["idTo"]);
    let grid = field(it, &["grid", "groupId"]);
    let outgoing = is_zero(&uid_from) || uid_from == self_uid;

    let (kind, peer) = if group_bucket || !is_zero(&grid) {
        let group = if is_zero(&grid) { &id_to } else { &grid };
        (
            ThreadKind::Group,
            group.trim_start_matches("group_").to_string(),
        )
    } else if is_zero(&id_to) && is_zero(&uid_from) {
        (ThreadKind::User, self_uid.to_string())
    } else if !outgoing && (is_zero(&id_to) || id_to == self_uid) {
        (ThreadKind::User, uid_from.clone())
    } else if !is_zero(&id_to) {
        (ThreadKind::User, id_to.clone())
    } else {
        (ThreadKind::User, uid_from.clone())
    };
    if is_zero(&peer) {
        return None;
    }

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
    client: Option<Arc<ZaloClient>>,
    self_uid: String,
    login_running: bool,
    uploads: Arc<UploadBoard>,
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
        client: ZaloClient,
    ) -> Option<mpsc::UnboundedReceiver<RealtimeEvent>> {
        let health = client.check_session().await;
        match &health {
            SessionHealth::Rejected => {
                self.client = None;
                self.set_session(SessionState::Rejected, "Zalo từ chối phiên đã lưu".into());
                return None;
            }
            SessionHealth::Valid(profile) => update_status(&self.status, |s| {
                s.session = SessionState::Valid;
                s.display_name = profile.display_name.clone();
                s.detail.clear();
            }),
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

    /// Friends become threads, so incoming messages have a name. Zalo rate-limits
    /// this call, so it only runs when a session comes up.
    async fn sync_contacts(&self, client: &ZaloClient) {
        match client.friends().await {
            Ok(friends) => {
                let count = friends.len();
                for f in friends {
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
                    if let Err(e) = self.store.upsert_thread(&thread, true) {
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
                reply,
            } => {
                let Some(client) = self.client.clone() else {
                    let _ = reply.send(Err("Zalo chưa đăng nhập".into()));
                    return;
                };
                let store = self.store.clone();
                let bus = self.bus.clone();
                let self_uid = self.self_uid.clone();
                tokio::spawn(async move {
                    let result =
                        send_and_store(&client, &store, &bus, &self_uid, kind, &remote_id, &text)
                            .await;
                    let _ = reply.send(result);
                });
            }
            ZaloCommand::SendAttachment {
                kind,
                remote_id,
                attachment,
                reply,
            } => {
                let Some(client) = self.client.clone() else {
                    let _ = reply.send(Err("Zalo chưa đăng nhập".into()));
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
                tokio::spawn(async move {
                    let result = sender.send(kind, &remote_id, attachment).await;
                    if let Err(e) = &result {
                        tracing::warn!(error = %e, "attachment not sent");
                    }
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
                tracing::debug!(cmd = frame.cmd, sub = frame.sub_cmd, "frame");
                if frame.is_control() {
                    for (file_id, url) in upload::file_done_events(&frame.payload) {
                        tracing::debug!(%file_id, "file stored by Zalo");
                        self.uploads.complete(file_id, url);
                    }
                }
                true
            }
        }
    }

    /// Recent messages (`get-last-msgs`), so the conversation list is not empty
    /// after a fresh start. Stored quietly: no unread counter, no notification;
    /// clients just reload their lists.
    async fn load_recent(&self, client: &ZaloClient) {
        match client.recent_messages().await {
            Ok(data) => {
                let stored = parse_frame_messages(&data, &self.self_uid)
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

    /// Store a parsed message; returns it when it was new. `live` messages count as
    /// unread when incoming.
    fn store_parsed(&self, m: ParsedMessage, live: bool) -> Option<Message> {
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
                update_status(&self.status, |s| {
                    s.session = SessionState::Valid;
                    s.display_name = profile.display_name;
                    s.detail.clear();
                });
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
        sent_at: now_ms(),
        raw: "{}".into(),
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
    self_uid: &str,
    kind: ThreadKind,
    remote_id: &str,
    text: &str,
) -> Result<Message, String> {
    let sent = client
        .send_text(recipient(kind, remote_id), text)
        .await
        .map_err(|e| e.to_string())?;
    let sent = Sent {
        msg_id: sent.msg_id,
        client_id: sent.client_id,
        msg_type: "webchat",
        text: text.to_string(),
        media: None,
    };
    store_sent(store, bus, self_uid, kind, remote_id, sent)
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

impl AttachmentSender {
    async fn send(
        &self,
        kind: ThreadKind,
        remote_id: &str,
        a: Attachment,
    ) -> Result<Message, String> {
        let to = recipient(kind, remote_id);
        let err = |e: zeta_zalo::Error| e.to_string();
        let caption = a.caption.trim().to_string();
        let size = a.bytes.len() as u64;
        let sent = if a.is_photo() {
            let photo = Photo {
                bytes: &a.bytes,
                file_name: &a.name,
                mime: &a.mime,
                width: a.width,
                height: a.height,
                caption: &caption,
            };
            let photo = self.client.send_photo(to, photo).await.map_err(err)?;
            self.keep(&[&photo.hd_url, &photo.thumb_url], &a).await;
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
            }
        } else {
            let uploaded = self
                .client
                .upload_file(to, &a.bytes, &a.name, &a.mime)
                .await
                .map_err(err)?;
            let url = self
                .uploads
                .wait(&uploaded.file_id, FILE_DONE_TIMEOUT)
                .await
                .ok_or_else(|| "Zalo chưa xác nhận đã nhận tệp, hãy thử lại".to_string())?;
            let file = self
                .client
                .send_file(to, &uploaded, &url)
                .await
                .map_err(err)?;
            self.keep(&[&url], &a).await;
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

    /// Keep the sent bytes in the media cache under the URLs Zalo gave them, so
    /// the sender sees them at once without downloading them back.
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
            if let Err(e) = cache.insert(url, &a.bytes, &a.mime).await {
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
        // Announced before anyone waits (the socket can beat the HTTP answer).
        board.complete("1".into(), "https://f.zdn.vn/1".into());
        assert_eq!(
            board.wait("1", Duration::from_millis(10)).await.as_deref(),
            Some("https://f.zdn.vn/1")
        );
        // Announced while waiting.
        let waiter = {
            let board = board.clone();
            tokio::spawn(async move { board.wait("2", Duration::from_secs(5)).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        board.complete("3".into(), "https://f.zdn.vn/3".into());
        board.complete("2".into(), "https://f.zdn.vn/2".into());
        assert_eq!(waiter.await.unwrap().as_deref(), Some("https://f.zdn.vn/2"));
        assert_eq!(board.wait("9", Duration::from_millis(10)).await, None);
    }

    #[test]
    fn photos_and_files() {
        let a = |mime: &str| Attachment {
            name: "x".into(),
            mime: mime.into(),
            bytes: vec![1],
            width: 0,
            height: 0,
            caption: String::new(),
        };
        assert!(a("image/png").is_photo());
        assert!(!a("image/svg+xml").is_photo());
        assert!(!a("application/pdf").is_photo());
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
