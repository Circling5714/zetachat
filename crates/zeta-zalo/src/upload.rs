//! Sending photos and files: a chunked upload to the file service, then a send.
//!
//! Ported from ctdozlite `send_attachment` (Zalo PC 26.10.11): photos go through
//! `photo_original/upload` then `photo_original/send`, other files through
//! `asyncfile/upload` then `asyncfile/msg`. A file can be sent only once Zalo has
//! stored it, which it announces on the socket (`cmd 601`, control `file_done`
//! with the file id and its URL, as handled by zca-js), so callers wait for that
//! event between [`ZaloClient::upload_file`] and [`ZaloClient::send_file`].
//!
//! Files are read from disk one part at a time, so a 1 GB file never sits in
//! memory. Part size, the largest file and the refused extensions are the
//! server's (`getServerInfo` → `setttings.features.sharefile`, see
//! [`FileLimits`]). Parts go up a few at a time; the last part goes alone once
//! all the others are stored, and its answer carries the file id.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::stream::{self, StreamExt, TryStreamExt};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::api::{first_str, Recipient};
use crate::client::ZaloClient;
use crate::domains::Service;
use crate::error::{Error, Result};
use crate::now_millis;

const MIB: u64 = 1024 * 1024;

/// Part size when the server says nothing (`chunk_size_file`, 3 MiB in 2026).
pub const DEFAULT_CHUNK_SIZE: u64 = 3 * MIB;

/// Parts uploaded at the same time.
const PARALLEL_PARTS: usize = 3;

/// Attempts per part when the network or the file service fails.
const PART_ATTEMPTS: u32 = 3;

/// Upload rules announced by the server (`setttings.features.sharefile`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileLimits {
    /// Largest file (`max_size_share_file_v3`, in MB: 1024).
    pub max_file_bytes: u64,
    /// Largest picture sent as a photo (`max_size_original_photo`, in MB: 100);
    /// larger pictures go as files.
    pub max_photo_bytes: u64,
    /// Size of one uploaded part (`chunk_size_file`).
    pub chunk_bytes: u64,
    /// Extensions Zalo refuses (`restricted_ext_file`), lower case, no dot.
    pub blocked_extensions: Vec<String>,
}

impl Default for FileLimits {
    fn default() -> Self {
        let blocked = [
            "exe", "cmd", "bat", "com", "lnk", "vbs", "msi", "vb", "ws", "wsf", "scf", "scr",
            "pif", "chm",
        ];
        Self {
            max_file_bytes: 1024 * MIB,
            max_photo_bytes: 100 * MIB,
            chunk_bytes: DEFAULT_CHUNK_SIZE,
            blocked_extensions: blocked.map(String::from).to_vec(),
        }
    }
}

impl FileLimits {
    /// The limits in a `getServerInfo` answer; what it lacks keeps its default.
    pub fn from_server_info(info: &Value) -> Self {
        let mut limits = Self::default();
        // Zalo spells it "setttings"; zca-js falls back to "settings" too.
        let share = info
            .get("setttings")
            .or_else(|| info.get("settings"))
            .and_then(|s| s.pointer("/features/sharefile"));
        let Some(share) = share else {
            return limits;
        };
        let megabytes = |key: &str| {
            share
                .get(key)
                .and_then(Value::as_u64)
                .filter(|mb| *mb > 0)
                .map(|mb| mb * MIB)
        };
        if let Some(bytes) = megabytes("max_size_share_file_v3") {
            limits.max_file_bytes = bytes;
        }
        if let Some(bytes) = megabytes("max_size_original_photo") {
            limits.max_photo_bytes = bytes;
        }
        if let Some(bytes) = share.get("chunk_size_file").and_then(Value::as_u64) {
            // Stay reasonable whatever the server says.
            limits.chunk_bytes = bytes.clamp(256 * 1024, 16 * MIB);
        }
        if let Some(list) = share.get("restricted_ext_file").and_then(Value::as_array) {
            let blocked: Vec<String> = list
                .iter()
                .filter_map(Value::as_str)
                .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
                .filter(|e| !e.is_empty())
                .collect();
            if !blocked.is_empty() {
                limits.blocked_extensions = blocked;
            }
        }
        limits
    }

    /// The extension of `file_name` when Zalo refuses it.
    pub fn blocked_extension(&self, file_name: &str) -> Option<String> {
        let ext = extension_of(file_name);
        (!ext.is_empty() && self.blocked_extensions.contains(&ext)).then_some(ext)
    }
}

/// A file on disk to upload.
#[derive(Clone, Copy, Debug)]
pub struct FileSource<'a> {
    pub path: &'a Path,
    pub size: u64,
    /// Name shown to the recipient.
    pub file_name: &'a str,
    pub mime: &'a str,
}

/// Told how many bytes Zalo has stored so far.
pub type Progress<'a> = &'a (dyn Fn(u64) + Send + Sync);

/// A photo Zalo stored but that is not sent yet.
#[derive(Clone, Debug)]
pub struct UploadedPhoto {
    pub photo_id: String,
    pub client_id: u64,
    pub size: u64,
    pub normal_url: String,
    pub hd_url: String,
    pub thumb_url: String,
}

/// A photo Zalo accepted.
#[derive(Clone, Debug)]
pub struct SentPhoto {
    /// Server message id, empty if the answer had none.
    pub msg_id: String,
    /// Client id of the message, to match the server echo.
    pub client_id: u64,
    pub normal_url: String,
    pub hd_url: String,
    pub thumb_url: String,
}

/// A file Zalo stored but that is not sent yet.
#[derive(Clone, Debug)]
pub struct UploadedFile {
    pub file_id: String,
    pub client_id: u64,
    pub file_name: String,
    pub size: u64,
    /// MD5 of the content, hex.
    pub checksum: String,
    /// Part size of the upload, to send its last part again.
    pub part_size: u64,
}

/// A file message Zalo accepted.
#[derive(Clone, Debug)]
pub struct SentFile {
    pub msg_id: String,
    pub client_id: u64,
}

/// API scope, `type` query value and recipient field of a target.
fn target(to: Recipient<'_>) -> (&'static str, u8, &'static str, String) {
    match to {
        Recipient::User(uid) => ("message", 2, "toid", uid.to_string()),
        Recipient::Group(grid) => ("group", 11, "grid", grid.to_string()),
    }
}

/// Ids are numbers in the official client; keep anything else as a string.
fn id_value(id: &str) -> Value {
    id.parse::<u64>()
        .map(Value::from)
        .unwrap_or_else(|_| json!(id))
}

fn or_else(value: String, fallback: &str) -> String {
    if value.is_empty() {
        fallback.to_string()
    } else {
        value
    }
}

fn extension_of(file_name: &str) -> String {
    file_name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default()
}

/// An id from an upload answer; parts that do not complete the file answer `-1`.
fn upload_id(answer: &Value, key: &str) -> String {
    let id = first_str(answer, &[key]);
    if id == "-1" {
        String::new()
    } else {
        id
    }
}

/// Worth sending the same part again.
fn is_transient(e: &Error) -> bool {
    match e {
        Error::Network(_) => true,
        Error::Status { status, .. } => *status == 429 || *status >= 500,
        _ => false,
    }
}

/// Bytes `offset..offset + len` of a file.
async fn read_part(path: &Path, offset: u64, len: u64) -> Result<Vec<u8>> {
    let mut file = tokio::fs::File::open(path).await?;
    file.seek(std::io::SeekFrom::Start(offset)).await?;
    let mut buf = vec![0u8; len as usize];
    file.read_exact(&mut buf).await?;
    Ok(buf)
}

/// One upload in progress: where it goes and how it is cut.
#[derive(Clone, Copy)]
struct Parts<'a> {
    to: Recipient<'a>,
    /// `photo_original` or `asyncfile`.
    kind: &'a str,
    src: FileSource<'a>,
    client_id: u64,
    part_size: u64,
}

impl Parts<'_> {
    fn count(&self) -> u64 {
        self.src.size.div_ceil(self.part_size)
    }

    fn is_photo(&self) -> bool {
        self.kind == "photo_original"
    }
}

impl ZaloClient {
    /// Upload limits in force (the server's once known, see
    /// [`ZaloClient::refresh_file_limits`]).
    pub fn file_limits(&self) -> &FileLimits {
        &self.limits
    }

    /// Ask the server for its current upload limits. Works without a session.
    pub async fn refresh_file_limits(&mut self) -> Result<()> {
        let info = self.server_info().await?;
        self.limits = FileLimits::from_server_info(&info);
        Ok(())
    }

    /// Upload part `index` (1-based); returns its length and Zalo's answer.
    async fn upload_part(&self, parts: Parts<'_>, index: u64) -> Result<(u64, Value)> {
        let key = self.session_key()?;
        let total = parts.count();
        let offset = (index - 1) * parts.part_size;
        let len = parts.part_size.min(parts.src.size - offset);
        let bytes = read_part(parts.src.path, offset, len).await?;

        let (scope, kind_code, target_key, target_id) = target(parts.to);
        let mut params = Map::new();
        params.insert("totalChunk".into(), json!(total));
        params.insert("fileName".into(), json!(parts.src.file_name));
        params.insert("clientId".into(), json!(parts.client_id));
        params.insert("totalSize".into(), json!(parts.src.size));
        params.insert("imei".into(), json!(self.device().imei));
        params.insert("isE2EE".into(), json!(0));
        if parts.is_photo() {
            params.insert("jxl".into(), json!(0));
        }
        params.insert("chunkId".into(), json!(index));
        params.insert(target_key.into(), json!(target_id));
        let url = format!(
            "{}/api/{scope}/{}/upload?{}&params={}&type={kind_code}",
            self.domains().primary(Service::File),
            parts.kind,
            Self::common_params(),
            self.encrypt_params(&Value::Object(params))?
        );

        let mut attempt = 1;
        loop {
            let sent = self
                .post_multipart(
                    &url,
                    "chunkContent",
                    parts.src.file_name,
                    parts.src.mime,
                    &bytes,
                    Some(key),
                )
                .await;
            match sent {
                Ok(answer) => return Ok((len, answer)),
                Err(e) if attempt < PART_ATTEMPTS && is_transient(&e) => {
                    tracing::debug!(part = index, attempt, error = %e, "sending a part again");
                    tokio::time::sleep(Duration::from_secs(u64::from(attempt * attempt))).await;
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Upload a whole file; returns the answer to the last part, which carries
    /// the ids and URLs.
    async fn upload_parts(&self, parts: Parts<'_>, progress: Progress<'_>) -> Result<Value> {
        if parts.src.size == 0 {
            return Err(Error::Protocol("cannot upload an empty file".into()));
        }
        let total = parts.count();
        let stored = AtomicU64::new(0);
        stream::iter(1..total)
            .map(|index| self.upload_part(parts, index))
            .buffer_unordered(PARALLEL_PARTS)
            .try_for_each(|(len, _)| {
                progress(stored.fetch_add(len, Ordering::Relaxed) + len);
                async { Ok(()) }
            })
            .await?;
        let (_, last) = self.upload_part(parts, total).await?;
        progress(parts.src.size);
        Ok(last)
    }

    /// Upload a photo; send it with [`ZaloClient::send_uploaded_photo`].
    pub async fn upload_photo(
        &self,
        to: Recipient<'_>,
        src: FileSource<'_>,
        progress: Progress<'_>,
    ) -> Result<UploadedPhoto> {
        let parts = Parts {
            to,
            kind: "photo_original",
            src,
            client_id: now_millis(),
            part_size: self.limits.chunk_bytes,
        };
        let uploaded = self.upload_parts(parts, progress).await?;
        let photo_id = upload_id(&uploaded, "photoId");
        let normal_url = first_str(&uploaded, &["normalUrl", "hdUrl", "oriUrl", "url"]);
        if photo_id.is_empty() || normal_url.is_empty() {
            return Err(Error::Protocol(
                "photo upload answered without photoId or URL".into(),
            ));
        }
        Ok(UploadedPhoto {
            photo_id,
            client_id: parts.client_id,
            size: src.size,
            hd_url: or_else(first_str(&uploaded, &["hdUrl"]), &normal_url),
            thumb_url: or_else(first_str(&uploaded, &["thumbUrl"]), &normal_url),
            normal_url,
        })
    }

    /// Send an uploaded photo, `width` × `height`, with an optional caption.
    pub async fn send_uploaded_photo(
        &self,
        to: Recipient<'_>,
        photo: &UploadedPhoto,
        width: u32,
        height: u32,
        caption: &str,
    ) -> Result<SentPhoto> {
        let key = self.session_key()?;
        let (scope, _, target_key, target_id) = target(to);
        let UploadedPhoto {
            photo_id,
            client_id,
            size,
            normal_url,
            hd_url,
            thumb_url,
        } = photo.clone();
        let mut params = Map::new();
        params.insert("photoId".into(), id_value(&photo_id));
        params.insert("clientId".into(), json!(client_id));
        params.insert("desc".into(), json!(caption));
        params.insert("width".into(), json!(width));
        params.insert("height".into(), json!(height));
        params.insert("rawUrl".into(), json!(normal_url));
        params.insert("hdUrl".into(), json!(hd_url));
        params.insert("thumbUrl".into(), json!(thumb_url));
        params.insert("oriUrl".into(), json!(normal_url));
        params.insert("normalUrl".into(), json!(normal_url));
        params.insert("fileSize".into(), json!(size));
        params.insert("hdSize".into(), json!(size));
        params.insert("zsource".into(), json!(-1));
        params.insert("jcp".into(), json!("{\"convertible\":true}"));
        params.insert("imei".into(), json!(self.device().imei));
        params.insert(target_key.into(), json!(target_id));
        if matches!(to, Recipient::Group(_)) {
            params.insert("visibility".into(), json!(0));
        }
        let url = format!(
            "{}/api/{scope}/photo_original/send?{}",
            self.domains().primary(Service::File),
            Self::common_params()
        );
        let form = format!("params={}", self.encrypt_params(&Value::Object(params))?);
        let data = self.post_form(&url, form, Some(key)).await?;
        Ok(SentPhoto {
            msg_id: first_str(&data, &["msgId"]),
            client_id,
            normal_url,
            hd_url,
            thumb_url,
        })
    }

    /// Upload a file whose MD5 is `checksum`. Zalo then announces it on the
    /// socket (see [`file_done_events`]); send it with [`ZaloClient::send_file`]
    /// and the URL from that announcement.
    pub async fn upload_file(
        &self,
        to: Recipient<'_>,
        src: FileSource<'_>,
        checksum: &str,
        progress: Progress<'_>,
    ) -> Result<UploadedFile> {
        let parts = Parts {
            to,
            kind: "asyncfile",
            src,
            client_id: now_millis(),
            part_size: self.limits.chunk_bytes,
        };
        let uploaded = self.upload_parts(parts, progress).await?;
        let file_id = upload_id(&uploaded, "fileId");
        if file_id.is_empty() {
            return Err(Error::Protocol(
                "file upload answered without fileId".into(),
            ));
        }
        Ok(UploadedFile {
            file_id,
            client_id: parts.client_id,
            file_name: src.file_name.to_string(),
            size: src.size,
            checksum: checksum.to_string(),
            part_size: parts.part_size,
        })
    }

    /// Send the last part of an uploaded file again, as Zalo PC does when the
    /// `file_done` announcement is late (`retry_upload_file_configs`). Returns
    /// the file id of the answer, empty if it had none.
    pub async fn resend_last_part(
        &self,
        to: Recipient<'_>,
        src: FileSource<'_>,
        file: &UploadedFile,
    ) -> Result<String> {
        let parts = Parts {
            to,
            kind: "asyncfile",
            src,
            client_id: file.client_id,
            part_size: file.part_size,
        };
        let (_, answer) = self.upload_part(parts, parts.count()).await?;
        Ok(upload_id(&answer, "fileId"))
    }

    /// Send a stored file as a message.
    pub async fn send_file(
        &self,
        to: Recipient<'_>,
        file: &UploadedFile,
        file_url: &str,
    ) -> Result<SentFile> {
        let key = self.session_key()?;
        let (scope, _, target_key, target_id) = target(to);
        let extension = extension_of(&file.file_name);
        let mut params = Map::new();
        params.insert("fileId".into(), id_value(&file.file_id));
        params.insert("checksum".into(), json!(file.checksum));
        params.insert("checksumSha".into(), json!(""));
        // The official client spells it "extention"; ctdozlite sends "extension".
        params.insert("extention".into(), json!(extension));
        params.insert("extension".into(), json!(extension));
        params.insert("totalSize".into(), json!(file.size));
        params.insert("fileName".into(), json!(file.file_name));
        params.insert("clientId".into(), json!(file.client_id));
        params.insert("fType".into(), json!(1));
        params.insert("fileCount".into(), json!(0));
        params.insert("fdata".into(), json!("{}"));
        params.insert("fileUrl".into(), json!(file_url));
        params.insert("zsource".into(), json!(401));
        params.insert("imei".into(), json!(self.device().imei));
        params.insert(target_key.into(), json!(target_id));
        let url = format!(
            "{}/api/{scope}/asyncfile/msg?{}",
            self.domains().primary(Service::File),
            Self::common_params()
        );
        let form = format!("params={}", self.encrypt_params(&Value::Object(params))?);
        let data = self.post_form(&url, form, Some(key)).await?;
        Ok(SentFile {
            msg_id: first_str(&data, &["msgId"]),
            client_id: file.client_id,
        })
    }
}

/// `file_done` announcements in a control frame (`cmd 601`): file id and URL of
/// each file Zalo finished storing.
pub fn file_done_events(payload: &Value) -> Vec<(String, String)> {
    let data = payload
        .get("data")
        .filter(|d| d.is_object())
        .unwrap_or(payload);
    let Some(controls) = data.get("controls").and_then(Value::as_array) else {
        return Vec::new();
    };
    controls
        .iter()
        .filter_map(|control| {
            let content = control.get("content")?;
            if content.get("act_type").and_then(Value::as_str) != Some("file_done") {
                return None;
            }
            let id = first_str(content, &["fileId"]);
            let url = content
                .pointer("/data/url")
                .and_then(Value::as_str)
                .unwrap_or_default();
            (!id.is_empty() && !url.is_empty()).then(|| (id, url.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_done_announcements_are_found() {
        let payload = json!({ "data": { "controls": [
            { "content": { "act_type": "typing" } },
            { "content": { "act_type": "file_done", "fileId": 777,
                           "data": { "url": "https://f.zdn.vn/a.pdf" } } },
            { "content": { "act_type": "file_done", "fileId": "" } }
        ]}});
        assert_eq!(
            file_done_events(&payload),
            [("777".to_string(), "https://f.zdn.vn/a.pdf".to_string())]
        );
        assert!(file_done_events(&json!({ "data": { "msgs": [] } })).is_empty());
        assert!(file_done_events(&Value::Null).is_empty());
    }

    #[test]
    fn targets_and_ids() {
        assert_eq!(
            target(Recipient::User("5")),
            ("message", 2, "toid", "5".into())
        );
        assert_eq!(
            target(Recipient::Group("9")),
            ("group", 11, "grid", "9".into())
        );
        assert_eq!(id_value("123"), json!(123));
        assert_eq!(id_value("a1"), json!("a1"));
        assert_eq!(extension_of("Báo cáo.PDF"), "pdf");
        assert_eq!(extension_of("README"), "");
        assert_eq!(upload_id(&json!({ "fileId": -1 }), "fileId"), "");
        assert_eq!(upload_id(&json!({ "fileId": 42 }), "fileId"), "42");
    }

    #[test]
    fn limits_come_from_the_server_settings() {
        // The shape of `getServerInfo` (November 2026), typo included.
        let info = json!({ "setttings": { "features": { "sharefile": {
            "chunk_size_file": 3145728,
            "max_size_share_file_v3": 1024,
            "max_size_original_photo": 100,
            "restricted_ext_file": ["exe", ".BAT", ""]
        }}}});
        let limits = FileLimits::from_server_info(&info);
        assert_eq!(limits.chunk_bytes, 3 * MIB);
        assert_eq!(limits.max_file_bytes, 1024 * MIB);
        assert_eq!(limits.max_photo_bytes, 100 * MIB);
        assert_eq!(limits.blocked_extensions, ["exe", "bat"]);
        assert_eq!(
            limits.blocked_extension("cài đặt.EXE").as_deref(),
            Some("exe")
        );
        assert_eq!(limits.blocked_extension("a.zip"), None);
        assert_eq!(limits.blocked_extension("exe"), None);

        // Nothing usable: defaults; absurd part sizes are clamped.
        assert_eq!(
            FileLimits::from_server_info(&json!({})),
            FileLimits::default()
        );
        let tiny = json!({ "settings": { "features": { "sharefile": { "chunk_size_file": 1 }}}});
        assert_eq!(FileLimits::from_server_info(&tiny).chunk_bytes, 256 * 1024);
    }

    #[tokio::test]
    async fn parts_are_read_from_disk() {
        let path = std::env::temp_dir().join(format!("zeta-part-{:x}", rand::random::<u64>()));
        tokio::fs::write(&path, b"0123456789").await.unwrap();
        assert_eq!(read_part(&path, 0, 4).await.unwrap(), b"0123");
        assert_eq!(read_part(&path, 8, 2).await.unwrap(), b"89");
        assert!(read_part(&path, 8, 4).await.is_err(), "past the end");
        let src = FileSource {
            path: &path,
            size: 10,
            file_name: "a",
            mime: "text/plain",
        };
        let parts = Parts {
            to: Recipient::User("1"),
            kind: "asyncfile",
            src,
            client_id: 1,
            part_size: 4,
        };
        assert_eq!(parts.count(), 3);
        assert!(!parts.is_photo());
        let _ = tokio::fs::remove_file(&path).await;
    }
}
