//! Cards a message can carry: a contact ("danh thiếp") and a bank account
//! ("số tài khoản"), and the bank accounts the account keeps in Zalo.
//!
//! As zca-js does it:
//!
//! - Contact: `POST {friend}/api/friend/mget-qr` with `{ fids: [uid] }`
//!   answers `{ "<uid>": "<QR picture URL>" }`. The card goes through
//!   `POST {file}/api/message/forward` (1-1: `toId`, `imei`) or
//!   `{file}/api/group/forward` (`grid`, `visibility` 0) with `msgType` 6 and
//!   `msgInfo` the JSON text of `{ contactUid, qrCodeUrl, phone? }`. Seen live: it
//!   arrives as a `chat.recommended` message whose `content` is `{ action:
//!   "recommened.user", title: <name>, thumb: <avatar>, params: <uid>,
//!   description: JSON text of { qrCodeUrl, gUid, phone? } }`.
//! - Banks Zalo knows: `GET {zimsg}/api/transfer/conf` with `{}` answers
//!   `{ banks: [{ bin, logo, name, name_eng, short_name, search_key_word }] }`.
//! - Saved accounts: `GET {zimsg}/api/transfer/list` with `{ page, limit }`
//!   answers `{ hasMore, total, myBanks: [{ bin, bank_number, holder_name,
//!   account_id, is_default, … }] }`; `POST /api/transfer/create` with `{ bin,
//!   bank_number, holder_name, language }`; `POST /api/transfer/delete` with
//!   `{ account_id, is_default, language }`.
//! - Sending an account: `POST {zimsg}/api/transfer/card` with `{ binBank,
//!   numAccBank, nameAccBank, cliMsgId, tsMsg, destUid, destType }` (`destType`
//!   0 for a person, 1 for a group). Seen live: the card arrives as a
//!   `chat.webcontent` message whose `content.action` is `zinstant.bankcard`;
//!   what it shows is in a template on Zalo's CDN (`params.pcItem.data_url`,
//!   HTML for the PC client), read by [`template_card`].

use std::io::Read;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::api::{first_str, Recipient, SentMessage};
use crate::client::ZaloClient;
use crate::domains::Service;
use crate::error::{Error, Result};
use crate::{fold, now_millis};

/// A bank Zalo can send accounts of.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bank {
    /// NAPAS code of the bank (6 digits), what VietQR codes name it by.
    pub bin: String,
    pub name: String,
    /// What people call it: `Vietcombank`, `MB`, `TPBank`…
    pub short_name: String,
    /// Its logo on Zalo's CDN; may be empty.
    pub logo: String,
    /// Other names people write, as Zalo lists them.
    pub keywords: String,
}

/// A bank account the account saved in Zalo.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BankAccount {
    /// Zalo's id of the saved account, to delete it.
    pub id: u64,
    pub bin: String,
    pub number: String,
    /// The holder's name, in capitals without accents as banks write it.
    pub holder: String,
    pub default: bool,
}

/// `fids`'s QR pictures of an `mget-qr` answer.
fn qr_of(data: &Value, uid: &str) -> String {
    match data.get(uid) {
        Some(Value::String(s)) => s.trim().to_string(),
        // Some answers nest it.
        Some(other) => first_str(other, &["qr", "url"]),
        None => String::new(),
    }
}

fn number(v: &Value, key: &str) -> u64 {
    match v.get(key) {
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

fn flag(v: &Value, key: &str) -> bool {
    match v.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0) != 0,
        Some(Value::String(s)) => matches!(s.trim(), "1" | "true"),
        _ => false,
    }
}

/// The banks of a `transfer/conf` answer.
fn banks_of(data: &Value) -> Vec<Bank> {
    let Some(list) = data.get("banks").and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|b| {
            let bin = first_str(b, &["bin"]);
            let short_name = first_str(b, &["short_name", "shortName", "name"]);
            (!bin.is_empty() && !short_name.is_empty()).then(|| Bank {
                bin,
                name: first_str(b, &["name", "name_eng"]),
                short_name,
                logo: first_str(b, &["logo"]).replace("http://", "https://"),
                keywords: first_str(b, &["search_key_word"]),
            })
        })
        .collect()
}

/// The accounts of a `transfer/list` (or `transfer/delete`) answer, and
/// whether Zalo has more.
fn accounts_of(data: &Value) -> (Vec<BankAccount>, bool) {
    let list = data
        .get("myBanks")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let accounts = list
        .iter()
        .filter_map(|a| {
            let number = first_str(a, &["bank_number"]);
            let bin = first_str(a, &["bin"]);
            (!number.is_empty() && !bin.is_empty()).then(|| BankAccount {
                id: number_or(a, &["account_id", "id"]),
                bin,
                number,
                holder: first_str(a, &["holder_name"]),
                default: flag(a, "is_default") || flag(a, "default"),
            })
        })
        .collect();
    (accounts, flag(data, "hasMore"))
}

fn number_or(v: &Value, keys: &[&str]) -> u64 {
    keys.iter()
        .map(|k| number(v, k))
        .find(|n| *n != 0)
        .unwrap_or(0)
}

/// What a bank card's template shows (`pcItem`, HTML).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateCard {
    pub bin: String,
    pub number: String,
    pub holder: String,
    /// As the card writes it: "Ngân hàng Vietcombank".
    pub bank_name: String,
}

/// `&amp;` and friends, the few a template uses.
fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// `<p …>text</p>` elements of an HTML text: (attributes, text).
fn paragraphs(html: &str) -> Vec<(&str, String)> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(at) = rest.find("<p") {
        rest = &rest[at + 2..];
        if !rest.starts_with([' ', '>']) {
            continue;
        }
        let Some(close) = rest.find('>') else { break };
        let attrs = &rest[..close];
        rest = &rest[close + 1..];
        let Some(end) = rest.find("</p>") else { break };
        let text = &rest[..end];
        if !text.contains('<') {
            out.push((attrs, unescape(text.trim())));
        }
        rest = &rest[end + 4..];
    }
    out
}

/// What a received bank card shows, from its template (seen live, November
/// 2026): the VietQR payload in its `bankcard-action.zalo.me?…&content=`
/// links gives the bank and the account; paragraphs give the holder
/// (`pc_element`), the bank's name and, failing the payload, the BIN
/// (`id="bank_bin"`) and the number (`number_text`).
pub fn template_card(html: &str) -> Option<TemplateCard> {
    let mut card = TemplateCard::default();
    if let Some(at) = html.find("bankcard-action.zalo.me") {
        let tail = &html[at..];
        if let Some(c) = tail.find("content=") {
            let payload: String = tail[c + 8..]
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '.')
                .collect();
            if let Some((bin, number)) = crate::vietqr::parse(&payload) {
                card.bin = bin;
                card.number = number;
            }
        }
    }
    for (attrs, text) in paragraphs(html) {
        let class = attrs
            .split("class=\"")
            .nth(1)
            .and_then(|c| c.split('"').next())
            .unwrap_or("");
        if attrs.contains("id=\"bank_bin\"") {
            if card.bin.is_empty() {
                card.bin = text;
            }
        } else if class.contains("number_text") {
            if card.number.is_empty() {
                card.number = text.chars().filter(|c| !c.is_whitespace()).collect();
            }
        } else if class.contains("pc_element") {
            if card.holder.is_empty() {
                card.holder = text;
            }
        } else if class.contains("normal_text")
            && !class.contains("mobile_element")
            && card.bank_name.is_empty()
        {
            card.bank_name = text;
        }
    }
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    (digits(&card.bin) && digits(&card.number)).then_some(card)
}

/// A holder's name the way banks (and Zalo, as zca-js
/// `normalizeHolderName`) write it: capitals without accents, letters and
/// digits only. `None` when fewer than 5 characters remain.
pub fn holder_name(input: &str) -> Option<String> {
    let plain: String = fold(input)
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                ' '
            }
        })
        .collect();
    let name = plain.split_whitespace().collect::<Vec<_>>().join(" ");
    (name.chars().count() >= 5).then_some(name)
}

/// An account number as banks take it: 6 to 19 digits (spaces, dots and
/// dashes between groups are dropped).
pub fn account_number(input: &str) -> Option<String> {
    let digits: String = input
        .chars()
        .filter(|c| !matches!(c, ' ' | '.' | '-'))
        .collect();
    let ok = (6..=19).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit());
    ok.then_some(digits)
}

impl ZaloClient {
    /// The QR picture Zalo draws for a user's profile (empty when it has none).
    pub async fn profile_qr(&self, uid: &str) -> Result<String> {
        let key = self.session_key()?;
        let form = format!("params={}", self.encrypt_params(&json!({ "fids": [uid] }))?);
        let data = self
            .post_service(Service::Friend, "/api/friend/mget-qr", form, Some(key))
            .await?;
        Ok(qr_of(&data, uid))
    }

    /// Send `uid`'s contact card ("danh thiếp") with its profile QR (see
    /// [`Self::profile_qr`]; the official clients show the card without one
    /// too); `phone` is shown on it when given.
    pub async fn send_contact_card(
        &self,
        to: Recipient<'_>,
        uid: &str,
        phone: &str,
        qr: &str,
    ) -> Result<SentMessage> {
        let key = self.session_key()?;
        let mut info = json!({ "contactUid": uid, "qrCodeUrl": qr });
        if !phone.trim().is_empty() {
            info["phone"] = json!(phone.trim());
        }
        let client_id = now_millis();
        let mut params = json!({
            "ttl": 0,
            "msgType": 6,
            "clientId": client_id.to_string(),
            "msgInfo": info.to_string(),
        });
        let path = match to {
            Recipient::User(uid) => {
                params["toId"] = json!(uid);
                params["imei"] = json!(self.device().imei);
                "/api/message/forward"
            }
            Recipient::Group(grid) => {
                params["grid"] = json!(grid);
                params["visibility"] = json!(0);
                "/api/group/forward"
            }
        };
        let form = format!("params={}", self.encrypt_params(&params)?);
        let url = format!(
            "{}{path}?{}",
            self.domains().primary(Service::File),
            Self::common_params()
        );
        let data = self.post_form(&url, form, Some(key)).await?;
        Ok(SentMessage {
            msg_id: first_str(&data, &["msgId"]),
            client_id,
        })
    }

    /// The banks Zalo can send accounts of.
    pub async fn banks(&self) -> Result<Vec<Bank>> {
        let data = self
            .get_encrypted(Service::Zimsg, "/api/transfer/conf", &json!({}))
            .await?;
        Ok(banks_of(&data))
    }

    /// The bank accounts saved in Zalo (at most 100).
    pub async fn bank_accounts(&self) -> Result<Vec<BankAccount>> {
        let mut all = Vec::new();
        for page in 0..5 {
            let data = self
                .get_encrypted(
                    Service::Zimsg,
                    "/api/transfer/list",
                    &json!({ "page": page, "limit": 20 }),
                )
                .await?;
            let (found, more) = accounts_of(&data);
            let empty = found.is_empty();
            all.extend(found);
            if !more || empty {
                break;
            }
        }
        Ok(all)
    }

    /// Save a bank account in Zalo.
    pub async fn add_bank_account(&self, bin: &str, number: &str, holder: &str) -> Result<()> {
        let key = self.session_key()?;
        let holder = holder_name(holder)
            .ok_or_else(|| Error::Protocol("tên chủ tài khoản quá ngắn".into()))?;
        let number = account_number(number)
            .ok_or_else(|| Error::Protocol("số tài khoản phải có 6 đến 19 chữ số".into()))?;
        let bin: u64 = bin
            .trim()
            .parse()
            .map_err(|_| Error::Protocol("mã ngân hàng không hợp lệ".into()))?;
        let form = format!(
            "params={}",
            self.encrypt_params(&json!({
                "bin": bin,
                "bank_number": number,
                "holder_name": holder,
                "language": "vi",
            }))?
        );
        match self
            .post_service(Service::Zimsg, "/api/transfer/create", form, Some(key))
            .await
        {
            Ok(_) | Err(Error::EmptyResponse { .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Delete a saved bank account.
    pub async fn remove_bank_account(&self, account: &BankAccount) -> Result<()> {
        let key = self.session_key()?;
        let form = format!(
            "params={}",
            self.encrypt_params(&json!({
                "account_id": account.id,
                "is_default": account.default,
                "language": "vi",
            }))?
        );
        match self
            .post_service(Service::Zimsg, "/api/transfer/delete", form, Some(key))
            .await
        {
            Ok(_) | Err(Error::EmptyResponse { .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// What a received bank card shows: its template (`url`, on Zalo's CDN)
    /// read with [`template_card`].
    pub async fn read_bank_card(&self, url: &str) -> Result<TemplateCard> {
        let bytes = self.get_bytes(url, 2 * 1024 * 1024).await?;
        let bytes = if bytes.starts_with(&[0x1f, 0x8b]) {
            let mut plain = Vec::new();
            flate2::read::GzDecoder::new(&bytes[..])
                .take(4 * 1024 * 1024)
                .read_to_end(&mut plain)?;
            plain
        } else {
            bytes
        };
        template_card(&String::from_utf8_lossy(&bytes))
            .ok_or_else(|| Error::Protocol("không đọc được thẻ tài khoản ngân hàng".into()))
    }

    /// Send a bank account as a card ("gửi số tài khoản"); Zalo draws the bank,
    /// the number, the holder and a VietQR code. Answers the client id it was
    /// sent with (Zalo answers nothing else).
    pub async fn send_bank_card(
        &self,
        to: Recipient<'_>,
        bin: &str,
        number: &str,
        holder: &str,
    ) -> Result<SentMessage> {
        let key = self.session_key()?;
        let bin: u64 = bin
            .trim()
            .parse()
            .map_err(|_| Error::Protocol("mã ngân hàng không hợp lệ".into()))?;
        let number = account_number(number)
            .ok_or_else(|| Error::Protocol("số tài khoản phải có 6 đến 19 chữ số".into()))?;
        let client_id = now_millis();
        let (dest, dest_type) = match to {
            Recipient::User(uid) => (uid, 0),
            Recipient::Group(grid) => (grid, 1),
        };
        let form = format!(
            "params={}",
            self.encrypt_params(&json!({
                "binBank": bin,
                "numAccBank": number,
                // zca-js: the name in capitals, or "---" without one.
                "nameAccBank": holder_name(holder).unwrap_or_else(|| "---".into()),
                "cliMsgId": client_id.to_string(),
                "tsMsg": client_id,
                "destUid": dest,
                "destType": dest_type,
            }))?
        );
        let url = format!(
            "{}/api/transfer/card?{}",
            self.domains().primary(Service::Zimsg),
            Self::common_params()
        );
        let data = match self.post_form(&url, form, Some(key)).await {
            Ok(data) => data,
            Err(Error::EmptyResponse { .. }) => Value::Null,
            Err(e) => return Err(e),
        };
        Ok(SentMessage {
            msg_id: first_str(&data, &["msgId"]),
            client_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holder_names_like_banks_write_them() {
        assert_eq!(
            holder_name("Nguyễn Văn Đức").as_deref(),
            Some("NGUYEN VAN DUC")
        );
        assert_eq!(
            holder_name("  trần-thị  bé 2 ").as_deref(),
            Some("TRAN THI BE 2")
        );
        assert_eq!(holder_name("An"), None);
        assert_eq!(holder_name(""), None);
    }

    #[test]
    fn account_numbers() {
        assert_eq!(
            account_number("0123 456.789-0").as_deref(),
            Some("01234567890")
        );
        assert_eq!(account_number("12345"), None);
        assert_eq!(account_number("12345678901234567890"), None);
        assert_eq!(account_number("12345a78"), None);
    }

    #[test]
    fn banks_and_accounts() {
        let banks = banks_of(&json!({ "banks": [
            { "bin": 970436, "name": "NH TMCP Ngoại Thương Việt Nam", "short_name": "Vietcombank",
              "logo": "http://x/vcb.png", "search_key_word": "vcb" },
            { "bin": 1 }
        ] }));
        assert_eq!(banks.len(), 1);
        assert_eq!(banks[0].bin, "970436");
        assert_eq!(banks[0].logo, "https://x/vcb.png");

        let (accounts, more) = accounts_of(&json!({ "hasMore": false, "total": 1, "myBanks": [
            { "id": "a1", "account_id": 77, "bin": 970436, "bank_number": "0123456789",
              "holder_name": "NGUYEN VAN A", "is_default": true },
            { "account_id": 78, "bin": 970436 }
        ] }));
        assert!(!more);
        assert_eq!(
            accounts,
            [BankAccount {
                id: 77,
                bin: "970436".into(),
                number: "0123456789".into(),
                holder: "NGUYEN VAN A".into(),
                default: true,
            }]
        );
    }

    #[test]
    fn bank_card_templates() {
        // Cut down from the template of a card sent while testing (a fake
        // Vietcombank account of ten zeros).
        let html = concat!(
            r#"<style>._x_30 { color: #fff; }</style><div id="vw_card">
            <img class="bank_logo">
            <p class="normal_text _x_29">Ngân hàng Vietcombank</p>
            <p class="number_text _x_30">0000000000</p>
            <p class="normal_text pc_element _x_31"> NGUYEN VAN THU </p>
            <p class="normal_text mobile_element _x_32"> NGUYEN VAN THU </p>
            <div class="bank_bin_element"><p id="bank_bin">970436</p></div>
            <zinstant-attr-theme>[{"_x_15":{"data-click-data":"https://bankcard-action.zalo.me?action=save&amp;content="#,
            "00020101021138540010A000000727",
            "01240006970436011000000000000208QRIBFTTA",
            "53037045802VN80300012com.vng.zalo0110ZABANKCARD6304CA3D",
            r#"&amp;sid=X"}}]</zinstant-attr-theme>"#
        );
        assert_eq!(
            template_card(html),
            Some(TemplateCard {
                bin: "970436".into(),
                number: "0000000000".into(),
                holder: "NGUYEN VAN THU".into(),
                bank_name: "Ngân hàng Vietcombank".into(),
            })
        );
        // Without the payload the paragraphs still tell.
        let plain = html.replace("bankcard-action", "elsewhere");
        assert_eq!(
            template_card(&plain).map(|c| c.number),
            Some("0000000000".into())
        );
        assert_eq!(template_card("<p>nothing</p>"), None);
    }

    #[test]
    fn profile_qr_answers() {
        let data = json!({ "12": "https://qr/12.png", "13": { "qr": "https://qr/13.png" } });
        assert_eq!(qr_of(&data, "12"), "https://qr/12.png");
        assert_eq!(qr_of(&data, "13"), "https://qr/13.png");
        assert_eq!(qr_of(&data, "14"), "");
    }
}
