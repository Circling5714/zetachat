//! Media cache: each file of Zalo's CDN is downloaded once, kept on disk and
//! served to the devices from there (with range requests, for video).
//!
//! Files live in `<data>/media/<2 hex>/<sha256 of the URL>`, next to a `.type`
//! file holding the content type they are served with. Zalo media URLs point at
//! content that never changes, so entries are never revalidated; the least
//! recently used ones go when the cache outgrows its budget.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use zeta_zalo::media::{is_media_url, MediaClient};

/// Largest file the gateway downloads.
pub const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;

/// Prefix of files the gateway stored itself (demo uploads): served from the
/// cache only, never downloaded.
pub const LOCAL_URL_PREFIX: &str = "https://local.zetachat.invalid/";

/// Served for everything that is not displayable media, always as an attachment.
pub const ATTACHMENT_TYPE: &str = "application/octet-stream";

/// Types served inline: images, video and audio that browsers play. Anything
/// else (HTML, SVG, PDF…) is served as an attachment, so a file sent in a chat
/// can never run as a page of the gateway's origin.
const INLINE_TYPES: &[&str] = &[
    "image/jpeg",
    "image/png",
    "image/gif",
    "image/webp",
    "image/avif",
    "image/bmp",
    "video/mp4",
    "video/webm",
    "video/quicktime",
    "audio/mpeg",
    "audio/aac",
    "audio/mp4",
    "audio/ogg",
    "audio/wav",
    "audio/webm",
];

/// A cache entry untouched for this long gets its time refreshed when used: the
/// trimming removes the entries with the oldest times first.
const TOUCH_AFTER: Duration = Duration::from_secs(600);

/// Temporary files of downloads older than this were interrupted.
const STALE_PART: Duration = Duration::from_secs(3600);

/// Content type recognised from the first bytes of a file.
pub fn sniff(head: &[u8]) -> Option<&'static str> {
    match head {
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'A', b'V', b'E', ..] => Some("audio/wav"),
        [b'B', b'M', ..] if head.len() >= 14 => Some("image/bmp"),
        [_, _, _, _, b'f', b't', b'y', b'p', brand @ ..] => match brand.get(..4) {
            Some(b"avif" | b"avis") => Some("image/avif"),
            // HEIC photos: most browsers cannot show them.
            Some(b"heic" | b"heix" | b"mif1" | b"msf1") => None,
            Some(b"M4A " | b"M4B ") => Some("audio/mp4"),
            Some(b"qt  ") => Some("video/quicktime"),
            _ => Some("video/mp4"),
        },
        [0x1A, 0x45, 0xDF, 0xA3, ..] => Some("video/webm"),
        [b'O', b'g', b'g', b'S', ..] => Some("audio/ogg"),
        [b'I', b'D', b'3', ..] => Some("audio/mpeg"),
        // ADTS AAC, then MPEG audio frames.
        [0xFF, b, ..] if b & 0xF6 == 0xF0 => Some("audio/aac"),
        [0xFF, b, ..] if b & 0xE0 == 0xE0 => Some("audio/mpeg"),
        _ => None,
    }
}

/// Content type a downloaded file is served with: what its bytes say, else what
/// the CDN declared if that is displayable media, else an attachment.
pub fn serve_type(declared: &str, head: &[u8]) -> &'static str {
    if let Some(sniffed) = sniff(head) {
        return sniffed;
    }
    let declared = declared
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    // Zalo labels its photos `image/jpg`.
    let declared = if declared == "image/jpg" {
        "image/jpeg"
    } else {
        declared.as_str()
    };
    known_type(declared)
}

/// Whether a content type is served inline rather than as an attachment.
pub fn is_inline(content_type: &str) -> bool {
    INLINE_TYPES.contains(&content_type)
}

fn known_type(content_type: &str) -> &'static str {
    INLINE_TYPES
        .iter()
        .copied()
        .find(|t| *t == content_type)
        .unwrap_or(ATTACHMENT_TYPE)
}

fn is_key(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

#[derive(Debug)]
pub enum FetchError {
    /// Not a URL of Zalo's media CDN.
    NotAllowed,
    /// A local file that is not (or no longer) in the cache.
    NotFound,
    /// Over [`MAX_FILE_BYTES`].
    TooLarge,
    /// The CDN refused or failed.
    Upstream(String),
    Io(std::io::Error),
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAllowed => f.write_str("not a Zalo media URL"),
            Self::NotFound => f.write_str("file not in the cache"),
            Self::TooLarge => write!(f, "file larger than {MAX_FILE_BYTES} bytes"),
            Self::Upstream(reason) => write!(f, "media download failed: {reason}"),
            Self::Io(e) => write!(f, "media cache I/O error: {e}"),
        }
    }
}

/// A file in the cache.
#[derive(Clone, Debug)]
pub struct CachedFile {
    pub path: PathBuf,
    pub content_type: &'static str,
}

pub struct MediaCache {
    dir: PathBuf,
    client: MediaClient,
    max_file_bytes: u64,
    /// One lock per URL being downloaded, so concurrent requests share a download.
    inflight: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Background downloads running at once.
    prefetching: Arc<tokio::sync::Semaphore>,
}

impl MediaCache {
    /// A cache in `dir`, created on first use.
    pub fn new(dir: PathBuf) -> zeta_zalo::Result<Self> {
        Ok(Self {
            dir,
            client: MediaClient::new()?,
            max_file_bytes: MAX_FILE_BYTES,
            inflight: Mutex::new(HashMap::new()),
            prefetching: Arc::new(tokio::sync::Semaphore::new(2)),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Download `urls` in the background, two files at a time, so pictures and
    /// videos open at once and stay available if Zalo's links expire.
    pub fn prefetch(self: &Arc<Self>, urls: Vec<String>) {
        if urls.is_empty() {
            return;
        }
        let cache = self.clone();
        tokio::spawn(async move {
            let Ok(_slot) = cache.prefetching.clone().acquire_owned().await else {
                return;
            };
            for url in urls {
                if let Err(e) = cache.get(&url).await {
                    tracing::debug!(error = %e, "media prefetch failed");
                }
            }
        });
    }

    /// Cache key, data file and type file of a URL.
    fn paths(&self, url: &str) -> (String, PathBuf, PathBuf) {
        let key: String = Sha256::digest(url.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let shard = self.dir.join(&key[..2]);
        let data = shard.join(&key);
        let meta = shard.join(format!("{key}.type"));
        (key, data, meta)
    }

    /// The cached copy of `url`, downloaded first when missing.
    pub async fn get(&self, url: &str) -> Result<CachedFile, FetchError> {
        if url.starts_with(LOCAL_URL_PREFIX) {
            let (_, data, meta) = self.paths(url);
            return lookup(&data, &meta).await.ok_or(FetchError::NotFound);
        }
        if !is_media_url(url) {
            return Err(FetchError::NotAllowed);
        }
        let (key, data, meta) = self.paths(url);
        if let Some(hit) = lookup(&data, &meta).await {
            return Ok(hit);
        }
        let gate = self
            .inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key.clone())
            .or_default()
            .clone();
        let result = {
            let _turn = gate.lock().await;
            match lookup(&data, &meta).await {
                Some(hit) => Ok(hit),
                None => self.download(url, &data, &meta).await,
            }
        };
        let mut inflight = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
        // Only the map and this request still hold the lock: nobody is waiting.
        if Arc::strong_count(&gate) <= 2 {
            inflight.remove(&key);
        }
        result
    }

    async fn download(
        &self,
        url: &str,
        data: &Path,
        meta: &Path,
    ) -> Result<CachedFile, FetchError> {
        if let Some(shard) = data.parent() {
            tokio::fs::create_dir_all(shard)
                .await
                .map_err(FetchError::Io)?;
        }
        let got = self
            .client
            .download(url, data, self.max_file_bytes)
            .await
            .map_err(|e| match e {
                zeta_zalo::Error::TooLarge { .. } => FetchError::TooLarge,
                zeta_zalo::Error::Io(e) => FetchError::Io(e),
                other => FetchError::Upstream(other.to_string()),
            })?;
        let head = read_head(data).await.map_err(FetchError::Io)?;
        let content_type = serve_type(&got.content_type, &head);
        tokio::fs::write(meta, content_type)
            .await
            .map_err(FetchError::Io)?;
        tracing::debug!(size = got.size, content_type, "media cached");
        Ok(CachedFile {
            path: data.to_path_buf(),
            content_type,
        })
    }

    /// Store `bytes` as the cached copy of `url`, for tests and sample data.
    pub async fn insert(
        &self,
        url: &str,
        bytes: &[u8],
        declared: &str,
    ) -> std::io::Result<CachedFile> {
        let (_, data, meta) = self.paths(url);
        if let Some(shard) = data.parent() {
            tokio::fs::create_dir_all(shard).await?;
        }
        tokio::fs::write(&data, bytes).await?;
        let content_type = serve_type(declared, bytes);
        tokio::fs::write(&meta, content_type).await?;
        Ok(CachedFile {
            path: data,
            content_type,
        })
    }

    /// Store a copy of the file at `src` as the cached copy of `url` (a file this
    /// gateway sent, so the sender sees it without downloading it back).
    pub async fn insert_file(
        &self,
        url: &str,
        src: &Path,
        declared: &str,
    ) -> std::io::Result<CachedFile> {
        let (key, data, meta) = self.paths(url);
        if let Some(shard) = data.parent() {
            tokio::fs::create_dir_all(shard).await?;
        }
        // Copied next to its place, then renamed: readers never see half a file.
        let part = data.with_file_name(format!("{key}.part{:08x}", rand::random::<u32>()));
        if let Err(e) = tokio::fs::copy(src, &part).await {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(e);
        }
        let head = read_head(&part).await?;
        tokio::fs::rename(&part, &data).await?;
        let content_type = serve_type(declared, &head);
        tokio::fs::write(&meta, content_type).await?;
        Ok(CachedFile {
            path: data,
            content_type,
        })
    }

    /// Delete the least recently used files until the cache holds at most
    /// `budget` bytes; also clears leftovers of interrupted downloads. Returns the
    /// number of files removed and the bytes freed. Blocking: run it off the
    /// async workers.
    pub fn trim(&self, budget: u64) -> std::io::Result<(usize, u64)> {
        let shards = match std::fs::read_dir(&self.dir) {
            Ok(shards) => shards,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0)),
            Err(e) => return Err(e),
        };
        let mut files = Vec::new();
        for shard in shards {
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(shard.path())? {
                let entry = entry?;
                let info = entry.metadata()?;
                if !info.is_file() {
                    continue;
                }
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let modified = info.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                if is_key(&name) {
                    files.push((modified, info.len(), entry.path()));
                } else if name.contains(".part")
                    && modified.elapsed().is_ok_and(|age| age > STALE_PART)
                {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        let mut total: u64 = files.iter().map(|(_, len, _)| len).sum();
        files.sort_by_key(|(modified, _, _)| *modified);
        let (mut removed, mut freed) = (0, 0);
        for (_, len, path) in files {
            if total <= budget {
                break;
            }
            if std::fs::remove_file(&path).is_err() {
                continue;
            }
            let _ = std::fs::remove_file(path.with_extension("type"));
            total -= len;
            removed += 1;
            freed += len;
        }
        Ok((removed, freed))
    }
}

async fn read_head(path: &Path) -> std::io::Result<Vec<u8>> {
    let file = tokio::fs::File::open(path).await?;
    let mut head = Vec::with_capacity(16);
    file.take(16).read_to_end(&mut head).await?;
    Ok(head)
}

async fn lookup(data: &Path, meta: &Path) -> Option<CachedFile> {
    let stored = tokio::fs::read_to_string(meta).await.ok()?;
    let info = tokio::fs::metadata(data).await.ok()?;
    if !info.is_file() {
        return None;
    }
    let stale = info
        .modified()
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age > TOUCH_AFTER);
    if stale {
        let path = data.to_path_buf();
        // Best effort, nobody waits for it.
        drop(tokio::task::spawn_blocking(move || {
            std::fs::File::options()
                .write(true)
                .open(path)?
                .set_modified(SystemTime::now())
        }));
    }
    Some(CachedFile {
        path: data.to_path_buf(),
        content_type: known_type(stored.trim()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("zetachat-media-{:016x}", rand::random::<u64>()))
    }

    #[test]
    fn sniffing_recognises_common_media() {
        let cases: &[(&[u8], Option<&str>)] = &[
            (b"\xFF\xD8\xFF\xE0\x00\x10JFIF", Some("image/jpeg")),
            (b"\x89PNG\r\n\x1a\n", Some("image/png")),
            (b"GIF89a", Some("image/gif")),
            (b"RIFF\x00\x00\x00\x00WEBPVP8 ", Some("image/webp")),
            (b"\x00\x00\x00\x18ftypmp42", Some("video/mp4")),
            (b"\x00\x00\x00\x18ftypisom", Some("video/mp4")),
            (b"\x00\x00\x00\x18ftypM4A ", Some("audio/mp4")),
            (b"\x00\x00\x00\x18ftypheic", None),
            (b"\x1A\x45\xDF\xA3", Some("video/webm")),
            (b"ID3\x04", Some("audio/mpeg")),
            (b"\xFF\xF1\x50\x80", Some("audio/aac")),
            (b"\xFF\xFB\x90\x00", Some("audio/mpeg")),
            (b"OggS\x00", Some("audio/ogg")),
            (b"#!AMR\n", None),
            (b"<!doctype html>", None),
            (b"", None),
        ];
        for (head, expected) in cases {
            assert_eq!(sniff(head), *expected, "{head:?}");
        }
    }

    #[test]
    fn only_displayable_media_is_served_inline() {
        // Bytes win over the declared type, which only counts for media types.
        assert_eq!(serve_type("text/html", b"\xFF\xD8\xFF\xE0"), "image/jpeg");
        assert_eq!(serve_type("image/jpg", b"????"), "image/jpeg");
        assert_eq!(serve_type("video/mp4; codecs=avc1", b""), "video/mp4");
        for declared in ["text/html", "image/svg+xml", "application/pdf", ""] {
            assert_eq!(
                serve_type(declared, b"<svg onload=alert(1)>"),
                ATTACHMENT_TYPE
            );
        }
        assert!(is_inline("image/png"));
        assert!(!is_inline(ATTACHMENT_TYPE));
    }

    #[tokio::test]
    async fn cached_files_are_found_and_foreign_urls_refused() {
        let dir = temp_dir();
        let cache = MediaCache::new(dir.clone()).unwrap();
        let url = "https://photo-stal-1.zdn.vn/a.jpg";
        cache
            .insert(url, b"\x89PNG\r\n\x1a\nrest", "image/jpg")
            .await
            .unwrap();
        let hit = cache.get(url).await.unwrap();
        assert_eq!(hit.content_type, "image/png");
        assert!(hit.path.starts_with(&dir));
        assert!(matches!(
            cache.get("https://example.com/a.jpg").await,
            Err(FetchError::NotAllowed)
        ));
        // Local files come from the cache only.
        let local = format!("{LOCAL_URL_PREFIX}1/a.pdf");
        assert!(matches!(cache.get(&local).await, Err(FetchError::NotFound)));
        cache
            .insert(&local, b"%PDF-1.7", "application/pdf")
            .await
            .unwrap();
        assert_eq!(
            cache.get(&local).await.unwrap().content_type,
            ATTACHMENT_TYPE
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn trimming_drops_the_least_recently_used_files() {
        let dir = temp_dir();
        let cache = MediaCache::new(dir.clone()).unwrap();
        let mut paths = Vec::new();
        for (i, age_s) in [(0, 300), (1, 100), (2, 200)] {
            let url = format!("https://photo.zdn.vn/{i}.jpg");
            let file = cache.insert(&url, &[0u8; 100], "image/jpeg").await.unwrap();
            let when = SystemTime::now() - Duration::from_secs(age_s);
            std::fs::File::options()
                .write(true)
                .open(&file.path)
                .unwrap()
                .set_modified(when)
                .unwrap();
            paths.push(file.path);
        }
        assert_eq!(cache.trim(1000).unwrap(), (0, 0));
        assert_eq!(cache.trim(150).unwrap(), (2, 200));
        assert!(!paths[0].exists() && !paths[2].exists());
        assert!(paths[1].exists());
        assert!(!paths[0].with_extension("type").exists());
        assert_eq!(
            MediaCache::new(dir.join("none")).unwrap().trim(0).unwrap(),
            (0, 0)
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
