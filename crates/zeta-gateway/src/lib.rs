//! zetachat gateway: keeps the single Zalo session, stores messages in SQLite and
//! serves your own devices over an authenticated HTTP API with a live event stream.

pub mod api;
pub mod auth;
pub mod events;
pub mod model;
pub mod store;
pub mod zalo;

use std::path::{Path, PathBuf};

/// SQLite database inside the data directory.
pub fn db_path(data_dir: &Path) -> PathBuf {
    data_dir.join("zetachat.db")
}

/// Saved Zalo session inside the data directory. Contains credentials.
pub fn session_path(data_dir: &Path) -> PathBuf {
    data_dir.join("zalo-session.json")
}
