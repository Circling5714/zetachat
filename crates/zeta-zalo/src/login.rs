//! QR login, verified live in November 2026 (see `docs/GIAO_THUC.md`).
//!
//! Flow: `getServerInfo` → `reqqr` → wait for the scan → wait for the tap on
//! "Đăng nhập" (this step sets the session cookie) → `getLoginInfo` (encryption key,
//! endpoints) → `me-v2` to prove the session works.
//!
//! Deliberate difference from ctdozlite: the session cookie `zpw_sek` is taken **only**
//! from the confirm step. ctdozlite's terminal login also searched the `getLoginInfo`
//! payload for cookie-like keys and picked `zpw_wpsk`, a different token, after which
//! every request failed with error 600.

use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::client::{RawResponse, ZaloClient};
use crate::crypto;
use crate::error::{endpoint_of, Error, Result};
use crate::param_cipher::ParamCipher;
use crate::session::{SessionData, SESSION_FORMAT};
use crate::{unix_now, ZPW_TYPE, ZPW_VER};

/// Pause between polls of the QR endpoints.
const POLL_PAUSE: Duration = Duration::from_millis(800);

/// A login QR code.
#[derive(Clone, Debug)]
pub struct QrTicket {
    /// Text the QR code encodes.
    pub payload: String,
    /// How long the code stays valid.
    pub timeout: Duration,
    wait_scan_url: String,
    confirm_url: String,
    retry: u32,
}

/// Who scanned the code.
#[derive(Clone, Debug, Default)]
pub struct ScanInfo {
    pub display_name: String,
    pub avatar: String,
}

/// Progress of [`ZaloClient::login_qr`].
#[derive(Debug)]
pub enum QrEvent<'a> {
    /// Show this code to the user.
    Ticket(&'a QrTicket),
    /// The phone scanned it; waiting for the tap on "Đăng nhập".
    Scanned(&'a ScanInfo),
    /// The phone confirmed; finishing the login.
    Confirmed,
}

fn field(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

impl QrTicket {
    fn from_data(d: &Value) -> Result<Self> {
        let ticket = Self {
            payload: field(d, "qr_cnt"),
            timeout: Duration::from_secs(
                d.get("qr_timeout").and_then(Value::as_u64).unwrap_or(180),
            ),
            wait_scan_url: field(d, "chk_wait_scan"),
            confirm_url: field(d, "chk_wait_cfirm"),
            retry: d.get("qr_retry").and_then(Value::as_u64).unwrap_or(3) as u32,
        };
        if ticket.payload.is_empty()
            || ticket.wait_scan_url.is_empty()
            || ticket.confirm_url.is_empty()
        {
            return Err(Error::Protocol(
                "reqqr answer lacks qr_cnt or polling URLs".into(),
            ));
        }
        Ok(ticket)
    }
}

/// Query string with `signkey` appended, pairs sorted (as `_constructUrlParams` does
/// after `getSignKey`).
pub(crate) fn signed_query(params: &Map<String, Value>, tag: &str) -> String {
    let sig = crypto::sign_key(tag, params);
    let mut pairs: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{k}={}", crypto::url_encode(&crypto::param_value(v))))
        .collect();
    pairs.push(format!("signkey={sig}"));
    pairs.sort();
    pairs.join("&")
}

/// Query string without signing.
pub(crate) fn flatten_query(params: &Map<String, Value>) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{k}={}", crypto::url_encode(&crypto::param_value(v))))
        .collect::<Vec<_>>()
        .join("&")
}

/// The session cookie of a successful confirm answer: the `Set-Cookie` header first,
/// then `data.zpw_sek`. Nothing else is trusted.
pub(crate) fn session_cookie_from_confirm(raw: &RawResponse, body: &Value) -> Option<String> {
    raw.zpw_sek_cookie.clone().or_else(|| {
        body.pointer("/data/zpw_sek")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    })
}

impl ZaloClient {
    /// `getServerInfo`: refresh the endpoint table. Works without a session.
    pub async fn refresh_server_info(&mut self) -> Result<()> {
        let mut p = Map::new();
        p.insert("imei".into(), json!(self.device().imei));
        p.insert("type".into(), json!(ZPW_TYPE));
        p.insert("client_version".into(), json!(ZPW_VER));
        p.insert("computer_name".into(), json!(self.device().computer_name));
        p.insert("sp_mtn".into(), json!(-1));
        p.insert("bkt".into(), json!(-1));
        let url = format!(
            "{}/api/login/getServerInfo?{}",
            self.domains().auth,
            signed_query(&p, "getserverinfo")
        );
        let data = self.get(&url, None).await?;
        self.domains_mut().apply_login_config(&data);
        Ok(())
    }

    /// Ask for a login QR code.
    pub async fn request_qr(&self, language: &str) -> Result<QrTicket> {
        let mut p = Map::new();
        p.insert("language".into(), json!(language));
        p.insert("client_time".into(), json!(unix_now()));
        p.insert("imei".into(), json!(self.device().imei));
        p.insert("computer_name".into(), json!(self.device().computer_name));
        p.insert("logged_uids".into(), json!("[]"));
        // 1 = return the payload so the client draws the code itself.
        p.insert("self_g".into(), json!(1));
        p.insert("qr_form".into(), json!(0));
        p.insert("type".into(), json!(ZPW_TYPE));
        p.insert("client_version".into(), json!(ZPW_VER));
        let url = format!(
            "{}/api/login/reqqr?{}",
            self.domains().auth,
            signed_query(&p, "reqqr")
        );
        let data = self.get(&url, None).await?;
        QrTicket::from_data(&data)
    }

    /// Long-poll until the phone scans the code. HTTP 408 means "still waiting".
    pub async fn wait_for_scan(&self, ticket: &QrTicket) -> Result<ScanInfo> {
        let url = format!("{}&lang=vi", ticket.wait_scan_url);
        let deadline = Instant::now() + ticket.timeout;
        let mut refusals = 0u32;
        loop {
            if Instant::now() >= deadline {
                return Err(Error::Timeout("the QR code was not scanned in time".into()));
            }
            let raw = match self.get_raw(&url).await {
                Ok(r) => r,
                Err(e) if e.is_timeout() => continue,
                Err(e) => return Err(e),
            };
            match raw.status {
                408 => {}
                200 => {
                    let v: Value = serde_json::from_str(&raw.body).unwrap_or(Value::Null);
                    let code = v.get("error_code").and_then(Value::as_i64).unwrap_or(0);
                    if code == 0 {
                        if let Some(d) = v.get("data").filter(|d| !d.is_null()) {
                            return Ok(ScanInfo {
                                display_name: field(d, "display_name"),
                                avatar: field(d, "avatar"),
                            });
                        }
                    } else {
                        refusals += 1;
                        if refusals > ticket.retry.max(1) {
                            return Err(Error::from_code(code, field(&v, "error_message")));
                        }
                    }
                }
                status => {
                    return Err(Error::Status {
                        status,
                        endpoint: endpoint_of(&url),
                    })
                }
            }
            tokio::time::sleep(POLL_PAUSE).await;
        }
    }

    /// Poll until the user taps "Đăng nhập"; returns the session cookie `zpw_sek`.
    /// `18012` means scanned but not confirmed yet.
    pub async fn wait_for_confirm(&self, ticket: &QrTicket) -> Result<String> {
        let deadline = Instant::now() + ticket.timeout.max(Duration::from_secs(60));
        loop {
            if Instant::now() >= deadline {
                return Err(Error::Timeout(
                    "the login was not confirmed on the phone".into(),
                ));
            }
            let raw = match self.get_raw(&ticket.confirm_url).await {
                Ok(r) => r,
                Err(e) if e.is_timeout() => continue,
                Err(e) => return Err(e),
            };
            if raw.status != 408 {
                if raw.status != 200 {
                    return Err(Error::Status {
                        status: raw.status,
                        endpoint: endpoint_of(&ticket.confirm_url),
                    });
                }
                let v: Value = serde_json::from_str(&raw.body)?;
                match v.get("error_code").and_then(Value::as_i64).unwrap_or(0) {
                    0 => {
                        return session_cookie_from_confirm(&raw, &v).ok_or_else(|| {
                            Error::Protocol("confirm succeeded without a zpw_sek".into())
                        })
                    }
                    18012 => {}
                    code => return Err(Error::from_code(code, field(&v, "error_message"))),
                }
            }
            tokio::time::sleep(POLL_PAUSE).await;
        }
    }

    /// `getLoginInfo`, encrypted with ParamCipher v2 and tried on both login hosts.
    async fn get_login_info(&self) -> Result<Value> {
        let ts = unix_now();
        let imei = self.device().imei.clone();
        let payload = json!({
            "imei": imei,
            "computer_name": self.device().computer_name,
            "language": "vi",
            "ts": ts,
        });
        let cipher = ParamCipher::new(ZPW_TYPE, &imei, ts)?;
        let query =
            flatten_query(&cipher.build_body(ZPW_TYPE, ZPW_VER, &payload, "getlogininfo")?);
        let hosts = [self.domains().auth.clone(), self.domains().wpa_chat.clone()];
        let mut last = None;
        for host in hosts {
            let url = format!("{host}/api/login/getLoginInfo?{query}&nretry=0");
            match self.get(&url, Some(cipher.key().as_bytes())).await {
                Ok(data) => return Ok(data),
                Err(e) => {
                    tracing::debug!(%host, error = %e, "getLoginInfo failed on this host");
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| Error::Protocol("no login host".into())))
    }

    /// Finish a login with the cookie from [`ZaloClient::wait_for_confirm`]: fetch the
    /// encryption key and endpoints, then prove the session with `me-v2`.
    pub async fn finish_login(
        &mut self,
        zpw_sek: String,
        scan: Option<&ScanInfo>,
    ) -> Result<&SessionData> {
        self.install_cookie(&zpw_sek);
        let cfg = self.get_login_info().await?;
        let zpw_enk = field(&cfg, "zpw_enk");
        if zpw_enk.is_empty() {
            return Err(Error::Protocol("getLoginInfo returned no zpw_enk".into()));
        }
        self.domains_mut().apply_login_config(&cfg);

        let session = SessionData {
            format: SESSION_FORMAT,
            device: self.device().clone(),
            zpw_sek,
            zpw_enk,
            uid: field(&cfg, "uid"),
            display_name: scan.map(|s| s.display_name.clone()).unwrap_or_default(),
            send2me_id: field(&cfg, "send2me_id"),
            domains: self.domains().clone(),
            logged_in_at: unix_now(),
        };
        self.set_session(session)?;

        // Fail fast if Zalo does not accept the session. This check is what exposed
        // the ctdozlite bug described in the module documentation.
        let profile = self.me().await?;
        if let Some(s) = self.session_mut() {
            if s.uid.is_empty() {
                s.uid = profile.uid.clone();
            }
            if s.display_name.is_empty() {
                s.display_name = profile.display_name.clone();
            }
        }
        self.session().ok_or(Error::NotLoggedIn)
    }

    /// Complete QR login. `on_event` receives the code to show and the progress.
    pub async fn login_qr(
        &mut self,
        language: &str,
        mut on_event: impl FnMut(QrEvent<'_>),
    ) -> Result<&SessionData> {
        if let Err(e) = self.refresh_server_info().await {
            // The built-in host table works without it.
            tracing::warn!(error = %e, "getServerInfo failed, using default hosts");
        }
        let ticket = self.request_qr(language).await?;
        on_event(QrEvent::Ticket(&ticket));
        let scan = self.wait_for_scan(&ticket).await?;
        on_event(QrEvent::Scanned(&scan));
        let zpw_sek = self.wait_for_confirm(&ticket).await?;
        on_event(QrEvent::Confirmed);
        self.finish_login(zpw_sek, Some(&scan)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_query_appends_a_valid_signature() {
        let mut p = Map::new();
        p.insert("imei".into(), json!("i"));
        p.insert("type".into(), json!("30"));
        let q = signed_query(&p, "reqqr");
        assert_eq!(
            q,
            format!(
                "imei=i&signkey={}&type=30",
                crypto::md5_hex("zsecurereqqri30")
            )
        );
    }

    #[test]
    fn ticket_requires_payload_and_urls() {
        let ok = json!({
            "qr_cnt": "http://example/qr", "chk_wait_scan": "https://a", "chk_wait_cfirm": "https://b",
            "qr_timeout": 120
        });
        let t = QrTicket::from_data(&ok).unwrap();
        assert_eq!(t.timeout, Duration::from_secs(120));
        assert!(QrTicket::from_data(&json!({ "qr_cnt": "x" })).is_err());
    }

    #[test]
    fn confirm_cookie_prefers_set_cookie_and_ignores_other_tokens() {
        let body = json!({ "error_code": 0, "data": { "zpw_sek": "BODY", "zpw_kt": "KT" } });
        let with_cookie = RawResponse {
            status: 200,
            body: String::new(),
            zpw_sek_cookie: Some("COOKIE".into()),
        };
        assert_eq!(
            session_cookie_from_confirm(&with_cookie, &body).as_deref(),
            Some("COOKIE")
        );

        let no_cookie = RawResponse {
            status: 200,
            body: String::new(),
            zpw_sek_cookie: None,
        };
        assert_eq!(
            session_cookie_from_confirm(&no_cookie, &body).as_deref(),
            Some("BODY")
        );

        // A payload with only other tokens (as getLoginInfo has) yields nothing.
        let others = json!({ "error_code": 0, "data": { "zpw_wpsk": "W", "zpw_kt": "KT" } });
        assert_eq!(session_cookie_from_confirm(&no_cookie, &others), None);
    }
}
