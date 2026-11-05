//! Media on Zalo's CDN: the photos, videos, voice messages, files and stickers
//! that messages point at.
//!
//! Verified live (November 2026): the CDN answers without the session cookie;
//! photos come from `photo-stal-*.zdn.vn` labelled `image/jpg`, videos from
//! `video-stal-*.dlmd.me` (with range support), stickers from `api.zaloapp.com`
//! and `stc-chat.zdn.vn`, avatars from `*.zadn.vn`.

use std::path::Path;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE, REFERER};
use reqwest::{redirect, Url};
use tokio::io::AsyncWriteExt;

use crate::error::{Error, Result};
use crate::USER_AGENT;

/// Domains Zalo serves media from; subdomains included.
pub const MEDIA_DOMAINS: &[&str] = &[
    "zdn.vn",
    "zadn.vn",
    "dlmd.me",
    // Files of hundreds of MB (seen live: `bfg30.dlfl.vn`).
    "dlfl.vn",
    "zaloapp.com",
    "zalo.me",
];

/// Whether `url` points at Zalo's media CDN. Only such URLs are downloaded, so a
/// crafted message cannot make the gateway fetch anything else (hosts on its own
/// LAN, for a start): no IP addresses, no explicit ports, no credentials.
pub fn is_media_url(url: &str) -> bool {
    let Ok(url) = Url::parse(url) else {
        return false;
    };
    if !matches!(url.scheme(), "https" | "http")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return false;
    }
    // `domain()` is `None` for IP addresses.
    let Some(host) = url.domain() else {
        return false;
    };
    let host = host.trim_end_matches('.');
    MEDIA_DOMAINS.iter().any(|domain| {
        host == *domain
            || host
                .strip_suffix(domain)
                .is_some_and(|sub| sub.ends_with('.'))
    })
}

/// Host part of a URL, for logs and errors (paths and queries stay out of them).
fn host_of(url: &str) -> String {
    Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default()
}

/// A file fetched from the CDN.
#[derive(Clone, Debug)]
pub struct Downloaded {
    /// `Content-Type` sent by the CDN: possibly empty or non-standard.
    pub content_type: String,
    pub size: u64,
}

/// HTTP client for the media CDN. It has no cookie store, so the session cookie
/// never reaches media hosts.
#[derive(Clone)]
pub struct MediaClient {
    http: reqwest::Client,
}

impl MediaClient {
    pub fn new() -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(REFERER, HeaderValue::from_static("https://chat.zalo.me/"));
        // Redirects may only lead to other media hosts.
        let policy = redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 {
                attempt.error("too many redirects")
            } else if is_media_url(attempt.url().as_str()) {
                attempt.follow()
            } else {
                attempt.stop()
            }
        });
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .default_headers(headers)
            .redirect(policy)
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(60))
            .build()?;
        Ok(Self { http })
    }

    /// Download `url` to `dest`, refusing files over `max_bytes`. The data goes to
    /// a temporary file renamed into place when complete, so `dest` is never a
    /// partial file.
    pub async fn download(&self, url: &str, dest: &Path, max_bytes: u64) -> Result<Downloaded> {
        if !is_media_url(url) {
            return Err(Error::Protocol("not a Zalo media URL".into()));
        }
        let mut resp = self.http.get(url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Status {
                status: status.as_u16(),
                endpoint: host_of(url),
            });
        }
        if resp.content_length().is_some_and(|n| n > max_bytes) {
            return Err(Error::TooLarge { limit: max_bytes });
        }
        let content_type = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let name = dest
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| Error::Protocol("bad download destination".into()))?;
        let tmp = dest.with_file_name(format!("{name}.part{:08x}", rand::random::<u32>()));
        let mut file = tokio::fs::File::create(&tmp).await?;
        let written = async {
            let mut size = 0u64;
            while let Some(chunk) = resp.chunk().await? {
                size += chunk.len() as u64;
                if size > max_bytes {
                    return Err(Error::TooLarge { limit: max_bytes });
                }
                file.write_all(&chunk).await?;
            }
            file.flush().await?;
            Ok(size)
        }
        .await;
        drop(file);
        let renamed = match written {
            Ok(size) => tokio::fs::rename(&tmp, dest)
                .await
                .map(|()| size)
                .map_err(Error::from),
            Err(e) => Err(e),
        };
        match renamed {
            Ok(size) => Ok(Downloaded { content_type, size }),
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_zalo_media_hosts_are_allowed() {
        for ok in [
            "https://photo-stal-24.zdn.vn/jpg/a/b.jpg",
            "https://video-stal-43.dlmd.me/x/y.mp4",
            "https://bfg30.dlfl.vn/a/b.bin",
            "https://s120-ava-talk.zadn.vn/a/b.jpg",
            "https://api.zaloapp.com/api/emoticon/sticker/webpc?eid=1&size=130",
            "http://stc-chat.zdn.vn/images/stickers/default/thumb/7.png",
            "https://PHOTO.ZDN.VN/a.jpg",
            "https://zdn.vn/a",
        ] {
            assert!(is_media_url(ok), "{ok}");
        }
        for bad in [
            "https://example.com/a.jpg",
            "https://evilzdn.vn/a.jpg",
            "https://zdn.vn.example.com/a.jpg",
            "https://photo.zdn.vn:8443/a.jpg",
            "https://user:pw@photo.zdn.vn/a.jpg",
            "https://127.0.0.1/a.jpg",
            "http://192.168.1.10/a.jpg",
            "https://[::1]/a.jpg",
            "ftp://photo.zdn.vn/a.jpg",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "/api/v1/status",
            "",
        ] {
            assert!(!is_media_url(bad), "{bad}");
        }
    }

    #[test]
    fn host_is_all_that_errors_mention() {
        assert_eq!(host_of("https://a.zdn.vn/p/q?sig=secret"), "a.zdn.vn");
        assert_eq!(host_of("nonsense"), "");
    }
}
