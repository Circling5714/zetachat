//! Photos and files on their way to Zalo.
//!
//! A device posts the file; the gateway writes it to disk as it arrives (never
//! whole in memory), answers at once with an upload id and sends the file to
//! Zalo in the background, one file at a time, in parts. Progress goes out as
//! transient `upload` events, which are not kept in the event log; `GET
//! /api/v1/uploads` lists the uploads in progress and the ones that ended in the
//! last minutes. An upload can be cancelled until the message itself is sent.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use futures_util::StreamExt;
use md5::{Digest, Md5};
use serde::Serialize;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;

use crate::events::EventBus;
use crate::model::{now_ms, Message};

/// Uploads waiting or running at the same time.
pub const MAX_PENDING: usize = 20;

/// How long uploads that ended stay listed.
const KEEP_ENDED: Duration = Duration::from_secs(10 * 60);

/// Progress events go out at most this often per upload.
const PROGRESS_EVERY: Duration = Duration::from_millis(400);

/// A device that stops sending for this long loses its upload.
const BODY_IDLE: Duration = Duration::from_secs(60);

/// Bytes kept from the start of a file, to recognise its type.
const HEAD_BYTES: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UploadState {
    /// Waiting for the uploads before it.
    Queued,
    /// Going to Zalo; when `sent == size`, Zalo is processing the file.
    Sending,
    /// Zalo has the file; the message is being sent. Too late to cancel.
    Finishing,
    Done,
    Failed,
    Cancelled,
}

impl UploadState {
    pub fn has_ended(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
}

/// What devices see of an upload.
#[derive(Clone, Debug, Serialize)]
pub struct Upload {
    pub id: String,
    pub thread_id: String,
    pub name: String,
    pub size: u64,
    /// `photo` or `file`.
    pub kind: &'static str,
    pub state: UploadState,
    /// Bytes Zalo has stored.
    pub sent: u64,
    /// Why it failed; empty otherwise.
    pub error: String,
    /// The message, once sent.
    pub message: Option<Message>,
    pub created_at: i64,
}

/// Why an upload cannot be cancelled.
#[derive(Debug, PartialEq, Eq)]
pub enum CancelError {
    /// The message is being sent or the upload has ended.
    TooLate(UploadState),
}

/// One upload, shared by the API (which cancels it) and the sender.
pub struct UploadJob {
    info: Mutex<Upload>,
    path: PathBuf,
    bus: EventBus,
    cancelled: AtomicBool,
    cancel: Notify,
    last_progress: Mutex<Option<Instant>>,
    ended_at: Mutex<Option<Instant>>,
}

impl UploadJob {
    fn lock(&self) -> std::sync::MutexGuard<'_, Upload> {
        self.info.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn snapshot(&self) -> Upload {
        self.lock().clone()
    }

    pub fn id(&self) -> String {
        self.lock().id.clone()
    }

    pub fn state(&self) -> UploadState {
        self.lock().state
    }

    /// The file, until the upload ends.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn publish(&self, upload: Upload) {
        self.bus.notify("upload", json!({ "upload": upload }));
    }

    /// Change the state unless the upload was cancelled; false if it was.
    fn advance(&self, from: &[UploadState], to: UploadState) -> bool {
        let snapshot = {
            let mut info = self.lock();
            if !from.contains(&info.state) {
                return false;
            }
            info.state = to;
            info.clone()
        };
        self.publish(snapshot);
        true
    }

    /// Its turn has come. False when it was cancelled meanwhile.
    pub fn start(&self) -> bool {
        self.advance(&[UploadState::Queued], UploadState::Sending)
    }

    /// Zalo stored `sent` bytes so far.
    pub fn progress(&self, sent: u64) {
        let snapshot = {
            let mut info = self.lock();
            if info.state != UploadState::Sending || sent <= info.sent {
                return;
            }
            info.sent = sent.min(info.size);
            let complete = info.sent == info.size;
            let mut last = self.last_progress.lock().unwrap_or_else(|e| e.into_inner());
            let due = last.is_none_or(|t| t.elapsed() >= PROGRESS_EVERY);
            if !(due || complete) {
                return;
            }
            *last = Some(Instant::now());
            info.clone()
        };
        self.publish(snapshot);
    }

    /// The message is about to be sent: no cancelling from now on. False when it
    /// was cancelled before.
    pub fn begin_finishing(&self) -> bool {
        self.advance(&[UploadState::Sending], UploadState::Finishing)
    }

    /// The upload ended with `result`; a cancelled upload stays cancelled.
    pub fn finish(&self, result: Result<Message, String>) {
        let snapshot = {
            let mut info = self.lock();
            if info.state.has_ended() {
                return;
            }
            match result {
                Ok(message) => {
                    info.state = UploadState::Done;
                    info.sent = info.size;
                    info.message = Some(message);
                }
                Err(error) => {
                    info.state = UploadState::Failed;
                    info.error = error;
                }
            }
            info.clone()
        };
        self.mark_ended();
        self.publish(snapshot);
    }

    /// Cancel a queued or sending upload.
    pub fn cancel(&self) -> Result<Upload, CancelError> {
        let snapshot = {
            let mut info = self.lock();
            if !matches!(info.state, UploadState::Queued | UploadState::Sending) {
                return Err(CancelError::TooLate(info.state));
            }
            info.state = UploadState::Cancelled;
            info.clone()
        };
        self.cancelled.store(true, Ordering::SeqCst);
        self.cancel.notify_waiters();
        self.mark_ended();
        self.publish(snapshot.clone());
        Ok(snapshot)
    }

    /// Resolves once the upload is cancelled.
    pub async fn cancelled(&self) {
        loop {
            // Registered before looking, so a cancel in between still wakes us.
            let notified = self.cancel.notified();
            if self.cancelled.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }

    fn mark_ended(&self) {
        *self.ended_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    }

    fn ended_long_ago(&self) -> bool {
        self.ended_at
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some_and(|t| t.elapsed() >= KEEP_ENDED)
    }

    /// Delete the file. Call once the upload has ended.
    pub async fn remove_file(&self) {
        if let Err(e) = tokio::fs::remove_file(&self.path).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(error = %e, "cannot delete an uploaded file");
            }
        }
    }
}

/// A file received from a device, waiting on disk. Deleted when dropped,
/// unless an upload took it over.
pub struct Spooled {
    path: Option<PathBuf>,
    pub size: u64,
    /// MD5 of the content, hex.
    pub md5: String,
    /// The first bytes, to recognise the type.
    pub head: Vec<u8>,
}

impl Drop for Spooled {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[derive(Debug)]
pub enum SpoolError {
    Empty,
    /// Over the limit, in bytes.
    TooLarge(u64),
    /// The device stopped sending or the connection broke.
    Interrupted(String),
    Io(std::io::Error),
}

impl std::fmt::Display for SpoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "file is empty"),
            Self::TooLarge(limit) => write!(f, "file larger than {limit} bytes"),
            Self::Interrupted(reason) => write!(f, "upload interrupted: {reason}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

/// All uploads of this run. Files live in `dir` while they wait.
pub struct Uploads {
    dir: PathBuf,
    bus: EventBus,
    jobs: Mutex<Vec<Arc<UploadJob>>>,
}

fn random_id() -> String {
    format!("up_{:016x}", rand::random::<u64>())
}

impl Uploads {
    /// Uploads do not survive a restart: files left in `dir` are deleted.
    pub fn new(dir: PathBuf, bus: EventBus) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        for entry in std::fs::read_dir(&dir)?.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_file()) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        Ok(Self {
            dir,
            bus,
            jobs: Mutex::new(Vec::new()),
        })
    }

    fn jobs(&self) -> std::sync::MutexGuard<'_, Vec<Arc<UploadJob>>> {
        let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        jobs.retain(|job| !job.ended_long_ago());
        jobs
    }

    /// Uploads in progress and those that ended recently, oldest first.
    pub fn list(&self) -> Vec<Upload> {
        self.jobs().iter().map(|job| job.snapshot()).collect()
    }

    pub fn get(&self, id: &str) -> Option<Arc<UploadJob>> {
        self.jobs().iter().find(|job| job.id() == id).cloned()
    }

    /// Uploads that have not ended.
    pub fn pending(&self) -> usize {
        self.jobs()
            .iter()
            .filter(|job| !job.state().has_ended())
            .count()
    }

    /// Write a request body to disk, at most `limit` bytes.
    pub async fn receive(&self, body: Body, limit: u64) -> Result<Spooled, SpoolError> {
        let path = self.dir.join(format!("{}.part", random_id()));
        let mut spooled = Spooled {
            path: Some(path.clone()),
            size: 0,
            md5: String::new(),
            head: Vec::new(),
        };
        let mut file = tokio::fs::File::create(&path)
            .await
            .map_err(SpoolError::Io)?;
        let mut hasher = Md5::new();
        let mut stream = body.into_data_stream();
        loop {
            let chunk = match tokio::time::timeout(BODY_IDLE, stream.next()).await {
                Err(_) => return Err(SpoolError::Interrupted("no data for a minute".into())),
                Ok(None) => break,
                Ok(Some(Err(e))) => return Err(SpoolError::Interrupted(e.to_string())),
                Ok(Some(Ok(chunk))) => chunk,
            };
            spooled.size += chunk.len() as u64;
            if spooled.size > limit {
                return Err(SpoolError::TooLarge(limit));
            }
            if spooled.head.len() < HEAD_BYTES {
                let take = (HEAD_BYTES - spooled.head.len()).min(chunk.len());
                spooled.head.extend_from_slice(&chunk[..take]);
            }
            hasher.update(&chunk);
            file.write_all(&chunk).await.map_err(SpoolError::Io)?;
        }
        file.flush().await.map_err(SpoolError::Io)?;
        if spooled.size == 0 {
            return Err(SpoolError::Empty);
        }
        spooled.md5 = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        Ok(spooled)
    }

    /// Copy a file the gateway already has (a cached photo to forward…) to
    /// the spool, as if a device had sent it; at most `limit` bytes.
    pub async fn spool_copy(&self, src: &Path, limit: u64) -> Result<Spooled, SpoolError> {
        let mut input = tokio::fs::File::open(src).await.map_err(SpoolError::Io)?;
        let size = input.metadata().await.map_err(SpoolError::Io)?.len();
        if size > limit {
            return Err(SpoolError::TooLarge(limit));
        }
        let path = self.dir.join(format!("{}.part", random_id()));
        let mut spooled = Spooled {
            path: Some(path.clone()),
            size: 0,
            md5: String::new(),
            head: Vec::new(),
        };
        let mut output = tokio::fs::File::create(&path)
            .await
            .map_err(SpoolError::Io)?;
        let mut hasher = Md5::new();
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let n = input.read(&mut buf).await.map_err(SpoolError::Io)?;
            if n == 0 {
                break;
            }
            let chunk = &buf[..n];
            spooled.size += n as u64;
            if spooled.head.len() < HEAD_BYTES {
                let take = (HEAD_BYTES - spooled.head.len()).min(n);
                spooled.head.extend_from_slice(&chunk[..take]);
            }
            hasher.update(chunk);
            output.write_all(chunk).await.map_err(SpoolError::Io)?;
        }
        output.flush().await.map_err(SpoolError::Io)?;
        if spooled.size == 0 {
            return Err(SpoolError::Empty);
        }
        spooled.md5 = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        Ok(spooled)
    }

    /// Queue a received file as an upload of `name` to `thread_id`.
    pub fn register(
        &self,
        mut file: Spooled,
        thread_id: &str,
        name: &str,
        photo: bool,
    ) -> Arc<UploadJob> {
        let path = file.path.take().unwrap_or_default();
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string)
            .unwrap_or_else(random_id);
        let upload = Upload {
            id,
            thread_id: thread_id.to_string(),
            name: name.to_string(),
            size: file.size,
            kind: if photo { "photo" } else { "file" },
            state: UploadState::Queued,
            sent: 0,
            error: String::new(),
            message: None,
            created_at: now_ms(),
        };
        let job = Arc::new(UploadJob {
            info: Mutex::new(upload.clone()),
            path,
            bus: self.bus.clone(),
            cancelled: AtomicBool::new(false),
            cancel: Notify::new(),
            last_progress: Mutex::new(None),
            ended_at: Mutex::new(None),
        });
        self.jobs().push(job.clone());
        job.publish(upload);
        job
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn uploads() -> (Uploads, EventBus, PathBuf) {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let bus = EventBus::new(store);
        let dir = std::env::temp_dir().join(format!("zetachat-up-{:016x}", rand::random::<u64>()));
        (Uploads::new(dir.clone(), bus.clone()).unwrap(), bus, dir)
    }

    #[tokio::test]
    async fn bodies_are_written_to_disk_with_their_checksum() {
        let (uploads, _, dir) = uploads();
        let file = uploads.receive(Body::from("hello"), 10).await.unwrap();
        // md5("hello")
        assert_eq!(file.md5, "5d41402abc4b2a76b9719d911017c592");
        assert_eq!((file.size, file.head.as_slice()), (5, &b"hello"[..]));
        let path = file.path.clone().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        drop(file);
        assert!(!path.exists(), "an unused file is deleted");

        assert!(matches!(
            uploads.receive(Body::from("0123456789A"), 10).await,
            Err(SpoolError::TooLarge(10))
        ));
        assert!(matches!(
            uploads.receive(Body::empty(), 10).await,
            Err(SpoolError::Empty)
        ));
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "nothing left over"
        );
    }

    #[tokio::test]
    async fn an_upload_goes_through_its_states_and_reports_them() {
        let (uploads, bus, _) = uploads();
        let mut events = bus.subscribe();
        let file = uploads
            .receive(Body::from("0123456789"), 100)
            .await
            .unwrap();
        let job = uploads.register(file, "zalo:u:1", "a.txt", false);
        assert!(job.path().exists());
        assert_eq!(uploads.pending(), 1);

        assert!(job.start());
        job.progress(4);
        job.progress(2); // never backwards
        job.progress(10);
        assert!(job.begin_finishing());
        assert!(matches!(
            job.cancel(),
            Err(CancelError::TooLate(UploadState::Finishing))
        ));
        job.finish(Err("boom".into()));
        job.remove_file().await;
        assert!(!job.path().exists());

        let mut seen = Vec::new();
        while let Ok(event) = events.try_recv() {
            assert_eq!((event.seq, event.kind.as_str()), (0, "upload"));
            let upload = &event.payload["upload"];
            seen.push((
                upload["state"].as_str().unwrap().to_string(),
                upload["sent"].as_u64().unwrap(),
            ));
        }
        let expected = [
            ("queued", 0),
            ("sending", 0),
            ("sending", 4),
            ("sending", 10),
            ("finishing", 10),
            ("failed", 10),
        ];
        assert_eq!(seen, expected.map(|(s, n)| (s.to_string(), n)));
        assert_eq!(uploads.pending(), 0);
        assert_eq!(uploads.list()[0].error, "boom");
    }

    #[tokio::test]
    async fn cancelling_wakes_the_sender() {
        let (uploads, _, _) = uploads();
        let file = uploads.receive(Body::from("x"), 100).await.unwrap();
        let job = uploads.register(file, "zalo:u:1", "a.txt", false);
        let waiter = {
            let job = job.clone();
            tokio::spawn(async move { job.cancelled().await })
        };
        tokio::task::yield_now().await;
        assert_eq!(job.cancel().unwrap().state, UploadState::Cancelled);
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap();
        assert!(!job.start(), "a cancelled upload never starts");
        job.finish(Err("late".into()));
        assert_eq!(job.state(), UploadState::Cancelled);
        assert!(uploads.get(&job.id()).is_some());
    }
}
