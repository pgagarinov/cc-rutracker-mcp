//! rutracker-cookies-macos — Brave cookie extraction on macOS.
//!
//! Replaces the Python `pycookiecheat` path with a native Rust implementation:
//!
//! 1. Resolve Brave profile directory from `Local State` (JSON).
//! 2. Read `<profile>/Cookies` (SQLite) and select rutracker rows.
//! 3. Decrypt `v10`-prefixed values with PBKDF2-SHA1 + AES-128-CBC.
//! 4. Keychain password fetched via `security-framework`
//!    (service = "Brave Safe Storage", account = "Brave").

pub mod decrypt;
pub mod error;

pub use error::{Error, Result};

#[cfg(target_os = "macos")]
pub mod keychain;

#[cfg(target_os = "macos")]
pub mod profile;

#[cfg(target_os = "macos")]
pub mod store;

use std::collections::HashMap;

/// Load rutracker cookies from Brave on macOS. Returns plaintext name→value pairs.
///
/// Phase 3B public entry point. The `#[cfg(target_os = "macos")]` wrapping keeps the
/// non-macOS targets compilable (they get a stub that returns [`Error::PlatformUnsupported`]).
#[cfg(target_os = "macos")]
pub fn load_brave_cookies(profile_name: &str) -> Result<HashMap<String, String>> {
    let profile_dir = profile::resolve_brave_profile_dir(profile_name)?;
    let cookies_db = profile_dir.join("Cookies");
    let password = keychain::fetch_brave_safe_storage_password()?;
    let rows = store::read_rutracker_rows(&cookies_db)?;

    let mut out = HashMap::new();
    for row in rows {
        let plaintext = if row.encrypted_value.is_empty() {
            row.value
        } else {
            let decrypted = decrypt::decrypt(&row.encrypted_value, password.as_bytes())?;
            String::from_utf8_lossy(&decrypted).into_owned()
        };
        if !plaintext.is_empty() {
            out.insert(row.name, plaintext);
        }
    }
    Ok(out)
}

#[cfg(not(target_os = "macos"))]
pub fn load_brave_cookies(_profile_name: &str) -> Result<HashMap<String, String>> {
    Err(Error::PlatformUnsupported)
}

/// Raise a clear error if the `bb_dl_key` cookie needed by `dl.php` is missing.
pub fn assert_dl_key(cookies: &HashMap<String, String>) -> Result<()> {
    if !cookies.contains_key("bb_dl_key") {
        return Err(Error::MissingDlKey);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_assert_dl_key_missing_returns_error() {
        let empty: HashMap<String, String> = HashMap::new();
        let err = assert_dl_key(&empty).unwrap_err();
        assert!(matches!(err, Error::MissingDlKey));
    }

    #[test]
    fn test_assert_dl_key_present_returns_ok() {
        let mut jar = HashMap::new();
        jar.insert("bb_dl_key".to_string(), "token".to_string());
        assert!(assert_dl_key(&jar).is_ok());
    }
}
