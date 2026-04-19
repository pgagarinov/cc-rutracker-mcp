//! SHA-derived version tag for the scanner agent file.
//!
//! `agent_sha_current()` returns the first 16 hex chars of SHA-256 of
//! `.claude/agents/rutracker-film-scanner.md`. This value is embedded into each
//! `.scan.json` and into the `scan-queue.jsonl` manifest so that any edit to
//! the agent's prompt file auto-invalidates previously-cached scans (plan
//! §3.5 cache invariant).

use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Path (relative to the repo root) of the scanner agent file. Lives as a
/// single constant so tests and callers agree on the lookup.
pub const AGENT_REL_PATH: &str = ".claude/agents/rutracker-film-scanner.md";

/// Compute the first 16 hex chars of SHA-256 of the scanner agent file inside
/// the current repo. Resolves the repo root by walking up from
/// `CARGO_MANIFEST_DIR` until a `.claude/` directory is found.
///
/// Panics only if the environment is so broken the repo root cannot be found
/// or the file cannot be read — callers in production runs (via the CLI) will
/// already have set up a working tree. Returns a stable 16-char lowercase hex
/// string.
pub fn agent_sha_current() -> String {
    let path = locate_agent_file().expect("could not locate scanner agent file");
    agent_sha_of(&path).expect("could not read scanner agent file")
}

/// Compute the first 16 hex chars of SHA-256 of the file at `path`.
pub fn agent_sha_of(path: &Path) -> io::Result<String> {
    let bytes = std::fs::read(path)?;
    let digest = Sha256::digest(&bytes);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    Ok(hex[..16].to_string())
}

/// Walk up from `CARGO_MANIFEST_DIR` looking for a directory that contains
/// `rel` (a repo-relative path). Returns the absolute path of that file when
/// found. Shared by [`locate_agent_file`] and
/// [`crate::skill_contract::locate_skill_file`].
pub(crate) fn locate_repo_file(rel: &str) -> io::Result<PathBuf> {
    let start = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut cur: &Path = &start;
    loop {
        let candidate = cur.join(rel);
        if candidate.is_file() {
            return Ok(candidate);
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("could not find {rel} walking up from {start:?}"),
                ));
            }
        }
    }
}

/// Walk up from `CARGO_MANIFEST_DIR` looking for a directory that contains
/// the `.claude/agents/rutracker-film-scanner.md` file. Returns the absolute
/// path of that file when found.
pub fn locate_agent_file() -> io::Result<PathBuf> {
    locate_repo_file(AGENT_REL_PATH)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// US-008: `locate_repo_file` returns `io::ErrorKind::NotFound` when the
    /// relative path does not exist anywhere up the directory tree from
    /// `CARGO_MANIFEST_DIR`. The error message must name the missing path.
    /// Covers L54–L57.
    #[test]
    fn test_locate_repo_file_not_found_surfaces_io_not_found() {
        let rel = ".does-not-exist-rutracker-test/absolutely-nowhere.md";
        let err = locate_repo_file(rel).expect_err("missing file must error");
        assert_eq!(
            err.kind(),
            io::ErrorKind::NotFound,
            "error kind must be NotFound, got: {:?}",
            err.kind()
        );
        let msg = err.to_string();
        assert!(
            msg.contains(rel),
            "error message must name the missing relative path, got: {msg}"
        );
        assert!(
            msg.contains("walking up"),
            "error message must mention 'walking up', got: {msg}"
        );
    }

    /// US-008: `agent_sha_of` propagates filesystem errors when reading a
    /// non-existent path. Covers the `?` in `std::fs::read(path)?` (L33).
    #[test]
    fn test_agent_sha_of_missing_file_surfaces_io_error() {
        let path = std::path::PathBuf::from(
            "/does-not-exist-rutracker-test/nowhere/rutracker-missing-sha.md",
        );
        let err = agent_sha_of(&path).expect_err("nonexistent path must error");
        // Must be an I/O error (NotFound on unix, similar on other OSes).
        assert!(
            matches!(err.kind(), io::ErrorKind::NotFound),
            "expected NotFound on missing file, got: {:?}",
            err.kind()
        );
    }
}
