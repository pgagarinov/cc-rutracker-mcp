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

    /// US-008: `invalidate` on a non-existent path is a no-op and must NOT
    /// error — the `path.exists()` guard short-circuits the remove.
    #[test]
    fn test_invalidate_absent_path_is_noop() {
        let dir = tempdir("invalidate-absent");
        let absent = dir.join("never-created.json");
        assert!(!absent.exists());
        invalidate(&absent).expect("invalidate of absent file must succeed");
        assert!(!absent.exists(), "path must still be absent after no-op");
    }

    // Serializes the two tests that mutate RUTRACKER_COOKIE_CACHE. `set_var` /
    // `remove_var` are process-global, so without this lock the two tests race
    // when cargo-llvm-cov runs the test binary with its default thread count.
    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// US-008: `default_cache_path` returns the `RUTRACKER_COOKIE_CACHE`
    /// env var verbatim when set.
    #[test]
    fn test_default_cache_path_honours_override_env() {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let original = std::env::var("RUTRACKER_COOKIE_CACHE").ok();
        let marker = format!("/tmp/rutracker-cache-override-{}", std::process::id());
        // SAFETY: altering env is safe — ENV_MUTEX serializes the mutation.
        unsafe { std::env::set_var("RUTRACKER_COOKIE_CACHE", &marker) };
        let got = default_cache_path().unwrap();
        // Restore before any assert, to prevent pollution if the assert panics.
        unsafe {
            match original {
                Some(v) => std::env::set_var("RUTRACKER_COOKIE_CACHE", v),
                None => std::env::remove_var("RUTRACKER_COOKIE_CACHE"),
            }
        }
        assert_eq!(
            got.to_string_lossy(),
            marker,
            "override env var must be returned verbatim"
        );
    }

    /// US-008: `default_cache_path` without the env var falls back to
    /// `$HOME/.rutracker/cookies.json`.
    #[test]
    fn test_default_cache_path_default_location_under_home() {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let original = std::env::var("RUTRACKER_COOKIE_CACHE").ok();
        // SAFETY: we're explicitly clearing a single env var we own.
        unsafe { std::env::remove_var("RUTRACKER_COOKIE_CACHE") };
        let got = default_cache_path().unwrap();
        unsafe {
            if let Some(v) = original {
                std::env::set_var("RUTRACKER_COOKIE_CACHE", v);
            }
        }
        assert!(
            got.ends_with(".rutracker/cookies.json"),
            "default must land at $HOME/.rutracker/cookies.json, got: {}",
            got.display()
        );
    }

    /// US-008: corrupt JSON in the cache file surfaces as `Error::LocalStateParse`,
    /// covering the `serde_json::from_slice(&bytes).map_err(…)` branch at L47.
    #[test]
    fn test_load_corrupt_json_returns_localstateparse_error() {
        let dir = tempdir("corrupt");
        let path = dir.join("cookies.json");
        std::fs::write(&path, b"{ not valid json at all ").unwrap();
        let err = load(&path, DEFAULT_TTL).expect_err("corrupt cache must error");
        assert!(
            matches!(err, Error::LocalStateParse(_)),
            "expected LocalStateParse, got: {err:?}"
        );
    }
}
