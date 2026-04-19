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
