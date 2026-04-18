use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("parser: {0}")]
    Parser(#[from] rutracker_parser::Error),

    #[error("http: {0}")]
    Http(#[from] rutracker_http::Error),

    #[error("rutracker-mirror v{binary} is older than your state.db (v{db}); upgrade the binary")]
    SchemaTooNew { binary: u32, db: u32 },

    #[error("mirror root not initialized at {0} — run `rutracker mirror init` first")]
    NotInitialized(String),

    #[error("unknown forum id: {0}; run `rutracker mirror structure` to refresh")]
    UnknownForum(String),

    #[error("mirror root is locked by another sync process (pid={holder_pid})")]
    Locked { holder_pid: u32 },
}

pub type Result<T> = std::result::Result<T, Error>;
