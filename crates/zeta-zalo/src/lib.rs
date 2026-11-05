//! `zeta-zalo`: unofficial client for the Zalo Web/PC protocol.
//!
//! This crate is the protocol core of Zeta Chat: QR login, session persistence,
//! request encryption and signing, a first set of chat APIs and the real-time
//! WebSocket transport. It has no UI and stores no chat history; the gateway
//! builds on top of it.
//!
//! Large parts are ported from ctdozlite (MIT OR Apache-2.0), see `NOTICE`.
//! Behaviour that differs on purpose is documented where it happens and in
//! `docs/GIAO_THUC.md`.

pub mod alias;
pub mod api;
pub mod board;
pub mod card;
pub mod client;
pub mod crypto;
pub mod domains;
pub mod error;
pub mod friend;
pub mod login;
pub mod media;
pub mod message;
pub mod param_cipher;
pub mod quick;
pub mod reaction;
pub mod receipt;
pub mod session;
pub mod socket;
pub mod sticker;
pub mod typing;
pub mod upload;
pub mod vietqr;

pub use client::ZaloClient;
pub use domains::{Domains, Service};
pub use error::{Error, Result};
pub use session::{Device, SessionData};

/// `zpw_ver` sent with every request (Zalo PC 26.10.x bundle).
pub const ZPW_VER: &str = "694";

/// `zpw_type`: 30 is the PC/Web client.
pub const ZPW_TYPE: &str = "30";

/// User-Agent of the Zalo PC Electron shell. The phone's login confirmation shows
/// it as "Chrome 12x - Windows 10".
pub const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
(KHTML, like Gecko) ZaloPC/26.10.11 Chrome/122.0.0.0 Electron/29.0.0 Safari/537.36";

/// Lower case without Vietnamese accents ("Đà Nẵng" → "da nang"), for matching
/// what people type against names and texts.
pub fn fold(s: &str) -> String {
    s.chars()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            'à' | 'á' | 'ạ' | 'ả' | 'ã' | 'â' | 'ầ' | 'ấ' | 'ậ' | 'ẩ' | 'ẫ' | 'ă' | 'ằ' | 'ắ'
            | 'ặ' | 'ẳ' | 'ẵ' => 'a',
            'è' | 'é' | 'ẹ' | 'ẻ' | 'ẽ' | 'ê' | 'ề' | 'ế' | 'ệ' | 'ể' | 'ễ' => {
                'e'
            }
            'ì' | 'í' | 'ị' | 'ỉ' | 'ĩ' => 'i',
            'ò' | 'ó' | 'ọ' | 'ỏ' | 'õ' | 'ô' | 'ồ' | 'ố' | 'ộ' | 'ổ' | 'ỗ' | 'ơ' | 'ờ' | 'ớ'
            | 'ợ' | 'ở' | 'ỡ' => 'o',
            'ù' | 'ú' | 'ụ' | 'ủ' | 'ũ' | 'ư' | 'ừ' | 'ứ' | 'ự' | 'ử' | 'ữ' => {
                'u'
            }
            'ỳ' | 'ý' | 'ỵ' | 'ỷ' | 'ỹ' => 'y',
            'đ' => 'd',
            other => other,
        })
        .collect()
}

/// Unix time in seconds.
pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Unix time in milliseconds, also used as client message id.
pub(crate) fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
