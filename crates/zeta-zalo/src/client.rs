//! HTTP plumbing shared by the login flow and the APIs: cookie jar, default
//! headers, common query parameters, encrypted `params` and response decoding.

use std::sync::Arc;
use std::time::Duration;

use reqwest::cookie::Jar;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE, ORIGIN, REFERER, SET_COOKIE};
use reqwest::{RequestBuilder, Url};
use serde_json::Value;

use crate::crypto;
use crate::domains::{Domains, Service};
use crate::error::{endpoint_of, Error, Result};
use crate::session::{Device, SessionData};
use crate::upload::FileLimits;
use crate::{USER_AGENT, ZPW_TYPE, ZPW_VER};

/// A Zalo client. Before login it can only run the login flow; after a login or
/// [`ZaloClient::from_session`] it can call the APIs.
pub struct ZaloClient {
    http: reqwest::Client,
    jar: Arc<Jar>,
    device: Device,
    domains: Domains,
    session: Option<SessionData>,
    key: Option<Vec<u8>>,
    pub(crate) limits: FileLimits,
}

/// Status, body and session cookie of a request whose status matters (QR polling).
pub(crate) struct RawResponse {
    pub status: u16,
    pub body: String,
    pub zpw_sek_cookie: Option<String>,
}

fn build_http(jar: Arc<Jar>) -> Result<reqwest::Client> {
    let mut headers = HeaderMap::new();
    headers.insert(REFERER, HeaderValue::from_static("https://chat.zalo.me/"));
    headers.insert(ORIGIN, HeaderValue::from_static("https://chat.zalo.me"));
    Ok(reqwest::Client::builder()
        .cookie_provider(jar)
        .default_headers(headers)
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(40))
        .build()?)
}

/// Make the jar send `zpw_sek` to every `*.zalo.me` and `*.zaloapp.com` host.
///
/// Verified live: the cookie alone is enough; a `zpw_sek` request header without the
/// cookie is not (Zalo answers with an empty body).
fn install_session_cookie(jar: &Jar, zpw_sek: &str) {
    for (domain, origin) in [
        ("zalo.me", "https://chat.zalo.me/"),
        ("zaloapp.com", "https://wpa.zaloapp.com/"),
    ] {
        if let Ok(url) = Url::parse(origin) {
            jar.add_cookie_str(
                &format!("zpw_sek={zpw_sek}; Domain={domain}; Path=/; Secure; HttpOnly"),
                &url,
            );
            jar.add_cookie_str(&format!("_zlang=vi; Domain={domain}; Path=/"), &url);
        }
    }
}

/// Value of a `zpw_sek` `Set-Cookie` header, ignoring deletions.
pub(crate) fn parse_zpw_sek_cookie(set_cookie: &str) -> Option<String> {
    let (name, rest) = set_cookie.split_once('=')?;
    if name.trim() != "zpw_sek" {
        return None;
    }
    let value = rest.split(';').next()?.trim();
    (!value.is_empty() && value != "EXPIRED" && value != "deleted").then(|| value.to_string())
}

impl ZaloClient {
    /// A client for logging in.
    pub fn new(device: Device) -> Result<Self> {
        let jar = Arc::new(Jar::default());
        Ok(Self {
            http: build_http(jar.clone())?,
            jar,
            device,
            domains: Domains::default(),
            session: None,
            key: None,
            limits: FileLimits::default(),
        })
    }

    /// Resume a saved session.
    pub fn from_session(session: SessionData) -> Result<Self> {
        let mut client = Self::new(session.device.clone())?;
        client.domains = session.domains.clone();
        client.set_session(session)?;
        Ok(client)
    }

    pub fn device(&self) -> &Device {
        &self.device
    }

    pub fn domains(&self) -> &Domains {
        &self.domains
    }

    /// The logged-in session, if any.
    pub fn session(&self) -> Option<&SessionData> {
        self.session.as_ref()
    }

    pub(crate) fn domains_mut(&mut self) -> &mut Domains {
        &mut self.domains
    }

    pub(crate) fn session_mut(&mut self) -> Option<&mut SessionData> {
        self.session.as_mut()
    }

    pub(crate) fn set_session(&mut self, session: SessionData) -> Result<()> {
        let key = session.session_key()?;
        install_session_cookie(&self.jar, &session.zpw_sek);
        self.key = Some(key);
        self.session = Some(session);
        Ok(())
    }

    pub(crate) fn install_cookie(&self, zpw_sek: &str) {
        install_session_cookie(&self.jar, zpw_sek);
    }

    /// `zpw_ver=…&zpw_type=…`, the query every API call starts with.
    pub fn common_params() -> String {
        format!("zpw_ver={ZPW_VER}&zpw_type={ZPW_TYPE}")
    }

    pub(crate) fn session_key(&self) -> Result<&[u8]> {
        self.key.as_deref().ok_or(Error::NotLoggedIn)
    }

    /// `getEncodedParams`: AES with the session key, base64, URL-encoded.
    pub(crate) fn encrypt_params(&self, payload: &Value) -> Result<String> {
        let b64 = crypto::encrypt_b64(self.session_key()?, &payload.to_string())?;
        Ok(crypto::url_encode(&b64))
    }

    /// [`Self::encrypt_params`] of a plain string rather than JSON (the sticker
    /// catalogue is asked for with the empty string).
    pub(crate) fn encrypt_text(&self, text: &str) -> Result<String> {
        let b64 = crypto::encrypt_b64(self.session_key()?, text)?;
        Ok(crypto::url_encode(&b64))
    }

    pub(crate) async fn get(&self, url: &str, key: Option<&[u8]>) -> Result<Value> {
        self.execute(self.http.get(url), url, key).await
    }

    /// GET a file rather than an API answer: its bytes, at most `max`.
    pub(crate) async fn get_bytes(&self, url: &str, max: u64) -> Result<Vec<u8>> {
        let resp = self.http.get(url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Status {
                status: status.as_u16(),
                endpoint: endpoint_of(url),
            });
        }
        if resp.content_length().is_some_and(|n| n > max) {
            return Err(Error::TooLarge { limit: max });
        }
        let bytes = resp.bytes().await?;
        if bytes.len() as u64 > max {
            return Err(Error::TooLarge { limit: max });
        }
        Ok(bytes.to_vec())
    }

    /// POST to a service, moving to the next host only when one answers 404:
    /// nothing was done there, so trying another cannot do anything twice.
    pub(crate) async fn post_service(
        &self,
        service: Service,
        path: &str,
        form: String,
        key: Option<&[u8]>,
    ) -> Result<Value> {
        let mut last = None;
        for host in self.domains.candidates(service) {
            let url = format!("{host}{path}?{}", Self::common_params());
            match self.post_form(&url, form.clone(), key).await {
                Err(e @ Error::Status { status: 404, .. }) => {
                    tracing::debug!(service = service.key(), %host, "404, trying next host");
                    last = Some(e);
                }
                other => return other,
            }
        }
        Err(last.unwrap_or_else(|| {
            Error::Protocol(format!("no host configured for {}", service.key()))
        }))
    }

    pub(crate) async fn post_form(
        &self,
        url: &str,
        form: String,
        key: Option<&[u8]>,
    ) -> Result<Value> {
        let request = self
            .http
            .post(url)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(form);
        self.execute(request, url, key).await
    }

    /// `GET {service}{path}?zpw_ver…&params=<encrypted params>`: any read call,
    /// for exploring the protocol (see the `zeta probe` example command).
    pub async fn get_encrypted(
        &self,
        service: Service,
        path: &str,
        params: &Value,
    ) -> Result<Value> {
        let key = self.session_key()?;
        let path_and_query = format!(
            "{path}?{}&params={}",
            Self::common_params(),
            self.encrypt_params(params)?
        );
        self.get_service(service, &path_and_query, Some(key)).await
    }

    /// POST one file part as `multipart/form-data` (uploads to the file service).
    pub(crate) async fn post_multipart(
        &self,
        url: &str,
        field: &str,
        file_name: &str,
        mime: &str,
        bytes: &[u8],
        key: Option<&[u8]>,
    ) -> Result<Value> {
        let boundary = format!("----WebKitFormBoundary{}", crypto::random_hex(16));
        let safe_name: String = file_name
            .chars()
            .map(|c| {
                if c == '"' || c == '\\' || c.is_control() {
                    '_'
                } else {
                    c
                }
            })
            .collect();
        let head = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"; \
             filename=\"{safe_name}\"\r\nContent-Type: {mime}\r\n\r\n"
        );
        let tail = format!("\r\n--{boundary}--\r\n");
        let mut body = Vec::with_capacity(head.len() + bytes.len() + tail.len());
        body.extend_from_slice(head.as_bytes());
        body.extend_from_slice(bytes);
        body.extend_from_slice(tail.as_bytes());
        let request = self
            .http
            .post(url)
            .header(
                CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .timeout(Duration::from_secs(120))
            .body(body);
        self.execute(request, url, key).await
    }

    /// GET on a service, moving to the next host on transport errors, 404 and 5xx.
    /// Zalo error codes never trigger failover.
    pub(crate) async fn get_service(
        &self,
        service: Service,
        path_and_query: &str,
        key: Option<&[u8]>,
    ) -> Result<Value> {
        let mut last = None;
        for host in self.domains.candidates(service) {
            let url = format!("{host}{path_and_query}");
            match self.get(&url, key).await {
                Ok(v) => return Ok(v),
                Err(e) if e.is_failover() => {
                    tracing::debug!(service = service.key(), %host, error = %e, "trying next host");
                    last = Some(e);
                }
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| {
            Error::Protocol(format!("no host configured for {}", service.key()))
        }))
    }

    /// GET that returns the raw status instead of failing on it.
    pub(crate) async fn get_raw(&self, url: &str) -> Result<RawResponse> {
        let resp = self.http.get(url).send().await?;
        let status = resp.status().as_u16();
        let zpw_sek_cookie = resp
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(parse_zpw_sek_cookie);
        let body = resp.text().await?;
        Ok(RawResponse {
            status,
            body,
            zpw_sek_cookie,
        })
    }

    async fn execute(
        &self,
        request: RequestBuilder,
        url: &str,
        key: Option<&[u8]>,
    ) -> Result<Value> {
        let endpoint = endpoint_of(url);
        let resp = request.send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Status {
                status: status.as_u16(),
                endpoint,
            });
        }
        let body = resp.text().await?;
        decode_response(&body, key, &endpoint)
    }
}

fn check_code(envelope: &Value) -> Result<()> {
    let code = envelope
        .get("error_code")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if code == 0 {
        return Ok(());
    }
    let message = envelope
        .get("error_message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Err(Error::from_code(code, message))
}

/// Turn a Zalo response body into its `data` payload.
///
/// * An empty body is an error. ctdozlite treated it as `error_code 0`, so its
///   session check reported a valid session that Zalo was actually refusing.
/// * Any non-zero `error_code`, outer or inside the decrypted `data`, is an error.
/// * `data` given as an encrypted string is decrypted with `key`; a whole body that
///   is not JSON is decrypted the same way.
pub(crate) fn decode_response(body: &str, key: Option<&[u8]>, endpoint: &str) -> Result<Value> {
    let body = body.trim();
    if body.is_empty() {
        return Err(Error::EmptyResponse {
            endpoint: endpoint.to_string(),
        });
    }
    let envelope: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => {
            let key =
                key.ok_or_else(|| Error::Protocol(format!("non-JSON body from {endpoint}")))?;
            serde_json::from_str(&crypto::decrypt_b64(key, body)?)?
        }
    };
    check_code(&envelope)?;
    let data = envelope.get("data").cloned().unwrap_or(Value::Null);
    let encrypted = match (&data, key) {
        (Value::String(t), Some(k))
            if !t.is_empty() && !t.starts_with('{') && !t.starts_with('[') =>
        {
            Some((t.clone(), k))
        }
        _ => None,
    };
    let Some((text, key)) = encrypted else {
        return Ok(data);
    };
    match crypto::decrypt_b64(key, &text) {
        Ok(plain) => {
            let inner: Value = serde_json::from_str(&plain)?;
            // The decrypted text is usually a full envelope with its own error_code.
            if inner.get("error_code").is_some() {
                check_code(&inner)?;
                Ok(inner.get("data").cloned().unwrap_or(Value::Null))
            } else {
                Ok(inner)
            }
        }
        Err(e) => {
            tracing::debug!(%endpoint, error = %e, "data is a plain string");
            Ok(data)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::cookie::CookieStore;
    use serde_json::json;

    const KEY: &[u8] = b"0123456789abcdef";

    fn enc(v: &Value) -> String {
        crypto::encrypt_b64(KEY, &v.to_string()).unwrap()
    }

    #[test]
    fn empty_body_is_never_success() {
        let err = decode_response("  ", Some(KEY), "https://h/x").unwrap_err();
        assert!(matches!(err, Error::EmptyResponse { .. }));
    }

    #[test]
    fn session_codes_are_rejections() {
        for code in [600, -102] {
            let body =
                json!({ "error_code": code, "error_message": "zpw_sek invalid" }).to_string();
            assert!(decode_response(&body, None, "e")
                .unwrap_err()
                .is_session_rejected());
        }
        let body = json!({ "error_code": 114, "error_message": "bad param" }).to_string();
        assert!(matches!(
            decode_response(&body, None, "e").unwrap_err(),
            Error::Server { code: 114, .. }
        ));
    }

    #[test]
    fn plain_data_is_returned() {
        let body = json!({ "error_code": 0, "data": { "a": 1 } }).to_string();
        assert_eq!(
            decode_response(&body, None, "e").unwrap(),
            json!({ "a": 1 })
        );
    }

    #[test]
    fn encrypted_data_envelope_is_unwrapped() {
        let inner = json!({ "error_code": 0, "data": { "profile": { "userId": "1" } } });
        let body = json!({ "error_code": 0, "data": enc(&inner) }).to_string();
        assert_eq!(
            decode_response(&body, Some(KEY), "e").unwrap(),
            json!({ "profile": { "userId": "1" } })
        );
    }

    #[test]
    fn rejection_inside_encrypted_data_is_detected() {
        let inner = json!({ "error_code": 600, "error_message": "x", "data": null });
        let body = json!({ "error_code": 0, "data": enc(&inner) }).to_string();
        assert!(decode_response(&body, Some(KEY), "e")
            .unwrap_err()
            .is_session_rejected());
    }

    #[test]
    fn whole_encrypted_body_is_decoded() {
        let body = enc(&json!({ "error_code": 0, "data": [1, 2] }));
        assert_eq!(
            decode_response(&body, Some(KEY), "e").unwrap(),
            json!([1, 2])
        );
    }

    #[test]
    fn non_json_without_key_is_an_error() {
        assert!(matches!(
            decode_response("<html>", None, "e").unwrap_err(),
            Error::Protocol(_)
        ));
    }

    #[test]
    fn zpw_sek_cookie_parsing() {
        assert_eq!(
            parse_zpw_sek_cookie("zpw_sek=abc.def; Domain=zaloapp.com; Path=/"),
            Some("abc.def".to_string())
        );
        assert_eq!(parse_zpw_sek_cookie("zpw_sek=EXPIRED; Path=/"), None);
        assert_eq!(parse_zpw_sek_cookie("app.event.zaloapp.com=1"), None);
    }

    #[test]
    fn session_cookie_reaches_both_host_families() {
        let jar = Jar::default();
        install_session_cookie(&jar, "S1");
        for host in [
            "https://tt-profile-wpa.chat.zalo.me/",
            "https://tt-group-wpa.zaloapp.com/",
            "https://tt-chat3-wpa.api.zaloapp.com/",
        ] {
            let header = jar.cookies(&Url::parse(host).unwrap()).unwrap();
            assert!(header.to_str().unwrap().contains("zpw_sek=S1"), "{host}");
        }
    }
}
