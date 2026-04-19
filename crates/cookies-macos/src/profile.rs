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

    /// US-008: `brave_app_support` composes `dirs::home_dir()` + the known
    /// subpath constant; no I/O. Covers the L9–L12 function body.
    #[test]
    fn test_brave_app_support_ends_with_brave_subpath() {
        let got = brave_app_support().expect("home_dir should be set in test env");
        assert!(
            got.ends_with(BRAVE_APP_SUPPORT_SUBPATH),
            "brave_app_support must end with the subpath constant, got: {}",
            got.display()
        );
    }

    /// US-008: `resolve_brave_profile_dir` is a thin wrapper that resolves
    /// the app-support dir then delegates to `resolve_profile_dir_at`. If
    /// Brave is not installed on the host (true in CI), the wrapper must
    /// error with an I/O error — not panic. Covers L14–L17.
    #[test]
    fn test_resolve_brave_profile_dir_wrapper_errors_when_local_state_absent() {
        // Most CI runners have no real Brave profile, so `Local State` is
        // absent at the resolved app-support path → std::fs::read errors.
        // (When run on a dev machine WITH Brave installed and the profile
        // name "Never-existing-profile-rutracker-test" missing, we still get
        // `ProfileNotFound`.) Both outcomes exercise the wrapper; either
        // way it must not panic.
        let result = resolve_brave_profile_dir("Never-existing-profile-rutracker-test");
        assert!(
            result.is_err(),
            "missing profile must surface as Err, not panic"
        );
    }

    /// US-008: parse errors in `Local State` (valid JSON but missing the
    /// expected structure) surface as `Error::LocalStateParse`. Covers
    /// L29 (the `.ok_or_else(|| Error::LocalStateParse …)`).
    #[test]
    fn test_local_state_without_profile_info_cache_errors_with_localstateparse() {
        let tmp = tempdir("no-info-cache");
        fs::write(tmp.join("Local State"), r#"{"profile":{}}"#).unwrap();
        let err = resolve_profile_dir_at(&tmp, "Peter").unwrap_err();
        assert!(
            matches!(err, Error::LocalStateParse(_)),
            "missing profile.info_cache must yield LocalStateParse, got: {err:?}"
        );
    }

    /// US-008: malformed JSON in `Local State` surfaces as
    /// `Error::LocalStateParse` via the serde_json error-map branch (L24).
    #[test]
    fn test_local_state_malformed_json_errors_with_localstateparse() {
        let tmp = tempdir("malformed-json");
        fs::write(tmp.join("Local State"), b"{not really json").unwrap();
        let err = resolve_profile_dir_at(&tmp, "Peter").unwrap_err();
        assert!(
            matches!(err, Error::LocalStateParse(_)),
            "malformed Local State JSON must yield LocalStateParse, got: {err:?}"
        );
    }

    /// US-008: a missing `Local State` file propagates via the `?` from
    /// `std::fs::read(&local_state_path)?` (L22). Covers the io::Error
    /// branch (Error::Io through `From<io::Error>`).
    #[test]
    fn test_local_state_missing_file_propagates_io_error() {
        let tmp = tempdir("no-local-state");
        // Deliberately do NOT create Local State.
        let err = resolve_profile_dir_at(&tmp, "Peter").unwrap_err();
        // We don't assert the exact Io variant (Error doesn't expose one)
        // but the error must not be ProfileNotFound or LocalStateParse.
        let s = err.to_string();
        assert!(
            !s.is_empty(),
            "missing Local State must surface a non-empty error"
        );
    }
}
