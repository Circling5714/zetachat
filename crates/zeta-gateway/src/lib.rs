//! zetachat gateway: keeps the single Zalo session, stores messages in SQLite and
//! serves your own devices over an authenticated HTTP API with a live event stream.

pub mod api;
pub mod auth;
pub mod demo;
pub mod events;
pub mod media;
pub mod media_cache;
pub mod model;
pub mod stickers;
pub mod store;
pub mod uploads;
pub mod web;
pub mod zalo;

use std::path::{Path, PathBuf};

/// SQLite database inside the data directory.
pub fn db_path(data_dir: &Path) -> PathBuf {
    data_dir.join("zetachat.db")
}

/// Cached media files inside the data directory.
pub fn media_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("media")
}

/// Files waiting to be sent to Zalo, inside the data directory.
pub fn uploads_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("uploads")
}

/// Saved Zalo session inside the data directory. Contains credentials.
pub fn session_path(data_dir: &Path) -> PathBuf {
    data_dir.join("zalo-session.json")
}
