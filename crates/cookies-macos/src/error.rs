use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("profile not found: {0}")]
    ProfileNotFound(String),

    #[error("Local State JSON parse failed: {0}")]
    LocalStateParse(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[cfg(target_os = "macos")]
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[cfg(target_os = "macos")]
    #[error("keychain: {0}")]
    Keychain(String),

    #[error("cookie decrypt failed: {0}")]
    Decrypt(String),

    #[error("unsupported cookie version prefix: expected 'v10', got {0:?}")]
    UnsupportedVersion(String),

    #[error("bb_dl_key cookie missing — dl.php requires it; run refresh_cookies() and ensure an active rutracker session")]
    MissingDlKey,

    #[error("not supported on this platform")]
    PlatformUnsupported,
}

pub type Result<T> = std::result::Result<T, Error>;
