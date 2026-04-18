//! rutracker-cookies-macos — Brave cookie extraction on macOS.
//!
//! Replaces the Python `pycookiecheat` path with a native Rust implementation:
//!
//! 1. Resolve Brave profile directory from `Local State` (JSON).
//! 2. Read `<profile>/Cookies` (SQLite) and select rutracker rows.
//! 3. Decrypt `v10`-prefixed values with PBKDF2-SHA1 + AES-128-CBC.
//! 4. Keychain password fetched via `security-framework`
//!    (service = "Brave Safe Storage", account = "Brave").

pub mod cache;
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

/// Load rutracker cookies. Order:
/// 1. If `$HOME/.rutracker/cookies.json` (or `RUTRACKER_COOKIE_CACHE`) exists and is within
///    TTL, return it — no Keychain prompt.
/// 2. Otherwise refresh from Brave profile + macOS Keychain, save the cache, return.
///
/// Cache path and TTL are controlled by `cache::default_cache_path()` and
/// `cache::DEFAULT_TTL` (7 days).
#[cfg(target_os = "macos")]
pub fn load_brave_cookies(profile_name: &str) -> Result<HashMap<String, String>> {
    let cache_path = cache::default_cache_path()?;
    if let Some(cached) = cache::load(&cache_path, cache::DEFAULT_TTL)? {
        if !cached.is_empty() {
            return Ok(cached);
        }
    }
    let fresh = refresh_brave_cookies(profile_name)?;
    // Best-effort save — cache write failure must not block the caller.
    if let Err(e) = cache::save(&cache_path, &fresh) {
        eprintln!(
            "warning: cookie cache write failed at {}: {e}",
            cache_path.display()
        );
    }
    Ok(fresh)
}

/// Force a fresh read from Brave + Keychain, ignoring any cached file.
#[cfg(target_os = "macos")]
pub fn refresh_brave_cookies(profile_name: &str) -> Result<HashMap<String, String>> {
    let profile_dir = profile::resolve_brave_profile_dir(profile_name)?;
    let cookies_db = profile_dir.join("Cookies");
    let password = keychain::fetch_brave_safe_storage_password()?;
    let rows = store::read_rutracker_rows(&cookies_db)?;

    // Chromium v10 plaintext layout on macOS (post-2020): SHA256(domain)[32 bytes] + value.
    // On older versions the hash prefix is absent. We try both offsets 32 and 0 per cookie
    // and keep whichever yields valid UTF-8.
    // Password candidates: Keychain value, then Chromium hardcoded fallback "peanuts".
    let candidates: [&[u8]; 2] = [password.as_bytes(), b"peanuts"];

    let mut out = HashMap::new();
    let mut any_decrypt_success = false;
    for row in rows {
        if row.encrypted_value.is_empty() {
            if !row.value.is_empty() {
                out.insert(row.name, row.value);
            }
            continue;
        }
        let mut final_str: Option<String> = None;
        'candidates: for &pw in &candidates {
            let Ok(bytes) = decrypt::decrypt(&row.encrypted_value, pw) else {
                continue;
            };
            for offset in [32usize, 0] {
                if bytes.len() < offset {
                    continue;
                }
                let slice = &bytes[offset..];
                if let Ok(s) = std::str::from_utf8(slice) {
                    final_str = Some(s.to_string());
                    any_decrypt_success = true;
                    break 'candidates;
                }
            }
        }
        if let Some(s) = final_str {
            if !s.is_empty() {
                out.insert(row.name, s);
            }
        }
    }
    if !any_decrypt_success {
        return Err(Error::Decrypt(
            "no candidate password produced valid UTF-8 plaintext for any cookie".to_string(),
        ));
    }
    Ok(out)
}

#[cfg(not(target_os = "macos"))]
pub fn load_brave_cookies(_profile_name: &str) -> Result<HashMap<String, String>> {
    Err(Error::PlatformUnsupported)
}

#[cfg(not(target_os = "macos"))]
pub fn refresh_brave_cookies(_profile_name: &str) -> Result<HashMap<String, String>> {
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
