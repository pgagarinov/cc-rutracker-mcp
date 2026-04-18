//! rutracker-mirror — local mirror of a rutracker.org forum subset.
//!
//! Platform-neutral: takes a preloaded [`rutracker_http::Client`] via dependency injection.
//! Does **not** depend on `rutracker-cookies-macos`; the CLI wires cookies in.
//!
//! Storage layout (see `.omc/plans/mirror-sync.md` §4):
//!
//! ```text
//! $HOME/.rutracker/mirror/
//! ├── structure.json   — forum tree snapshot
//! ├── watchlist.json   — user-edited list of forums to sync
//! ├── state.db         — SQLite (WAL) sync index + per-forum meta
//! └── forums/<id>/topics/<topic_id>.json
//! ```
//!
//! The JSON-per-topic layer is the source of truth. `state.db` is a derived index
//! that can be rebuilt from the on-disk JSONs via `Mirror::rebuild_index` (M6).

pub mod config;
pub mod driver;
pub mod engine;
pub mod error;
pub mod lock;
pub mod migrate;
pub mod mirror;
pub mod state;
pub mod structure;
pub mod topic_io;
pub mod watchlist;

pub use driver::{DriverError, ForumSummary, SyncDriver, SyncSummary};
pub use error::{Error, Result};
pub use mirror::{default_root, Mirror, SCHEMA_VERSION};
