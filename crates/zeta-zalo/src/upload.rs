//! Sending photos and files: a chunked upload to the file service, then a send.
//!
//! Ported from ctdozlite `send_attachment` (Zalo PC 26.10.11): photos go through
//! `photo_original/upload` then `photo_original/send`, other files through
//! `asyncfile/upload` then `asyncfile/msg`. A file can be sent only once Zalo has
//! stored it, which it announces on the socket (`cmd 601`, control `file_done`
//! with the file id and its URL, as handled by zca-js), so callers wait for that
//! event between [`ZaloClient::upload_file`] and [`ZaloClient::send_file`].

use serde_json::{json, Map, Value};

use crate::api::{first_str, Recipient};
use crate::client::ZaloClient;
use crate::crypto;
use crate::domains::Service;
use crate::error::{Error, Result};
use crate::now_millis;

/// Size of one uploaded part.
pub const CHUNK_SIZE: usize = 1024 * 1024;

/// A photo to send.
#[derive(Clone, Copy, Debug)]
pub struct Photo<'a> {
    pub bytes: &'a [u8],
    pub file_name: &'a str,
    pub mime: &'a str,
    pub width: u32,
    pub height: u32,
    /// Shown under the photo; may be empty.
    pub caption: &'a str,
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

impl ZaloClient {
    /// Upload `bytes` in parts to `{file}/api/{scope}/{kind}/upload`; returns the
    /// answer to the last part, which carries the ids and URLs.
    #[allow(clippy::too_many_arguments)]
    async fn upload_parts(
        &self,
        to: Recipient<'_>,
        kind: &str,
        bytes: &[u8],
        file_name: &str,
        mime: &str,
        client_id: u64,
        photo: bool,
    ) -> Result<Value> {
        if bytes.is_empty() {
            return Err(Error::Protocol("cannot upload an empty file".into()));
        }
        let key = self.session_key()?;
        let (scope, kind_code, target_key, target_id) = target(to);
        let total = bytes.len().div_ceil(CHUNK_SIZE);
        let mut last = Value::Null;
        for (i, chunk) in bytes.chunks(CHUNK_SIZE).enumerate() {
            let mut params = Map::new();
            params.insert("totalChunk".into(), json!(total));
            params.insert("fileName".into(), json!(file_name));
            params.insert("clientId".into(), json!(client_id));
            params.insert("totalSize".into(), json!(bytes.len()));
            params.insert("imei".into(), json!(self.device().imei));
            params.insert("isE2EE".into(), json!(0));
            if photo {
                params.insert("jxl".into(), json!(0));
            }
            params.insert("chunkId".into(), json!(i + 1));
            params.insert(target_key.into(), json!(target_id));
            let url = format!(
                "{}/api/{scope}/{kind}/upload?{}&params={}&type={kind_code}",
                self.domains().primary(Service::File),
                Self::common_params(),
                self.encrypt_params(&Value::Object(params))?
            );
            last = self
                .post_multipart(&url, "chunkContent", file_name, mime, chunk, Some(key))
                .await?;
        }
        Ok(last)
    }

    /// Upload and send a photo.
    pub async fn send_photo(&self, to: Recipient<'_>, photo: Photo<'_>) -> Result<SentPhoto> {
        let key = self.session_key()?;
        let client_id = now_millis();
        let uploaded = self
            .upload_parts(
                to,
                "photo_original",
                photo.bytes,
                photo.file_name,
                photo.mime,
                client_id,
                true,
            )
            .await?;
        let photo_id = first_str(&uploaded, &["photoId"]);
        let normal_url = first_str(&uploaded, &["normalUrl", "hdUrl", "oriUrl", "url"]);
        if photo_id.is_empty() || normal_url.is_empty() {
            return Err(Error::Protocol(
                "photo upload answered without photoId or URL".into(),
            ));
        }
        let hd_url = or_else(first_str(&uploaded, &["hdUrl"]), &normal_url);
        let thumb_url = or_else(first_str(&uploaded, &["thumbUrl"]), &normal_url);

        let (scope, _, target_key, target_id) = target(to);
        let size = photo.bytes.len();
        let mut params = Map::new();
        params.insert("photoId".into(), id_value(&photo_id));
        params.insert("clientId".into(), json!(client_id));
        params.insert("desc".into(), json!(photo.caption));
        params.insert("width".into(), json!(photo.width));
        params.insert("height".into(), json!(photo.height));
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

    /// Upload a file. Zalo then announces it on the socket (see
    /// [`file_done_events`]); send it with [`ZaloClient::send_file`] and the URL
    /// from that announcement.
    pub async fn upload_file(
        &self,
        to: Recipient<'_>,
        bytes: &[u8],
        file_name: &str,
        mime: &str,
    ) -> Result<UploadedFile> {
        let client_id = now_millis();
        let uploaded = self
            .upload_parts(to, "asyncfile", bytes, file_name, mime, client_id, false)
            .await?;
        let file_id = first_str(&uploaded, &["fileId"]);
        if file_id.is_empty() {
            return Err(Error::Protocol(
                "file upload answered without fileId".into(),
            ));
        }
        Ok(UploadedFile {
            file_id,
            client_id,
            file_name: file_name.to_string(),
            size: bytes.len() as u64,
            checksum: crypto::md5_hex_bytes(bytes),
        })
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
    }
}
