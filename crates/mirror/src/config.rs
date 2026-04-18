//! On-disk config files: `watchlist.json`.
//!
//! `structure.json` lives in [`crate::structure`] — it is a parsed snapshot
//! of the forum tree, not a user-edited config.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Watchlist {
    pub schema_version: u32,
    #[serde(default)]
    pub forums: Vec<WatchlistEntry>,
}

impl Default for Watchlist {
    fn default() -> Self {
        Self {
            schema_version: 1,
            forums: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WatchlistEntry {
    pub forum_id: String,
    pub name: String,
    pub added_at: String,
}
