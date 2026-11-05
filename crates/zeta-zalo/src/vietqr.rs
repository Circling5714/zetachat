//! VietQR: the EMVCo QR codes Vietnamese banks scan to transfer money to an
//! account (NAPAS 247). A payload is a run of `ID LEN VALUE` fields (two-digit
//! id and length); the account sits under field 38:
//!
//! ```text
//! 00 02 01                  payload format
//! 01 02 11 | 12             static | with an amount
//! 38 .. 00 10 A000000727    NAPAS
//!       01 .. 00 06 <BIN>   the bank
//!             01 .. <acct>  the account
//!       02 08 QRIBFTTA      transfer to an account
//! 53 03 704                 VND
//! 54 .. <amount>            optional
//! 58 02 VN
//! 62 .. 08 .. <purpose>     optional
//! 63 04 <CRC>               CRC-16/CCITT-FALSE of all before, "6304" included
//! ```
//!
//! Zalo's bank cards carry such a payload (seen live, November 2026): the card's
//! template links `https://bankcard-action.zalo.me?action=transfer&content=<payload>`.

/// One `ID LEN VALUE` field.
fn field(id: &str, value: &str) -> String {
    format!("{id}{:02}{value}", value.len())
}

/// CRC-16/CCITT-FALSE (polynomial 0x1021, start 0xFFFF), as EMVCo asks.
fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for byte in data {
        crc ^= u16::from(*byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// What a note in a payload may hold: letters, digits and spaces without
/// accents (banks show nothing else), at most 50.
fn plain_note(note: &str) -> String {
    let plain: String = note
        .chars()
        .map(|c| {
            // Accents off, case kept: "Đi" stays "Di".
            let base = crate::fold(&c.to_string()).chars().next().unwrap_or(' ');
            let base = if c.is_uppercase() {
                base.to_ascii_uppercase()
            } else {
                base
            };
            if base.is_ascii_alphanumeric() {
                base
            } else {
                ' '
            }
        })
        .collect();
    let words: Vec<&str> = plain.split_whitespace().collect();
    words.join(" ").chars().take(50).collect()
}

/// The payload of a QR that transfers to `account` at the bank `bin`,
/// optionally `amount` dong with a note. `None` when the bank or the
/// account is not one a bank would take.
pub fn payload(bin: &str, account: &str, amount: u64, note: &str) -> Option<String> {
    let digits = |s: &str, min: usize, max: usize| {
        (min..=max).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
    };
    if !digits(bin, 6, 6) || !digits(account, 6, 19) {
        return None;
    }
    let beneficiary = field("00", bin) + &field("01", account);
    let napas = field("00", "A000000727") + &field("01", &beneficiary) + &field("02", "QRIBFTTA");
    let mut out = field("00", "01") + &field("01", if amount > 0 { "12" } else { "11" });
    out += &field("38", &napas);
    out += &field("53", "704");
    if amount > 0 {
        out += &field("54", &amount.to_string());
    }
    out += &field("58", "VN");
    let note = plain_note(note);
    if !note.is_empty() {
        out += &field("62", &field("08", &note));
    }
    out += "6304";
    Some(format!("{out}{:04X}", crc16(out.as_bytes())))
}

/// The fields of a run of `ID LEN VALUE`, in order; `None` when it does not
/// split into fields.
fn fields(s: &str) -> Option<Vec<(&str, &str)>> {
    let mut out = Vec::new();
    let mut rest = s;
    while !rest.is_empty() {
        let id = rest.get(..2)?;
        let len: usize = rest.get(2..4)?.parse().ok()?;
        let value = rest.get(4..4 + len)?;
        out.push((id, value));
        rest = &rest[4 + len..];
    }
    Some(out)
}

/// The bank (BIN) and account a VietQR payload transfers to, after checking
/// its CRC.
pub fn parse(payload: &str) -> Option<(String, String)> {
    let payload = payload.trim();
    let (body, crc) = payload.split_at(payload.len().checked_sub(4)?);
    if !body.ends_with("6304") || !payload.is_ascii() {
        return None;
    }
    if format!("{:04X}", crc16(body.as_bytes())) != crc.to_ascii_uppercase() {
        return None;
    }
    let top = fields(&body[..body.len() - 4])?;
    let napas = top.iter().find(|(id, _)| *id == "38")?.1;
    let beneficiary = fields(napas)?.into_iter().find(|(id, _)| *id == "01")?.1;
    let inner = fields(beneficiary)?;
    let get = |want: &str| {
        inner
            .iter()
            .find(|(id, _)| *id == want)
            .map(|(_, v)| v.to_string())
    };
    Some((get("00")?, get("01")?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_of_the_standard_check_string() {
        // CRC-16/CCITT-FALSE of "123456789" is 0x29B1.
        assert_eq!(crc16(b"123456789"), 0x29B1);
    }

    #[test]
    fn payloads_round_trip() {
        let p = payload("970436", "0123456789", 0, "").unwrap();
        assert!(p.starts_with("000201010211"));
        assert!(p.contains("0010A000000727"));
        assert!(p.contains("QRIBFTTA"));
        assert_eq!(
            parse(&p),
            Some(("970436".to_string(), "0123456789".to_string()))
        );
        let with_amount = payload("970422", "1900123456", 150_000, "Tiền cơm tháng 11!").unwrap();
        assert!(with_amount.starts_with("000201010212"));
        assert!(with_amount.contains("5406150000"));
        assert!(with_amount.contains("Tien com thang 11"));
        assert!(parse(&with_amount).is_some());
        assert_eq!(payload("97043", "0123456789", 0, ""), None);
        assert_eq!(payload("970436", "12 34", 0, ""), None);
    }

    #[test]
    fn a_payload_seen_in_a_zalo_bank_card() {
        // The fake account sent to the Cloud thread while testing: Vietcombank,
        // ten zeros, with Zalo's own field 80 (com.vng.zalo, ZABANKCARD).
        let seen = concat!(
            "00020101021138540010A000000727",
            "01240006970436011000000000000208QRIBFTTA",
            "53037045802VN80300012com.vng.zalo0110ZABANKCARD6304CA3D"
        );
        assert_eq!(
            parse(seen),
            Some(("970436".to_string(), "0000000000".to_string()))
        );
        // One digit changed: the CRC no longer matches.
        assert_eq!(parse(&seen.replace("0000000000", "0000000001")), None);
    }
}
