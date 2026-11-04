//! Device tokens. Each client device gets its own random token; the gateway stores
//! only its SHA-256, so a leaked database does not leak usable tokens.

use axum::extract::FromRequestParts;
use axum::http::header::{AUTHORIZATION, COOKIE};
use axum::http::request::Parts;
use axum::http::HeaderMap;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::RngCore;
use sha2::{Digest, Sha256};

use crate::api::{ApiError, AppState};
use crate::model::Device;

/// Prefix that makes tokens easy to recognise (and to scan for in leaks).
pub const TOKEN_PREFIX: &str = "zc_";

/// Cookie a browser client can use instead of the `Authorization` header.
pub const COOKIE_NAME: &str = "zetachat_token";

/// A new random token: `zc_` + 32 random bytes in URL-safe base64.
pub fn new_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    format!("{TOKEN_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes))
}

/// A short random device id.
pub fn new_device_id() -> String {
    let mut bytes = [0u8; 6];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Hex SHA-256 of a token, the only form stored in the database.
pub fn hash_token(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Token from `Authorization: Bearer …` or the `zetachat_token` cookie.
pub fn token_from_headers(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        if let Some(token) = value.strip_prefix("Bearer ") {
            let token = token.trim();
            if !token.is_empty() {
                return Some(token.to_string());
            }
        }
    }
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            (name == COOKIE_NAME && !value.is_empty()).then(|| value.to_string())
        })
}

/// Extractor that only lets known, non-revoked devices through.
pub struct AuthDevice(pub Device);

impl FromRequestParts<AppState> for AuthDevice {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = token_from_headers(&parts.headers).ok_or_else(ApiError::unauthorized)?;
        match state.store.device_for_token(&token) {
            Ok(Some(device)) => Ok(Self(device)),
            Ok(None) => Err(ApiError::unauthorized()),
            Err(e) => Err(ApiError::internal(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn tokens_are_random_and_prefixed() {
        let a = new_token();
        let b = new_token();
        assert!(a.starts_with(TOKEN_PREFIX));
        assert_eq!(a.len(), TOKEN_PREFIX.len() + 43);
        assert_ne!(a, b);
        assert_eq!(hash_token(&a).len(), 64);
        assert_ne!(hash_token(&a), hash_token(&b));
    }

    #[test]
    fn token_comes_from_bearer_or_cookie() {
        let mut h = HeaderMap::new();
        assert_eq!(token_from_headers(&h), None);
        h.insert(COOKIE, HeaderValue::from_static("a=1; zetachat_token=zc_c"));
        assert_eq!(token_from_headers(&h).as_deref(), Some("zc_c"));
        h.insert(AUTHORIZATION, HeaderValue::from_static("Bearer zc_h"));
        assert_eq!(token_from_headers(&h).as_deref(), Some("zc_h"));
    }
}
