//! Plaintext cookie cache on disk, outside the Brave profile.
//!
//! Avoids re-prompting the macOS Keychain on every run. Stored at
//! `$HOME/.rutracker/cookies.json` (override with `RUTRACKER_COOKIE_CACHE=<path>`),
//! mode 0600 so other users on the box cannot read it.

use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

pub const DEFAULT_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Debug, Serialize, Deserialize)]
struct CacheFile {
    /// Milliseconds since UNIX epoch when this cache was written.
    saved_at_ms: u128,
    cookies: HashMap<String, String>,
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

pub fn default_cache_path() -> Result<PathBuf> {
    if let Ok(override_path) = std::env::var("RUTRACKER_COOKIE_CACHE") {
        return Ok(PathBuf::from(override_path));
    }
    let home = dirs::home_dir().ok_or_else(|| Error::ProfileNotFound("$HOME not set".into()))?;
    Ok(home.join(".rutracker").join("cookies.json"))
}

/// Read the cache. Returns:
/// - `Ok(Some(map))` if cache exists and is fresher than `ttl`.
/// - `Ok(None)` if cache is missing or stale — caller should refresh from Keychain.
/// - `Err(_)` on filesystem / JSON errors.
pub fn load(path: &std::path::Path, ttl: Duration) -> Result<Option<HashMap<String, String>>> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(path)?;
    let cache: CacheFile =
        serde_json::from_slice(&bytes).map_err(|e| Error::LocalStateParse(e.to_string()))?;
    let age = now_ms().saturating_sub(cache.saved_at_ms);
    if Duration::from_millis(age as u64) > ttl {
        return Ok(None);
    }
    Ok(Some(cache.cookies))
}

/// Write `cookies` to `path` with mode `0600`. Creates parent dirs as needed.
pub fn save(path: &std::path::Path, cookies: &HashMap<String, String>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let cache = CacheFile {
        saved_at_ms: now_ms(),
        cookies: cookies.clone(),
    };
    let bytes =
        serde_json::to_vec_pretty(&cache).map_err(|e| Error::LocalStateParse(e.to_string()))?;
    std::fs::write(path, bytes)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub fn invalidate(path: &std::path::Path) -> Result<()> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    fn tempdir(suffix: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "rutracker-cookie-cache-{}-{}",
            std::process::id(),
            suffix
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn test_round_trip() {
        let dir = tempdir("roundtrip");
        let path = dir.join("cookies.json");
        let mut c = HashMap::new();
        c.insert("bb_session".to_string(), "abc".to_string());
        c.insert("bb_guid".to_string(), "def".to_string());
        save(&path, &c).unwrap();
        let loaded = load(&path, DEFAULT_TTL).unwrap().unwrap();
        assert_eq!(loaded, c);
    }

    #[test]
    fn test_missing_returns_none() {
        let dir = tempdir("missing");
        let path = dir.join("absent.json");
        let loaded = load(&path, DEFAULT_TTL).unwrap();
        assert!(loaded.is_none());
    }

    #[test]
    fn test_stale_returns_none() {
        let dir = tempdir("stale");
        let path = dir.join("cookies.json");
        let mut c = HashMap::new();
        c.insert("x".into(), "y".into());
        save(&path, &c).unwrap();
        // Ask for TTL shorter than sleep; guarantee stale.
        sleep(Duration::from_millis(50));
        let loaded = load(&path, Duration::from_millis(10)).unwrap();
        assert!(loaded.is_none(), "stale cache should return None");
    }

    #[cfg(unix)]
    #[test]
    fn test_save_sets_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir("perms");
        let path = dir.join("cookies.json");
        save(&path, &HashMap::new()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "cookie cache file should be mode 0600");
    }

    #[test]
    fn test_invalidate_removes_file() {
        let dir = tempdir("invalidate");
        let path = dir.join("cookies.json");
        save(&path, &HashMap::new()).unwrap();
        assert!(path.exists());
        invalidate(&path).unwrap();
        assert!(!path.exists());
    }
}
