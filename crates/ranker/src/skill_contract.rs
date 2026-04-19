//! Static-file contract checks for the committed Claude-side artefacts.
//!
//! The agent markdown at `.claude/agents/rutracker-film-scanner.md` and the
//! skill markdown at `.claude/skills/rank-scan-run.md` are the version-tracked
//! contract between Rust (`scan_prepare`, `scan_io`) and Claude Code (the
//! `/rank-scan-run` skill + subagent). These tests catch renames / deletions
//! / frontmatter drift — they do NOT invoke the agent.

use std::io;
use std::path::PathBuf;

/// Locate `.claude/skills/rank-scan-run.md` by walking up from
/// `CARGO_MANIFEST_DIR` via [`crate::agent_sha::locate_repo_file`].
pub fn locate_skill_file() -> io::Result<PathBuf> {
    crate::agent_sha::locate_repo_file(".claude/skills/rank-scan-run.md")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_sha::{agent_sha_current, locate_agent_file};

    #[test]
    fn test_scanner_agent_file_exists_and_has_frontmatter() {
        let path = locate_agent_file().expect("scanner agent md must exist");
        let content = std::fs::read_to_string(&path).expect("must be readable");
        assert!(
            content.starts_with("---"),
            "{path:?} must start with YAML frontmatter `---`"
        );
        assert!(
            content.contains("name: rutracker-film-scanner"),
            "frontmatter must declare name: rutracker-film-scanner"
        );
        assert!(
            content.contains("model: haiku"),
            "frontmatter must declare model: haiku"
        );
    }

    #[test]
    fn test_rank_scan_run_skill_references_agent() {
        let path = locate_skill_file().expect("rank-scan-run skill md must exist");
        let content = std::fs::read_to_string(&path).expect("must be readable");
        assert!(
            content.contains("rutracker-film-scanner"),
            "skill must reference the scanner subagent by name"
        );
        assert!(
            content.contains("scan-queue.jsonl"),
            "skill must reference the Rust-produced manifest file"
        );
    }

    #[test]
    fn test_agent_sha_stable_across_compilations() {
        // Two back-to-back reads must yield identical hex — protects against
        // a future change that accidentally introduces non-determinism
        // (e.g. tempfile path, timestamp, env var) in `agent_sha_current`.
        let a = agent_sha_current();
        let b = agent_sha_current();
        assert_eq!(a, b, "agent_sha must be deterministic");
        assert_eq!(a.len(), 16, "agent_sha must be 16 hex chars");
        assert!(
            a.chars().all(|c| c.is_ascii_hexdigit()),
            "agent_sha must be hex: {a}"
        );
    }
}
