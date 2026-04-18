//! Brave profile directory resolution via `Local State` JSON.

use crate::{Error, Result};
use std::path::{Path, PathBuf};

pub const BRAVE_APP_SUPPORT_SUBPATH: &str =
    "Library/Application Support/BraveSoftware/Brave-Browser";

pub fn brave_app_support() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| Error::ProfileNotFound("$HOME not set".into()))?;
    Ok(home.join(BRAVE_APP_SUPPORT_SUBPATH))
}

pub fn resolve_brave_profile_dir(display_name: &str) -> Result<PathBuf> {
    let app_support = brave_app_support()?;
    resolve_profile_dir_at(&app_support, display_name)
}

/// Testable core: given a Brave app-support directory, resolve the profile subdir by name.
pub fn resolve_profile_dir_at(app_support: &Path, display_name: &str) -> Result<PathBuf> {
    let local_state_path = app_support.join("Local State");
    let bytes = std::fs::read(&local_state_path)?;
    let json: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| Error::LocalStateParse(e.to_string()))?;
    let profiles = json
        .get("profile")
        .and_then(|p| p.get("info_cache"))
        .and_then(|c| c.as_object())
        .ok_or_else(|| Error::LocalStateParse("missing profile.info_cache".into()))?;
    for (dir_name, info) in profiles {
        if info.get("name").and_then(|n| n.as_str()) == Some(display_name) {
            return Ok(app_support.join(dir_name));
        }
    }
    let available: Vec<String> = profiles
        .values()
        .filter_map(|v| v.get("name").and_then(|n| n.as_str()).map(String::from))
        .collect();
    Err(Error::ProfileNotFound(format!(
        "display name {display_name:?} not found. Available: {available:?}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_resolves_profile_by_display_name() {
        let tmp = tempdir("resolve-by-name");
        fs::write(
            tmp.join("Local State"),
            r#"{"profile":{"info_cache":{"Profile 2":{"name":"Peter"},"Profile 1":{"name":"Default"}}}}"#,
        )
        .unwrap();
        let got = resolve_profile_dir_at(&tmp, "Peter").unwrap();
        assert_eq!(got, tmp.join("Profile 2"));
    }

    #[test]
    fn test_unknown_profile_name_errors_with_available_list() {
        let tmp = tempdir("unknown-name");
        fs::write(
            tmp.join("Local State"),
            r#"{"profile":{"info_cache":{"Profile 1":{"name":"Alice"}}}}"#,
        )
        .unwrap();
        let err = resolve_profile_dir_at(&tmp, "Bob").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Bob"),
            "error names the missing profile, got: {msg}"
        );
        assert!(
            msg.contains("Alice"),
            "error lists available profiles, got: {msg}"
        );
    }

    fn tempdir(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rutracker-cookies-test-{}-{}",
            std::process::id(),
            suffix
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
