//! `Mirror` — root handle for the on-disk mirror.

use std::path::{Path, PathBuf};

use rutracker_http::Client;

use crate::config::Watchlist;
use crate::state::State;
use crate::{Error, Result};

/// Schema version supported by this binary. Bumped on any breaking change to
/// `state.db` or the on-disk JSON layout. Forward-only: older binaries refuse
/// newer DBs with [`Error::SchemaTooNew`].
pub const SCHEMA_VERSION: u32 = 1;

/// Resolve the default mirror root:
/// 1. `$RUTRACKER_MIRROR_ROOT` if set
/// 2. `$HOME/.rutracker/mirror`
/// 3. `.rutracker/mirror` (relative — fallback when neither is set)
pub fn default_root() -> PathBuf {
    if let Ok(v) = std::env::var("RUTRACKER_MIRROR_ROOT") {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home).join(".rutracker").join("mirror");
        }
    }
    PathBuf::from(".rutracker").join("mirror")
}

/// Root handle. Owns the SQLite state and (optionally) an HTTP client.
pub struct Mirror {
    root: PathBuf,
    client: Option<Client>,
    state: State,
}

impl Mirror {
    /// Initialise a fresh mirror at `root`. Creates directories, empty
    /// `structure.json` / `watchlist.json`, and applies migration `0001_init.sql`.
    pub fn init(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;
        std::fs::create_dir_all(root.join("forums"))?;

        let structure_path = root.join("structure.json");
        if !structure_path.exists() {
            let empty = crate::structure::Structure::empty();
            let bytes = serde_json::to_vec_pretty(&empty)?;
            crate::topic_io::atomic_write_bytes(&structure_path, &bytes)?;
        }

        let watchlist_path = root.join("watchlist.json");
        if !watchlist_path.exists() {
            let empty = Watchlist::default();
            let bytes = serde_json::to_vec_pretty(&empty)?;
            crate::topic_io::atomic_write_bytes(&watchlist_path, &bytes)?;
        }

        let state = State::init(root.join("state.db"))?;
        Ok(Self {
            root,
            client: None,
            state,
        })
    }

    /// Open an existing mirror. Refuses to open if the on-disk schema is newer
    /// than [`SCHEMA_VERSION`] (see [`Error::SchemaTooNew`]).
    pub fn open(root: impl AsRef<Path>, client: Option<Client>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let db_path = root.join("state.db");
        if !db_path.exists() {
            return Err(Error::NotInitialized(root.display().to_string()));
        }
        let state = State::open(&db_path)?;
        crate::migrate::check_schema_version(&state)?;
        Ok(Self {
            root,
            client,
            state,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut State {
        &mut self.state
    }

    pub fn client(&self) -> Option<&Client> {
        self.client.as_ref()
    }

    pub fn with_client(mut self, client: Client) -> Self {
        self.client = Some(client);
        self
    }

    /// Topics directory for a given forum id, `<root>/forums/<forum_id>/topics`.
    pub fn forum_topics_dir(&self, forum_id: &str) -> PathBuf {
        self.root.join("forums").join(forum_id).join("topics")
    }

    /// Scan `forums/<forum_id>/topics/*.json` and insert any rows missing from
    /// `topic_index`. Cheap recovery path after a crash between the JSON write
    /// and the SQLite commit (see plan §4.2).
    pub fn backfill_missing_index_rows(&mut self, forum_id: &str) -> Result<usize> {
        let topics_dir = self.forum_topics_dir(forum_id);
        self.state
            .backfill_missing_index_rows(forum_id, &topics_dir)
    }
}
