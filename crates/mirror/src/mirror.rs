//! `Mirror` — root handle for the on-disk mirror.

use std::path::{Path, PathBuf};

use rutracker_http::Client;

use crate::config::Watchlist;
use crate::state::State;
use crate::{Error, Result};

/// Schema version supported by this binary. Bumped on any breaking change to
/// `state.db` or the on-disk JSON layout. Forward-only: older binaries refuse
/// newer DBs with [`Error::SchemaTooNew`].
pub const SCHEMA_VERSION: u32 = 2;

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

        let mut state = State::init(root.join("state.db"))?;
        crate::migrate::ensure_schema(state.conn_mut())?;
        Ok(Self {
            root,
            client: None,
            state,
        })
    }

    /// Open an existing mirror. Refuses to open if the on-disk schema is newer
    /// than [`SCHEMA_VERSION`] (see [`Error::SchemaTooNew`]); applies pending
    /// forward-migrations when the on-disk schema is older.
    pub fn open(root: impl AsRef<Path>, client: Option<Client>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let db_path = root.join("state.db");
        if !db_path.exists() {
            return Err(Error::NotInitialized(root.display().to_string()));
        }
        let mut state = State::open(&db_path)?;
        crate::migrate::ensure_schema(state.conn_mut())?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_root_returns_path() {
        // Just exercises the function so the `if let Ok(...)` branches are reached
        // deterministically without racing on process-wide env. We assert the
        // returned path is non-empty and contains the expected `.rutracker/mirror`
        // tail whenever HOME is set (which is true in every sane CI and dev env).
        let got = default_root();
        assert!(!got.as_os_str().is_empty());
        if std::env::var("RUTRACKER_MIRROR_ROOT").is_err() && std::env::var("HOME").is_ok() {
            assert!(
                got.ends_with(".rutracker/mirror"),
                "default_root should end in .rutracker/mirror, got {}",
                got.display()
            );
        }
    }

    #[test]
    fn test_open_on_uninitialized_errors() {
        let dir =
            std::env::temp_dir().join(format!("rutracker-open-uninit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let err = match Mirror::open(&dir, None) {
            Ok(_) => panic!("open on uninitialized dir must error"),
            Err(e) => e,
        };
        assert!(matches!(err, crate::Error::NotInitialized(_)));
    }

    #[test]
    fn test_with_client_attaches_client() {
        let dir =
            std::env::temp_dir().join(format!("rutracker-with-client-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let m = Mirror::init(&dir).unwrap();
        assert!(m.client().is_none());
        let http = rutracker_http::Client::new("https://example.test/forum/").unwrap();
        let m2 = m.with_client(http);
        assert!(m2.client().is_some());
    }

    #[test]
    fn test_forum_topics_dir_is_correct() {
        let dir = std::env::temp_dir().join(format!("rutracker-topics-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let m = Mirror::init(&dir).unwrap();
        let got = m.forum_topics_dir("252");
        assert_eq!(got, dir.join("forums").join("252").join("topics"));
    }
}
