//! Atomic JSON write helpers: temp-file + `sync_all` + rename + parent-dir fsync.
//!
//! APFS / ext4 guarantee rename atomicity within the same directory. Parent-dir
//! fsync adds true power-loss durability. NFS/SMB are out of scope (see plan §15).

use std::fs::File;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::Result;

/// One post inside a topic file — opening post or comment.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Post {
    pub post_id: u64,
    pub author: String,
    pub date: String,
    pub text: String,
}

impl From<rutracker_parser::Comment> for Post {
    fn from(c: rutracker_parser::Comment) -> Self {
        Self {
            post_id: c.post_id,
            author: c.author,
            date: c.date,
            text: c.text,
        }
    }
}

/// On-disk topic archive. The source of truth — SQLite `topic_index` is a
/// derived cache rebuildable from these files (plan §4.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopicFile {
    pub schema_version: u32,
    pub topic_id: String,
    pub forum_id: String,
    pub title: String,
    pub fetched_at: String,
    pub last_post_id: u64,
    pub last_post_at: String,
    pub opening_post: Post,
    pub comments: Vec<Post>,
    pub metadata: serde_json::Value,
}

/// Serialize `value` as pretty JSON and write it atomically to `path`.
/// Creates the parent directory if missing.
pub fn write_json_atomic<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(value)?;
    atomic_write_bytes(path, &bytes)
}

/// Write `data` atomically to `path`: `<path>.tmp` → `sync_all` → rename → parent-dir fsync.
pub fn atomic_write_bytes(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = tmp_path(path);
    {
        let mut f = File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    if let Some(parent) = path.parent() {
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

fn tmp_path(path: &Path) -> std::path::PathBuf {
    let mut name = path
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}
