//! ParamCipher v2: per-request parameter encryption used before a session key exists
//! (`getLoginInfo`, password login).
//!
//! Ported from ctdozlite `src/param_cipher.rs` (MIT OR Apache-2.0). The server rejects
//! unencrypted `params` on these endpoints with `18060 Invalid encryption protocol`.

use serde_json::{json, Map, Value};

use crate::crypto;
use crate::error::Result;

/// Static key of the Zalo PC bundle: 32 ASCII characters used as an AES-256 key.
pub const PARAM_CIPHER_KEY: &[u8] = b"3FC4F0D2AB50057BCE0D90D9187A22B1";

/// Encryption material for one request.
pub struct ParamCipher {
    /// `AES(PARAM_CIPHER_KEY, "{type},{imei},{firstLaunchTime}")` as upper-case hex.
    pub zcid: String,
    /// Random hex salt.
    pub zcid_ext: String,
    encrypt_key: String,
}

/// Characters at even and odd positions (`processStr` in the bundle).
fn split_even_odd(s: &str) -> (Vec<char>, Vec<char>) {
    let chars: Vec<char> = s.chars().collect();
    let even = chars.iter().step_by(2).copied().collect();
    let odd = chars.iter().skip(1).step_by(2).copied().collect();
    (even, odd)
}

impl ParamCipher {
    /// `new C({type, imei, firstLaunchTime})`.
    pub fn new(api_type: &str, imei: &str, first_launch_time: i64) -> Result<Self> {
        let plain = format!("{api_type},{imei},{first_launch_time}");
        let zcid = crypto::hex_upper(&crypto::aes_cbc_encrypt(
            PARAM_CIPHER_KEY,
            plain.as_bytes(),
        )?);
        let zcid_ext = crypto::random_hex(10);

        // createEncryptKey(): 8 even chars of md5(zcid_ext), 12 even chars of zcid,
        // 12 chars of reversed odd chars of zcid.
        let md5_upper = crypto::md5_hex(&zcid_ext).to_uppercase();
        let (n_even, _) = split_even_odd(&md5_upper);
        let (a_even, mut a_odd) = split_even_odd(&zcid);
        a_odd.reverse();
        let encrypt_key: String = n_even
            .iter()
            .take(8)
            .chain(a_even.iter().take(12))
            .chain(a_odd.iter().take(12))
            .collect();

        Ok(Self {
            zcid,
            zcid_ext,
            encrypt_key,
        })
    }

    /// AES key for this request. The response `data` decrypts with the same key.
    pub fn key(&self) -> &str {
        &self.encrypt_key
    }

    /// `encryptParam(tag)`: the signed query object.
    ///
    /// The signature covers exactly `{client_version, enc_ver, params, type, zcid,
    /// zcid_ext}`; anything appended later (`nretry`, `ref`) must stay out of it.
    /// `tag` is the endpoint name lower-cased without dashes, e.g. `getlogininfo`.
    pub fn build_body(
        &self,
        api_type: &str,
        api_version: &str,
        payload: &Value,
        tag: &str,
    ) -> Result<Map<String, Value>> {
        let encrypted = crypto::encrypt_b64(self.encrypt_key.as_bytes(), &payload.to_string())?;
        let mut m = Map::new();
        m.insert("zcid".into(), json!(self.zcid));
        m.insert("zcid_ext".into(), json!(self.zcid_ext));
        m.insert("enc_ver".into(), json!("v2"));
        m.insert("params".into(), json!(encrypted));
        m.insert("type".into(), json!(api_type));
        m.insert("client_version".into(), json!(api_version));
        let sig = crypto::sign_key(tag, &m);
        m.insert("signkey".into(), json!(sig));
        Ok(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zcid_is_upper_hex_of_whole_blocks() {
        let p = ParamCipher::new("30", "abc", 1000).unwrap();
        assert_eq!(p.zcid.len() % 32, 0);
        assert!(p
            .zcid
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase()));
    }

    #[test]
    fn zcid_is_deterministic_and_salt_is_not() {
        let a = ParamCipher::new("30", "abc", 1000).unwrap();
        let b = ParamCipher::new("30", "abc", 1000).unwrap();
        assert_eq!(a.zcid, b.zcid);
        assert_eq!(a.zcid_ext.len(), 10);
    }

    #[test]
    fn encrypt_key_is_32_ascii_chars() {
        let p = ParamCipher::new("30", "abc", 1000).unwrap();
        assert_eq!(p.key().len(), 32);
        assert!(p.key().is_ascii());
    }

    #[test]
    fn body_is_signed_and_payload_decrypts_with_the_key() {
        let p = ParamCipher::new("30", "abc", 1000).unwrap();
        let payload = json!({ "imei": "abc", "ts": 1 });
        let body = p.build_body("30", "694", &payload, "getlogininfo").unwrap();
        assert_eq!(body["enc_ver"], "v2");
        assert_eq!(body["type"], "30");
        assert_eq!(body["client_version"], "694");

        let mut unsigned = body.clone();
        let sig = unsigned.remove("signkey").unwrap();
        assert_eq!(sig, json!(crypto::sign_key("getlogininfo", &unsigned)));

        let plain =
            crypto::decrypt_b64(p.key().as_bytes(), body["params"].as_str().unwrap()).unwrap();
        assert_eq!(plain, payload.to_string());
    }

    #[test]
    fn signature_uses_sorted_values() {
        // Sorted keys: client_version, enc_ver, params, type, zcid, zcid_ext.
        let mut m = Map::new();
        m.insert("zcid".into(), json!("A"));
        m.insert("enc_ver".into(), json!("v2"));
        m.insert("type".into(), json!("30"));
        m.insert("params".into(), json!("P"));
        m.insert("client_version".into(), json!("694"));
        m.insert("zcid_ext".into(), json!("B"));
        assert_eq!(
            crypto::sign_key("TAG", &m),
            crypto::md5_hex("zsecureTAG694v2P30AB")
        );
    }
}
