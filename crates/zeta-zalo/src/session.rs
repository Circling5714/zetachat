//! Device identity and the persisted session.

use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::crypto;
use crate::domains::Domains;
use crate::error::{Error, Result};
use crate::USER_AGENT;

/// Version of the [`SessionData`] file format.
pub const SESSION_FORMAT: u32 = 1;

/// How this client identifies itself to Zalo. Keep it stable for a machine: a new
/// IMEI looks like a new computer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Device {
    /// `uuid-md5(user agent)`, 69 characters, the format of the official client.
    pub imei: String,
    /// Shown as "Ứng dụng" on the phone's login confirmation.
    pub computer_name: String,
}

impl Device {
    /// A fresh identity.
    pub fn generate(computer_name: impl Into<String>) -> Self {
        Self {
            imei: format!("{}-{}", crypto::uuid_v4(), crypto::md5_hex(USER_AGENT)),
            computer_name: computer_name.into(),
        }
    }
}

/// Everything needed to resume a logged-in session.
///
/// **Contains credentials.** Store it with owner-only permissions, never log it and
/// never commit it. [`SessionData::save`] writes it that way.
#[derive(Clone, Serialize, Deserialize)]
pub struct SessionData {
    pub format: u32,
    pub device: Device,
    /// Session cookie, taken from the QR confirm step.
    pub zpw_sek: String,
    /// Base64 AES key from `getLoginInfo`, used for `params` and `data`.
    pub zpw_enk: String,
    /// Account id.
    pub uid: String,
    #[serde(default)]
    pub display_name: String,
    /// Id of "Cloud của tôi".
    #[serde(default)]
    pub send2me_id: String,
    #[serde(default)]
    pub domains: Domains,
    /// Unix seconds of the login, 0 when unknown.
    #[serde(default)]
    pub logged_in_at: i64,
}

impl std::fmt::Debug for SessionData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionData")
            .field("format", &self.format)
            .field("computer_name", &self.device.computer_name)
            .field("zpw_sek", &"<redacted>")
            .field("zpw_enk", &"<redacted>")
            .field("uid", &"<redacted>")
            .field("logged_in_at", &self.logged_in_at)
            .finish()
    }
}

impl SessionData {
    /// Raw AES key decoded from `zpw_enk`.
    pub fn session_key(&self) -> Result<Vec<u8>> {
        crypto::decode_session_key(&self.zpw_enk)
    }

    /// Read a session file written by [`SessionData::save`].
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let session: Self = serde_json::from_str(&text)?;
        if session.format != SESSION_FORMAT {
            return Err(Error::Protocol(format!(
                "unsupported session format {}",
                session.format
            )));
        }
        Ok(session)
    }

    /// Write atomically (temporary file + rename), owner-only on Unix.
    pub fn save(&self, path: &Path) -> Result<()> {
        let text = serde_json::to_vec_pretty(self)?;
        let tmp = path.with_extension("tmp");
        {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut file = opts.open(&tmp)?;
            file.write_all(&text)?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Convert a ctdozlite `session.json`.
    ///
    /// A file written by ctdozlite's terminal QR login may hold `zpw_wpsk` instead of
    /// the real session cookie (see `docs/GIAO_THUC.md`); check the result with
    /// [`crate::ZaloClient::check_session`].
    pub fn from_ctdozlite_json(text: &str) -> Result<Self> {
        let v: Value = serde_json::from_str(text)?;
        let get = |pointer: &str| {
            v.pointer(pointer)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let first_of = |pointers: &[&str]| {
            pointers
                .iter()
                .map(|p| get(p))
                .find(|s| !s.is_empty())
                .unwrap_or_default()
        };
        let zpw_sek = first_of(&["/zpw_sek", "/login/zpw_sek"]);
        let zpw_enk = first_of(&["/login/zpw_enk", "/secret_key_b64"]);
        if zpw_sek.is_empty() || zpw_enk.is_empty() {
            return Err(Error::Protocol(
                "ctdozlite session has no zpw_sek or zpw_enk".into(),
            ));
        }

        let mut domains = Domains::default();
        if let Some(d) = v.get("domains") {
            if let Some(x) = d
                .get("lp_chat")
                .and_then(Value::as_str)
                .filter(|x| !x.is_empty())
            {
                domains.lp_chat = x.to_string();
            }
            if let Some(x) = d
                .get("lp_control")
                .and_then(Value::as_str)
                .filter(|x| !x.is_empty())
            {
                domains.lp_control = x.to_string();
            }
            let ws: Vec<String> = d
                .get("ws")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            if !ws.is_empty() {
                domains.ws = ws;
            }
        }

        let session = Self {
            format: SESSION_FORMAT,
            device: Device {
                imei: get("/imei"),
                computer_name: get("/computer_name"),
            },
            zpw_sek,
            zpw_enk,
            uid: get("/login/uin"),
            display_name: get("/login/display_name"),
            send2me_id: get("/login/send2me_id"),
            domains,
            logged_in_at: 0,
        };
        session.session_key()?;
        Ok(session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD as B64, Engine};

    fn sample() -> SessionData {
        SessionData {
            format: SESSION_FORMAT,
            device: Device::generate("Test"),
            zpw_sek: "SEK-SECRET-VALUE".into(),
            zpw_enk: B64.encode([1u8; 16]),
            uid: "123".into(),
            display_name: "Tester".into(),
            send2me_id: "456".into(),
            domains: Domains::default(),
            logged_in_at: 1,
        }
    }

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("zeta-zalo-test-{}", crypto::random_hex(12)));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn generated_imei_has_official_shape() {
        let d = Device::generate("PC");
        assert_eq!(d.imei.len(), 69);
        // md5 of USER_AGENT, checked with md5sum.
        assert!(d.imei.ends_with("-13c30bf4b6d245f0d54ae7dfbc31b491"));
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = temp_dir();
        let path = dir.join("session.json");
        let s = sample();
        s.save(&path).unwrap();
        let back = SessionData::load(&path).unwrap();
        assert_eq!(back.zpw_sek, s.zpw_sek);
        assert_eq!(back.device, s.device);
        assert_eq!(back.session_key().unwrap(), vec![1u8; 16]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn debug_output_hides_credentials() {
        let text = format!("{:?}", sample());
        assert!(!text.contains("SEK-SECRET-VALUE"));
        assert!(text.contains("<redacted>"));
    }

    #[test]
    fn imports_a_ctdozlite_session() {
        let enk = B64.encode([2u8; 16]);
        let text = serde_json::json!({
            "imei": "imei-1",
            "computer_name": "DESKTOP",
            "zpw_sek": "sek-1",
            "secret_key_b64": enk,
            "login": { "uin": "42", "zpw_enk": enk, "zpw_sek": "sek-1", "display_name": "", "send2me_id": "7" },
            "domains": { "lp_chat": "https://p5-msg-chat.zaloapp.com", "ws": ["wss://ws2-msg.chat.zalo.me"] }
        })
        .to_string();
        let s = SessionData::from_ctdozlite_json(&text).unwrap();
        assert_eq!(s.uid, "42");
        assert_eq!(s.zpw_sek, "sek-1");
        assert_eq!(s.domains.lp_chat, "https://p5-msg-chat.zaloapp.com");
        assert_eq!(s.domains.ws, vec!["wss://ws2-msg.chat.zalo.me".to_string()]);
        assert_eq!(s.domains.profile, Domains::default().profile);
    }

    #[test]
    fn ctdozlite_session_without_cookie_is_rejected() {
        assert!(SessionData::from_ctdozlite_json(r#"{"login":{"zpw_enk":"AAAA"}}"#).is_err());
    }
}
