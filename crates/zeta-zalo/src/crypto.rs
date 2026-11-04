//! Crypto primitives of the Zalo Web/PC protocol: AES-CBC with a zero IV, MD5, the
//! `signkey` signature and the identifiers the official client generates.
//!
//! Ported from ctdozlite `src/crypto.rs` (MIT OR Apache-2.0).

use std::fmt::Write as _;

use aes::{Aes128, Aes256};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use md5::{Digest, Md5};
use rand::Rng;
use serde_json::{Map, Value};

use crate::error::{Error, Result};

/// The protocol always uses 16 zero bytes as IV.
const ZERO_IV: [u8; 16] = [0u8; 16];

fn key_error(e: impl std::fmt::Display) -> Error {
    Error::Crypto(format!("AES key: {e}"))
}

fn bad_key_len(n: usize) -> Error {
    Error::Crypto(format!("AES key must be 16 or 32 bytes, got {n}"))
}

/// AES-CBC (AES-128 or AES-256 by key length), zero IV, PKCS7 padding.
pub fn aes_cbc_encrypt(key: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; plaintext.len() + 16];
    buf[..plaintext.len()].copy_from_slice(plaintext);
    let pad_err = |_| Error::Crypto("AES padding failed".into());
    let len = match key.len() {
        16 => cbc::Encryptor::<Aes128>::new_from_slices(key, &ZERO_IV)
            .map_err(key_error)?
            .encrypt_padded_mut::<Pkcs7>(&mut buf, plaintext.len())
            .map_err(pad_err)?
            .len(),
        32 => cbc::Encryptor::<Aes256>::new_from_slices(key, &ZERO_IV)
            .map_err(key_error)?
            .encrypt_padded_mut::<Pkcs7>(&mut buf, plaintext.len())
            .map_err(pad_err)?
            .len(),
        n => return Err(bad_key_len(n)),
    };
    buf.truncate(len);
    Ok(buf)
}

/// Inverse of [`aes_cbc_encrypt`].
pub fn aes_cbc_decrypt(key: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>> {
    let mut buf = ciphertext.to_vec();
    let unpad_err = |_| Error::Crypto("AES unpad failed (wrong key?)".into());
    let len = match key.len() {
        16 => cbc::Decryptor::<Aes128>::new_from_slices(key, &ZERO_IV)
            .map_err(key_error)?
            .decrypt_padded_mut::<Pkcs7>(&mut buf)
            .map_err(unpad_err)?
            .len(),
        32 => cbc::Decryptor::<Aes256>::new_from_slices(key, &ZERO_IV)
            .map_err(key_error)?
            .decrypt_padded_mut::<Pkcs7>(&mut buf)
            .map_err(unpad_err)?
            .len(),
        n => return Err(bad_key_len(n)),
    };
    buf.truncate(len);
    Ok(buf)
}

/// `encodeAES`: [`aes_cbc_encrypt`] with base64 output.
pub fn encrypt_b64(key: &[u8], plaintext: &str) -> Result<String> {
    Ok(B64.encode(aes_cbc_encrypt(key, plaintext.as_bytes())?))
}

/// `decodeAES`: base64 input, UTF-8 output.
pub fn decrypt_b64(key: &[u8], b64: &str) -> Result<String> {
    let raw = B64
        .decode(b64.trim())
        .map_err(|e| Error::Crypto(format!("base64: {e}")))?;
    String::from_utf8(aes_cbc_decrypt(key, &raw)?).map_err(|e| Error::Crypto(format!("UTF-8: {e}")))
}

/// Decode `zpw_enk` (base64 of a 16- or 32-byte AES key).
pub fn decode_session_key(zpw_enk: &str) -> Result<Vec<u8>> {
    let raw = B64
        .decode(zpw_enk.trim())
        .map_err(|e| Error::Crypto(format!("zpw_enk is not base64: {e}")))?;
    match raw.len() {
        16 | 32 => Ok(raw),
        n => Err(Error::Crypto(format!(
            "zpw_enk decodes to {n} bytes, expected 16 or 32"
        ))),
    }
}

pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

pub(crate) fn hex_upper(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02X}");
    }
    out
}

/// Lower-case hex MD5.
pub fn md5_hex(s: &str) -> String {
    hex_lower(&Md5::digest(s.as_bytes()))
}

/// How a JSON value is written into a query string and into the signature.
pub(crate) fn param_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `getSignKey(tag, params)` = md5("zsecure" + tag + values sorted by key name).
pub fn sign_key(tag: &str, params: &Map<String, Value>) -> String {
    let mut keys: Vec<&String> = params.keys().collect();
    keys.sort();
    let mut s = String::from("zsecure");
    s.push_str(tag);
    for k in keys {
        s.push_str(&param_value(&params[k]));
    }
    md5_hex(&s)
}

/// `len` random lower-case hex characters.
pub fn random_hex(len: usize) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| HEX[rng.gen_range(0..16)] as char)
        .collect()
}

/// Random RFC 4122 version-4 UUID.
pub fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    rand::thread_rng().fill(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex_lower(&b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// `encodeURIComponent`.
pub fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => out.push(b as char),
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Expected values computed with OpenSSL:
    // printf '%s' '{"a":1}' | openssl enc -aes-128-cbc -K <hex of "0123456789abcdef"> -iv 0 | base64
    // printf '%s' 'zalo' | openssl enc -aes-256-cbc -K <64 zeros> -iv 0 | base64

    #[test]
    fn aes128_matches_openssl() {
        let key = b"0123456789abcdef";
        assert_eq!(
            encrypt_b64(key, r#"{"a":1}"#).unwrap(),
            "4Bu2yOikMUYfFkhodEsv7w=="
        );
        assert_eq!(
            decrypt_b64(key, "4Bu2yOikMUYfFkhodEsv7w==").unwrap(),
            r#"{"a":1}"#
        );
    }

    #[test]
    fn aes256_matches_openssl() {
        let key = [0u8; 32];
        assert_eq!(
            encrypt_b64(&key, "zalo").unwrap(),
            "U4QZztwhqmZvALKgpq69oQ=="
        );
        assert_eq!(
            decrypt_b64(&key, "U4QZztwhqmZvALKgpq69oQ==").unwrap(),
            "zalo"
        );
    }

    #[test]
    fn rejects_other_key_lengths() {
        assert!(encrypt_b64(&[0u8; 24], "x").is_err());
        assert!(aes_cbc_decrypt(&[0u8; 8], &[0u8; 16]).is_err());
    }

    #[test]
    fn md5_known_vector() {
        assert_eq!(md5_hex("abc"), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn signkey_sorts_keys_and_concatenates_values() {
        let mut m = Map::new();
        m.insert("b".into(), json!("2"));
        m.insert("a".into(), json!(1));
        // md5("zsecure" + "10053" + "1" + "2"), checked with md5sum.
        assert_eq!(sign_key("10053", &m), "815e0c6031b6e8c37d0c4babb2b02337");
    }

    #[test]
    fn session_key_must_be_16_or_32_bytes() {
        assert_eq!(
            decode_session_key(&B64.encode([7u8; 16])).unwrap().len(),
            16
        );
        assert_eq!(
            decode_session_key(&B64.encode([7u8; 32])).unwrap().len(),
            32
        );
        assert!(decode_session_key(&B64.encode([7u8; 20])).is_err());
        assert!(decode_session_key("not base64!").is_err());
    }

    #[test]
    fn url_encode_matches_encode_uri_component() {
        assert_eq!(url_encode("a+b/c=d é"), "a%2Bb%2Fc%3Dd%20%C3%A9");
        assert_eq!(url_encode("A-z_0.!~*'()"), "A-z_0.!~*'()");
    }

    #[test]
    fn uuid_v4_has_version_and_variant() {
        let u = uuid_v4();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        assert!("89ab".contains(&u[19..20]));
    }

    #[test]
    fn random_hex_shape() {
        let h = random_hex(10);
        assert_eq!(h.len(), 10);
        assert!(h
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }
}
