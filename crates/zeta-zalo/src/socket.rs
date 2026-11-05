//! Real-time transport over WebSocket (`zpw_ws`).
//!
//! Ported from ctdozlite `src/socket.rs` (MIT OR Apache-2.0), where it was decoded
//! from the Zalo PC bundle:
//! * frame: `u8 version | u16 LE cmd | u8 subCmd | JSON body`;
//! * the server speaks first with `cmd 1` (authentication) carrying an AES-GCM key;
//! * `encrypt`: 0 plain, 1 base64 + deflate, 2 AES-GCM + deflate, 3 AES-GCM, where
//!   an AES-GCM blob is `iv(16) | aad(16) | ciphertext+tag`;
//! * after authentication the client pulls the queues listed in `ctrl_qCmds` /
//!   `qCmds`; pulling a queue the server did not list closes the socket;
//! * keep-alive ping every 15 seconds.

use std::io::Read;
use std::time::{Duration, Instant};

use aes_gcm::aead::consts::U16;
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::AesGcm;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::{COOKIE, ORIGIN, USER_AGENT as UA_HEADER};
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

use crate::client::ZaloClient;
use crate::error::{Error, Result};
use crate::{now_millis, USER_AGENT};

/// Command ids (`cmd`) seen on the socket.
pub mod cmd {
    pub const AUTHEN: u16 = 1;
    pub const PING: u16 = 2;
    pub const PUSH_MSG_USER: u16 = 501;
    pub const PULL_MSG_USER: u16 = 510;
    pub const PULL_MSG_GROUP: u16 = 511;
    pub const PUSH_MSG_GROUP: u16 = 521;
    pub const HAS_MSG: u16 = 533;
    pub const PUSH_MISS_MSG: u16 = 534;
    pub const GET_MSG: u16 = 570;
    pub const CTRL_PUSH: u16 = 601;
    pub const CTRL_ACTION: u16 = 602;
    pub const CTRL_PULL: u16 = 603;
    pub const CTRL_PULL_VOIP: u16 = 604;
}

const SUB_KEEP_CONNECTION: u8 = 1;
const PING_INTERVAL: Duration = Duration::from_secs(15);
const MAX_BACKOFF_SECS: u64 = 30;

/// One decoded socket frame.
#[derive(Clone, Debug)]
pub struct WsEvent {
    pub cmd: u16,
    pub sub_cmd: u8,
    pub version: u8,
    /// Decrypted body when possible, otherwise the raw envelope.
    pub payload: Value,
}

impl WsEvent {
    /// Chat message frames, pushed or pulled.
    pub fn is_message(&self) -> bool {
        matches!(
            self.cmd,
            501 | 510 | 511 | 513 | 514 | 515 | 517 | 518 | 521 | 534 | 570
        )
    }

    /// Control frames (reactions, recalls, seen, typing, call invites…).
    pub fn is_control(&self) -> bool {
        (cmd::CTRL_PUSH..=cmd::CTRL_PULL_VOIP).contains(&self.cmd)
    }
}

/// Keys, types and array sizes of a payload, never its values: safe to log
/// while learning the shape of frames. Keys that look like ids are masked.
pub fn payload_shape(v: &Value) -> String {
    fn walk(v: &Value, depth: usize, out: &mut String) {
        match v {
            Value::Object(map) if depth < 8 => {
                out.push('{');
                for (i, (key, value)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    let id_like = key.len() >= 6 && key.bytes().all(|b| b.is_ascii_digit());
                    out.push_str(if id_like { "<id>" } else { key });
                    out.push_str(": ");
                    walk(value, depth + 1, out);
                }
                out.push('}');
            }
            Value::Object(map) => out.push_str(&format!("{{{} keys}}", map.len())),
            Value::Array(items) => match items.first() {
                Some(first) => {
                    out.push_str(&format!("[{} × ", items.len()));
                    walk(first, depth + 1, out);
                    out.push(']');
                }
                None => out.push_str("[]"),
            },
            // JSON inside a string (Zalo nests some contents this way).
            Value::String(s) if s.starts_with('{') => match serde_json::from_str::<Value>(s) {
                Ok(inner) => {
                    out.push_str("json");
                    walk(&inner, depth + 1, out);
                }
                Err(_) => out.push_str("str"),
            },
            Value::String(_) => out.push_str("str"),
            Value::Number(_) => out.push_str("num"),
            Value::Bool(_) => out.push_str("bool"),
            Value::Null => out.push_str("null"),
        }
    }
    let mut out = String::new();
    walk(v, 0, &mut out);
    out
}

/// What the real-time loop reports.
#[derive(Clone, Debug)]
pub enum RealtimeEvent {
    /// Authenticated on an endpoint.
    Connected { endpoint: String },
    /// A decoded frame.
    Frame(WsEvent),
    /// The connection ended; the loop reconnects by itself.
    Disconnected { reason: String },
    /// Zalo refused the session. The loop stops; a new login is needed.
    SessionRejected,
}

/// Encode one frame.
pub fn encode_frame(cmd: u16, sub_cmd: u8, version: u8, body: Option<&Value>) -> Vec<u8> {
    let body = body.map(Value::to_string).unwrap_or_default();
    let mut out = Vec::with_capacity(4 + body.len());
    out.push(version);
    out.extend_from_slice(&cmd.to_le_bytes());
    out.push(sub_cmd);
    out.extend_from_slice(body.as_bytes());
    out
}

/// Decode one frame into `(version, cmd, subCmd, body)`.
pub fn decode_frame(buf: &[u8]) -> Option<(u8, u16, u8, Value)> {
    if buf.len() < 4 {
        return None;
    }
    let body = &buf[4..];
    let value = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(body).ok()?
    };
    Some((buf[0], u16::from_le_bytes([buf[1], buf[2]]), buf[3], value))
}

/// `decodeURIComponent` that leaves raw base64 characters alone.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Inflate a payload: gzip (what the server sends today), zlib or raw deflate.
fn inflate(buf: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let ok = if buf.starts_with(&[0x1f, 0x8b]) {
        flate2::read::GzDecoder::new(buf)
            .read_to_end(&mut out)
            .is_ok()
    } else {
        flate2::read::ZlibDecoder::new(buf)
            .read_to_end(&mut out)
            .is_ok()
    };
    if ok && !out.is_empty() {
        return Ok(out);
    }
    out.clear();
    flate2::read::DeflateDecoder::new(buf)
        .read_to_end(&mut out)
        .map_err(|e| Error::Protocol(format!("cannot inflate socket payload: {e}")))?;
    if out.is_empty() {
        return Err(Error::Protocol("empty socket payload after inflate".into()));
    }
    Ok(out)
}

/// AES-GCM with a 16-byte nonce: `iv(16) | aad(16) | ciphertext+tag(16)`.
fn gcm_decrypt(key: &[u8], blob: &[u8]) -> Result<Vec<u8>> {
    if blob.len() < 48 {
        return Err(Error::Crypto(format!(
            "AES-GCM blob too short ({} bytes)",
            blob.len()
        )));
    }
    let nonce = aes_gcm::Nonce::<U16>::from_slice(&blob[..16]);
    let payload = Payload {
        msg: &blob[32..],
        aad: &blob[16..32],
    };
    let key_err = |_| Error::Crypto("invalid AES-GCM key".into());
    let plain = match key.len() {
        32 => AesGcm::<aes::Aes256, U16>::new_from_slice(key)
            .map_err(key_err)?
            .decrypt(nonce, payload),
        16 => AesGcm::<aes::Aes128, U16>::new_from_slice(key)
            .map_err(key_err)?
            .decrypt(nonce, payload),
        n => return Err(Error::Crypto(format!("unsupported AES-GCM key length {n}"))),
    };
    plain.map_err(|_| Error::Crypto("AES-GCM authentication failed".into()))
}

/// Apply the envelope's `encrypt` scheme to its `data` field.
pub fn decrypt_payload(key: Option<&[u8]>, encrypt: u8, data: &str) -> Result<Vec<u8>> {
    let need_key = || key.ok_or_else(|| Error::Crypto("no socket key yet".into()));
    let b64 = |s: &str| {
        B64.decode(s)
            .map_err(|e| Error::Crypto(format!("socket base64: {e}")))
    };
    match encrypt {
        0 => Ok(data.as_bytes().to_vec()),
        1 => inflate(&b64(data)?),
        2 => inflate(&gcm_decrypt(need_key()?, &b64(&percent_decode(data))?)?),
        3 => gcm_decrypt(need_key()?, &b64(&percent_decode(data))?),
        other => Err(Error::Protocol(format!(
            "unsupported socket encrypt type {other}"
        ))),
    }
}

/// Queues the server told us to pull, deduplicated, in order.
fn queues_from_auth(auth: &Value) -> Vec<(u16, u8)> {
    let mut out: Vec<(u16, u8)> = Vec::new();
    for key in ["ctrl_qCmds", "qCmds"] {
        let Some(items) = auth
            .pointer(&format!("/data/{key}"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        for q in items {
            let Some(c) = q.get("cmd").and_then(Value::as_u64) else {
                continue;
            };
            let sub = q.get("subCmd").and_then(Value::as_u64).unwrap_or(0);
            let pair = (c as u16, sub as u8);
            if !out.contains(&pair) {
                out.push(pair);
            }
        }
    }
    out
}

fn pull_frame(
    cmd: u16,
    sub_cmd: u8,
    req_id: &str,
    first: bool,
    last_id: Value,
    pre_ids: Value,
) -> Vec<u8> {
    let body = json!({ "first": first, "reqId": req_id, "lastId": last_id, "preIds": pre_ids });
    encode_frame(cmd, sub_cmd, 1, Some(&body))
}

/// Socket URL: explicit `/` path (the front end answers `501` without one), the
/// common parameters and a timestamp.
fn build_url(base: &str) -> String {
    let (host, query) = match base.split_once('?') {
        Some((h, q)) => (h, Some(q)),
        None => (base, None),
    };
    let after_scheme = host.split_once("://").map(|(_, rest)| rest).unwrap_or(host);
    let mut url = host.to_string();
    if !after_scheme.contains('/') {
        url.push('/');
    }
    url.push('?');
    if let Some(q) = query {
        url.push_str(q);
        url.push('&');
    }
    url.push_str(&format!(
        "{}&t={}",
        ZaloClient::common_params(),
        now_millis()
    ));
    url
}

/// Settings for [`spawn`], taken from a logged-in client.
#[derive(Clone)]
pub struct RealtimeConfig {
    pub endpoints: Vec<String>,
    zpw_sek: String,
}

impl std::fmt::Debug for RealtimeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealtimeConfig")
            .field("endpoints", &self.endpoints)
            .field("zpw_sek", &"<redacted>")
            .finish()
    }
}

impl RealtimeConfig {
    pub fn from_client(client: &ZaloClient) -> Result<Self> {
        let session = client.session().ok_or(Error::NotLoggedIn)?;
        Ok(Self {
            endpoints: client.domains().ws.clone(),
            zpw_sek: session.zpw_sek.clone(),
        })
    }
}

fn ws_err(e: impl std::fmt::Display) -> Error {
    Error::WebSocket(e.to_string())
}

/// Run the real-time loop in the background. It reconnects with capped backoff and
/// stops when Zalo rejects the session or when the receiver is dropped.
pub fn spawn(cfg: RealtimeConfig) -> mpsc::UnboundedReceiver<RealtimeEvent> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut backoff = 2u64;
        let mut endpoint_idx = 0usize;
        loop {
            let started = Instant::now();
            let reason = match run_once(&cfg, &tx, endpoint_idx).await {
                Ok(reason) => reason,
                Err(e) if e.is_session_rejected() => {
                    let _ = tx.send(RealtimeEvent::SessionRejected);
                    break;
                }
                Err(e) => e.to_string(),
            };
            if tx.send(RealtimeEvent::Disconnected { reason }).is_err() {
                break;
            }
            // A connection that lived a while was healthy: start over gently.
            if started.elapsed() > Duration::from_secs(60) {
                backoff = 2;
            }
            endpoint_idx = endpoint_idx.wrapping_add(1);
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            backoff = (backoff * 2).min(MAX_BACKOFF_SECS);
        }
    });
    rx
}

async fn run_once(
    cfg: &RealtimeConfig,
    tx: &mpsc::UnboundedSender<RealtimeEvent>,
    endpoint_idx: usize,
) -> Result<String> {
    let base = if cfg.endpoints.is_empty() {
        "wss://ws1-msg.chat.zalo.me".to_string()
    } else {
        cfg.endpoints[endpoint_idx % cfg.endpoints.len()].clone()
    };
    let mut request = build_url(&base).into_client_request().map_err(ws_err)?;
    {
        let headers = request.headers_mut();
        let cookie = HeaderValue::from_str(&format!("zpw_sek={}; _zlang=vi", cfg.zpw_sek))
            .map_err(ws_err)?;
        headers.insert(COOKIE, cookie);
        headers.insert(ORIGIN, HeaderValue::from_static("https://chat.zalo.me"));
        headers.insert(UA_HEADER, HeaderValue::from_static(USER_AGENT));
    }
    let (mut ws, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(ws_err)?;

    let mut cipher_key: Option<Vec<u8>> = None;
    let mut authed = false;
    let mut req_id = 0u64;
    let mut next_req = || {
        req_id += 1;
        format!("req_{req_id}")
    };
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ping.tick().await;

    loop {
        tokio::select! {
            _ = ping.tick() => {
                if authed {
                    let frame = encode_frame(cmd::PING, SUB_KEEP_CONNECTION, 1, Some(&json!({ "eventId": now_millis() })));
                    ws.send(Message::Binary(frame)).await.map_err(ws_err)?;
                }
            }
            incoming = ws.next() => {
                let Some(incoming) = incoming else {
                    return Ok("closed by server".into());
                };
                let buf = match incoming.map_err(ws_err)? {
                    Message::Binary(b) => b,
                    Message::Ping(p) => {
                        ws.send(Message::Pong(p)).await.map_err(ws_err)?;
                        continue;
                    }
                    Message::Close(c) => return Ok(format!("close {c:?}")),
                    _ => continue,
                };
                let Some((version, cmd_id, sub_cmd, envelope)) = decode_frame(&buf) else {
                    continue;
                };
                let encrypt = envelope.get("encrypt").and_then(Value::as_u64).unwrap_or(0) as u8;

                if cmd_id == cmd::AUTHEN {
                    if let Some(k) = envelope.get("key").and_then(Value::as_str).filter(|k| !k.is_empty()) {
                        cipher_key = Some(B64.decode(k).map_err(|e| Error::Crypto(format!("socket key: {e}")))?);
                    }
                    let raw = envelope.get("data").and_then(Value::as_str).unwrap_or_default();
                    let auth: Value = serde_json::from_slice(&decrypt_payload(cipher_key.as_deref(), encrypt, raw)?)?;
                    let code = auth.get("error_code").and_then(Value::as_i64).unwrap_or(-1);
                    if code != 0 {
                        let message = auth.get("error_message").and_then(Value::as_str).unwrap_or("socket authentication failed");
                        return Err(Error::from_code(code, message));
                    }
                    authed = true;
                    let _ = tx.send(RealtimeEvent::Connected { endpoint: base.clone() });
                    for (q, s) in queues_from_auth(&auth) {
                        let frame = pull_frame(q, s, &next_req(), true, json!(1), json!([]));
                        ws.send(Message::Binary(frame)).await.map_err(ws_err)?;
                    }
                    continue;
                }

                let payload = match envelope.get("data").and_then(Value::as_str) {
                    Some(d) if encrypt != 0 => decrypt_payload(cipher_key.as_deref(), encrypt, d)
                        .ok()
                        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                        .unwrap_or_else(|| envelope.clone()),
                    _ => envelope.clone(),
                };

                // "There are messages": fetch them.
                if cmd_id == cmd::HAS_MSG {
                    if let (Some(qid), Some(ids)) = (payload.pointer("/data/queueId"), payload.pointer("/data/ids")) {
                        let body = json!({ "queueId": qid, "ids": ids, "lastId": "1", "reqId": next_req() });
                        ws.send(Message::Binary(encode_frame(cmd::GET_MSG, 0, 1, Some(&body)))).await.map_err(ws_err)?;
                    }
                }
                // A paged pull with more to come: ask for the next page.
                if payload.pointer("/data/more").and_then(Value::as_i64) == Some(1) {
                    let qkey = format!("{cmd_id}_{sub_cmd}");
                    let last_id = payload
                        .pointer("/data/lastActionId")
                        .or_else(|| payload.pointer(&format!("/data/queueStatus/{qkey}/lastId")))
                        .cloned()
                        .unwrap_or(json!(1));
                    let pre_ids = payload
                        .pointer(&format!("/data/queueStatus/{qkey}/ids"))
                        .cloned()
                        .unwrap_or_else(|| json!([]));
                    let frame = pull_frame(cmd_id, sub_cmd, &next_req(), false, last_id, pre_ids);
                    ws.send(Message::Binary(frame)).await.map_err(ws_err)?;
                }

                let event = RealtimeEvent::Frame(WsEvent { cmd: cmd_id, sub_cmd, version, payload });
                if tx.send(event).is_err() {
                    return Ok("receiver dropped".into());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const KEY: [u8; 32] = [9u8; 32];

    fn gcm_blob(plain: &[u8]) -> Vec<u8> {
        let iv = [1u8; 16];
        let aad = [2u8; 16];
        let ct = AesGcm::<aes::Aes256, U16>::new_from_slice(&KEY)
            .unwrap()
            .encrypt(
                aes_gcm::Nonce::<U16>::from_slice(&iv),
                Payload {
                    msg: plain,
                    aad: &aad,
                },
            )
            .unwrap();
        [iv.as_slice(), aad.as_slice(), ct.as_slice()].concat()
    }

    #[test]
    fn frame_roundtrip() {
        let body = json!({ "a": 1, "b": "x" });
        let f = encode_frame(604, 0, 1, Some(&body));
        assert_eq!(f[0], 1);
        assert_eq!(u16::from_le_bytes([f[1], f[2]]), 604);
        assert_eq!(decode_frame(&f), Some((1, 604, 0, body)));
    }

    #[test]
    fn frame_without_body() {
        let f = encode_frame(cmd::PING, SUB_KEEP_CONNECTION, 1, None);
        assert_eq!(f.len(), 4);
        assert_eq!(decode_frame(&f), Some((1, 2, 1, Value::Null)));
        assert_eq!(decode_frame(&[1, 2]), None);
    }

    #[test]
    fn queues_follow_the_auth_lists() {
        let auth = json!({ "error_code": 0, "data": {
            "ctrl_qCmds": [{ "cmd": 610, "subCmd": 1 }, { "cmd": 610, "subCmd": 1 }, { "cmd": 603, "subCmd": 1 }],
            "qCmds": [{ "cmd": 511, "subCmd": 1 }]
        }});
        assert_eq!(queues_from_auth(&auth), vec![(610, 1), (603, 1), (511, 1)]);
        assert!(queues_from_auth(&json!({ "data": {} })).is_empty());
    }

    #[test]
    fn shapes_hide_values_and_ids() {
        // serde_json keeps keys sorted.
        let v = json!({ "data": { "reacts": [{ "msgId": "123", "content": "{\"rIcon\":\"/-heart\"}" }] } });
        assert_eq!(
            payload_shape(&v),
            "{data: {reacts: [1 × {content: json{rIcon: str}, msgId: str}]}}"
        );
        assert_eq!(payload_shape(&json!({ "1234567": 1 })), "{<id>: num}");
        assert_eq!(payload_shape(&json!([])), "[]");
    }

    #[test]
    fn percent_decode_keeps_raw_base64() {
        assert_eq!(percent_decode("abc+/="), "abc+/=");
        assert_eq!(percent_decode("a%2Bb%3D"), "a+b=");
        assert_eq!(percent_decode("tail%2"), "tail%2");
    }

    #[test]
    fn plain_and_compressed_payloads() {
        assert_eq!(decrypt_payload(None, 0, "{}").unwrap(), b"{}");
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(b"{\"x\":1}").unwrap();
        let encoded = B64.encode(z.finish().unwrap());
        assert_eq!(decrypt_payload(None, 1, &encoded).unwrap(), b"{\"x\":1}");
    }

    #[test]
    fn aes_gcm_payloads() {
        let blob = B64.encode(gcm_blob(b"{\"ok\":true}"));
        assert_eq!(
            decrypt_payload(Some(&KEY), 3, &blob).unwrap(),
            b"{\"ok\":true}"
        );

        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(b"{\"gz\":1}").unwrap();
        let blob = B64.encode(gcm_blob(&gz.finish().unwrap()));
        assert_eq!(
            decrypt_payload(Some(&KEY), 2, &blob).unwrap(),
            b"{\"gz\":1}"
        );

        assert!(decrypt_payload(None, 3, &B64.encode(gcm_blob(b"x"))).is_err());
        assert!(decrypt_payload(Some(&[0u8; 32]), 3, &B64.encode(gcm_blob(b"x"))).is_err());
        assert!(decrypt_payload(Some(&KEY), 9, "").is_err());
    }

    #[test]
    fn short_gcm_blob_is_rejected() {
        let err = gcm_decrypt(&KEY, &[0u8; 16]).unwrap_err().to_string();
        assert!(err.contains("too short"), "{err}");
    }

    #[test]
    fn url_gets_path_params_and_timestamp() {
        let u = build_url("wss://ws1-msg.chat.zalo.me");
        assert!(u.starts_with("wss://ws1-msg.chat.zalo.me/?zpw_ver="), "{u}");
        assert!(u.contains("&t="));
        let u = build_url("wss://h/path?x=1");
        assert!(u.starts_with("wss://h/path?x=1&zpw_ver="), "{u}");
    }
}
