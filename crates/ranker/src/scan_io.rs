//! Read side of the `.scan.json` cache produced by the `/rank-scan-run` skill.
//!
//! This module is pure I/O + deserialisation — no network, no agent call. The
//! aggregator consumes it in later phases to turn per-topic analyses into a
//! film-level score.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Per-release tech-quality flags — five independent axes. Each is `true` when
/// the scanner saw at least one substantive praise or complaint about that
/// axis. `tech_praise` and `tech_complaints` share this shape (plan §3.1).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TechQuality {
    #[serde(default)]
    pub audio: bool,
    #[serde(default)]
    pub video: bool,
    #[serde(default)]
    pub subtitles: bool,
    #[serde(default)]
    pub dubbing: bool,
    #[serde(default)]
    pub sync: bool,
}

/// Scanner output for a single topic (Stage B.2 analysis payload).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TopicAnalysis {
    pub sentiment_score: f32,
    pub confidence: f32,
    #[serde(default)]
    pub themes_positive: Vec<String>,
    #[serde(default)]
    pub themes_negative: Vec<String>,
    #[serde(default)]
    pub tech_complaints: TechQuality,
    #[serde(default)]
    pub tech_praise: TechQuality,
    pub substantive_count: u32,
    #[serde(default)]
    pub red_flags: Vec<String>,
    pub relevance: f32,
}

/// On-disk envelope written by the `/rank-scan-run` skill at
/// `forums/<fid>/scans/<tid>.scan.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScanFile {
    pub schema: u32,
    pub agent_sha: String,
    pub scanned_at: String,
    pub topic_id: String,
    pub last_post_id: String,
    pub analysis: TopicAnalysis,
}

/// Errors surfaced when loading a `.scan.json` file.
#[derive(Debug, Error)]
pub enum ScanError {
    #[error("io error reading scan file {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("scan file {path:?} is not valid JSON: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

/// Load and parse a `.scan.json` file from disk.
pub fn read_scan(path: &Path) -> Result<ScanFile, ScanError> {
    let bytes = std::fs::read(path).map_err(|source| ScanError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_slice(&bytes).map_err(|source| ScanError::Json {
        path: path.to_path_buf(),
        source,
    })
}

/// Cache-hit check used by `scan_prepare`. True iff `scan_path` exists AND its
/// persisted `agent_sha` AND `last_post_id` match the expected values — any
/// mismatch (or a missing / unreadable file) → `false`, caller re-queues.
pub fn is_cached(scan_path: &Path, expected_agent_sha: &str, expected_last_post_id: &str) -> bool {
    if !scan_path.exists() {
        return false;
    }
    match read_scan(scan_path) {
        Ok(sf) => sf.agent_sha == expected_agent_sha && sf.last_post_id == expected_last_post_id,
        Err(_) => false,
    }
}

/// True when a sidecar `.scan.failed.json` exists for the given topic — these
/// are JSON-parse failures the skill recorded when the agent returned malformed
/// output. The aggregator excludes them from scored topics but counts them in
/// its diagnostic report (plan §3.6).
pub fn scan_is_failed(tid: &str, scans_dir: &Path) -> bool {
    scans_dir.join(format!("{tid}.scan.failed.json")).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_scan_json(agent_sha: &str, last_post_id: &str) -> String {
        format!(
            r#"{{
              "schema": 1,
              "agent_sha": "{agent_sha}",
              "scanned_at": "2026-04-18T20:30:44.785551+00:00",
              "topic_id": "6843582",
              "last_post_id": "{last_post_id}",
              "analysis": {{
                "sentiment_score": 7.5,
                "confidence": 0.8,
                "themes_positive": ["сильная игра", "хорошая операторская работа"],
                "themes_negative": ["затянутый финал"],
                "tech_complaints": {{"audio": false, "video": false, "subtitles": false, "dubbing": false, "sync": false}},
                "tech_praise":     {{"audio": true,  "video": true,  "subtitles": false, "dubbing": true,  "sync": true}},
                "substantive_count": 12,
                "red_flags": [],
                "relevance": 0.9
              }}
            }}"#
        )
    }

    #[test]
    fn test_reads_scan_json_schema_v1() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("6843582.scan.json");
        std::fs::write(&path, sample_scan_json("abc1234567890def", "1001")).unwrap();

        let sf = read_scan(&path).expect("scan JSON must parse");
        assert_eq!(sf.schema, 1);
        assert_eq!(sf.agent_sha, "abc1234567890def");
        assert_eq!(sf.topic_id, "6843582");
        assert_eq!(sf.last_post_id, "1001");
        assert!((sf.analysis.sentiment_score - 7.5).abs() < 1e-5);
        assert!((sf.analysis.confidence - 0.8).abs() < 1e-5);
        assert_eq!(sf.analysis.substantive_count, 12);
        assert!(sf.analysis.tech_praise.audio);
        assert!(!sf.analysis.tech_complaints.audio);
        assert_eq!(sf.analysis.themes_positive.len(), 2);
    }

    #[test]
    fn test_stale_scan_skipped_by_aggregator() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("6843582.scan.json");
        std::fs::write(&path, sample_scan_json("abc1234567890def", "1001")).unwrap();

        // Topic advanced to last_post_id = 1050; aggregator must refuse this cache.
        assert!(is_cached(&path, "abc1234567890def", "1001"));
        assert!(!is_cached(&path, "abc1234567890def", "1050"));
        // Agent file changed too — also invalid.
        assert!(!is_cached(&path, "deadbeefdeadbeef", "1001"));
    }

    #[test]
    fn test_failed_scan_excluded_from_aggregation() {
        let tmp = tempfile::TempDir::new().unwrap();
        let failed_path = tmp.path().join("6843582.scan.failed.json");
        std::fs::write(&failed_path, r#"{"error":"parse_failed","raw":"..."}"#).unwrap();

        assert!(scan_is_failed("6843582", tmp.path()));
        assert!(!scan_is_failed("9999999", tmp.path()));
    }
}
