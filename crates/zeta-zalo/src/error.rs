//! Error type shared by the whole crate.

/// `error_code` values meaning Zalo no longer accepts the session cookie.
pub const SESSION_REJECTED_CODES: [i64; 2] = [600, -102];

/// Everything that can go wrong while talking to Zalo.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Transport failure: DNS, TLS, connect, timeout. The URL is stripped from the
    /// inner error because its query string carries request parameters.
    #[error("network error: {0}")]
    Network(reqwest::Error),

    /// Non-2xx HTTP status. `endpoint` never contains the query string.
    #[error("HTTP {status} from {endpoint}")]
    Status { status: u16, endpoint: String },

    /// HTTP 200 with an empty body. Zalo answers like this when it cannot use the
    /// request's session, so it is never a success for a regular API call.
    #[error("empty response from {endpoint}")]
    EmptyResponse { endpoint: String },

    /// Zalo refused the session cookie (`600` / `-102`): a new login is needed.
    #[error("session rejected by Zalo (error {code}): {message}")]
    SessionRejected { code: i64, message: String },

    /// Any other non-zero `error_code`.
    #[error("Zalo error {code}: {message}")]
    Server { code: i64, message: String },

    /// The call needs a logged-in session.
    #[error("not logged in")]
    NotLoggedIn,

    #[error("crypto error: {0}")]
    Crypto(String),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// The answer does not have the expected shape.
    #[error("unexpected response: {0}")]
    Protocol(String),

    #[error("timed out: {0}")]
    Timeout(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("websocket error: {0}")]
    WebSocket(String),

    /// A download went over its size limit.
    #[error("file larger than {limit} bytes")]
    TooLarge { limit: u64 },
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Self::Network(e.without_url())
    }
}

impl Error {
    /// Map a non-zero Zalo `error_code` to the matching variant.
    pub fn from_code(code: i64, message: impl Into<String>) -> Self {
        let message = message.into();
        if SESSION_REJECTED_CODES.contains(&code) {
            Self::SessionRejected { code, message }
        } else {
            Self::Server { code, message }
        }
    }

    /// Zalo no longer accepts the session.
    pub fn is_session_rejected(&self) -> bool {
        matches!(self, Self::SessionRejected { .. })
    }

    /// Worth retrying the same read on another host of the service.
    pub fn is_failover(&self) -> bool {
        match self {
            Self::Network(_) => true,
            Self::Status { status, .. } => *status == 404 || *status >= 500,
            _ => false,
        }
    }

    /// A client-side timeout, expected while long-polling.
    pub(crate) fn is_timeout(&self) -> bool {
        matches!(self, Self::Network(e) if e.is_timeout())
    }
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// `scheme://host/path` of a URL, without the query (which carries request params).
pub(crate) fn endpoint_of(url: &str) -> String {
    url.split('?').next().unwrap_or(url).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_codes_map_to_session_rejected() {
        assert!(Error::from_code(600, "x").is_session_rejected());
        assert!(Error::from_code(-102, "x").is_session_rejected());
        assert!(!Error::from_code(114, "x").is_session_rejected());
    }

    #[test]
    fn failover_only_for_transport_and_host_errors() {
        let status = |s| Error::Status {
            status: s,
            endpoint: String::new(),
        };
        assert!(status(404).is_failover());
        assert!(status(502).is_failover());
        assert!(!status(400).is_failover());
        assert!(!Error::from_code(600, "x").is_failover());
    }

    #[test]
    fn endpoint_drops_the_query() {
        assert_eq!(
            endpoint_of("https://h.example/api/x?params=secret&a=1"),
            "https://h.example/api/x"
        );
    }
}
