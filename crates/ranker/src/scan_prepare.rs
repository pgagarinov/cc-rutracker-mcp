//! Stage B.1 — generate the `scan-queue.jsonl` manifest consumed by the
//! `/rank-scan-run` skill.
//!
//! All mutable-state decisions (cache hit/miss, payload truncation, queue
//! ordering) live here so they are unit-testable without a live agent. The
//! skill downstream is a thin consumer (plan §3.2 / §3.3).

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::scan_io::is_cached;

/// Tunable knobs. `max_payload_bytes` bounds the size of the JSON payload the
/// agent receives per topic; defaults to 8 KiB per plan §3.2.
#[derive(Debug, Clone)]
pub struct ScanPrepareOpts {
    pub max_payload_bytes: usize,
}

impl Default for ScanPrepareOpts {
    fn default() -> Self {
        Self {
            max_payload_bytes: 8192,
        }
    }
}

/// Summary stats returned to the CLI for user-visible reporting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PrepareReport {
    pub queued: usize,
    pub skipped_cached: usize,
    pub total: usize,
}

/// All the ways `scan_prepare` can fail during a run.
#[derive(Debug, Error)]
pub enum ScanPrepareError {
    #[error("io error at {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("json error at {path:?}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

/// Minimal projection of `TopicFile` — only the fields `scan_prepare` needs.
/// Keeps this module decoupled from `rutracker-mirror`'s full type and
/// forward-compatible with new fields there.
#[derive(Debug, Deserialize)]
struct TopicView {
    #[serde(default)]
    title: String,
    #[serde(default)]
    last_post_id: u64,
    #[serde(default)]
    opening_post: PostView,
    #[serde(default)]
    comments: Vec<PostView>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct PostView {
    #[serde(default)]
    author: String,
    #[serde(default)]
    date: String,
    #[serde(default)]
    text: String,
}

/// One manifest line. Public `Serialize` so tests can round-trip it.
#[derive(Debug, Serialize, Deserialize)]
pub struct QueueLine {
    pub topic_id: String,
    pub forum_id: String,
    pub last_post_id: String,
    pub agent_sha: String,
    pub scan_path: String,
    pub payload: Payload,
    pub included_comments: usize,
    pub total_comments: usize,
}

/// Compact payload fed to the agent. Required: full title + opening_post.text;
/// comments are newest-first and may be truncated to fit `max_payload_bytes`.
#[derive(Debug, Serialize, Deserialize)]
pub struct Payload {
    pub title: String,
    pub opening_post: OpeningPost,
    pub comments: Vec<PayloadComment>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OpeningPost {
    pub text: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PayloadComment {
    pub author: String,
    pub date: String,
    pub text: String,
}

/// Walk `<mirror_root>/forums/<forum_id>/topics/*.json`, skip cached topics,
/// and emit one manifest line per queued topic into
/// `<mirror_root>/forums/<forum_id>/scan-queue.jsonl` (atomic `.tmp → rename`).
/// Also ensures `<mirror_root>/forums/<forum_id>/scans/` exists so the skill
/// can drop `.scan.json` files there later.
pub fn scan_prepare(
    mirror_root: &Path,
    forum_id: &str,
    agent_sha: &str,
    opts: ScanPrepareOpts,
) -> Result<PrepareReport, ScanPrepareError> {
    let forum_dir = mirror_root.join("forums").join(forum_id);
    let topics_dir = forum_dir.join("topics");
    let scans_dir = forum_dir.join("scans");

    fs::create_dir_all(&scans_dir).map_err(|source| ScanPrepareError::Io {
        path: scans_dir.clone(),
        source,
    })?;
    fs::create_dir_all(&forum_dir).map_err(|source| ScanPrepareError::Io {
        path: forum_dir.clone(),
        source,
    })?;

    let queue_path = forum_dir.join("scan-queue.jsonl");
    let tmp_path = forum_dir.join("scan-queue.jsonl.tmp");

    // Gather topic JSON paths deterministically (sorted by filename).
    let mut topic_paths: Vec<PathBuf> = Vec::new();
    if topics_dir.is_dir() {
        let read = fs::read_dir(&topics_dir).map_err(|source| ScanPrepareError::Io {
            path: topics_dir.clone(),
            source,
        })?;
        for entry in read {
            let entry = entry.map_err(|source| ScanPrepareError::Io {
                path: topics_dir.clone(),
                source,
            })?;
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) == Some("json") {
                topic_paths.push(p);
            }
        }
    }
    topic_paths.sort();

    let mut report = PrepareReport::default();

    // Write into the temp file directly — stream instead of buffering in RAM
    // so we stay flat on big forums.
    let mut out = File::create(&tmp_path).map_err(|source| ScanPrepareError::Io {
        path: tmp_path.clone(),
        source,
    })?;

    for path in &topic_paths {
        report.total += 1;
        let tid = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };

        let bytes = fs::read(path).map_err(|source| ScanPrepareError::Io {
            path: path.clone(),
            source,
        })?;
        let view: TopicView =
            serde_json::from_slice(&bytes).map_err(|source| ScanPrepareError::Json {
                path: path.clone(),
                source,
            })?;

        let last_post_id = view.last_post_id.to_string();
        let scan_path = scans_dir.join(format!("{tid}.scan.json"));

        if is_cached(&scan_path, agent_sha, &last_post_id) {
            report.skipped_cached += 1;
            continue;
        }

        let total_comments = view.comments.len();
        let payload = build_payload(
            &view.title,
            view.opening_post.text.clone(),
            &view.comments,
            opts.max_payload_bytes,
        );
        let included_comments = payload.comments.len();

        let rel_scan_path = format!("forums/{forum_id}/scans/{tid}.scan.json");
        let line = QueueLine {
            topic_id: tid.clone(),
            forum_id: forum_id.to_string(),
            last_post_id,
            agent_sha: agent_sha.to_string(),
            scan_path: rel_scan_path,
            payload,
            included_comments,
            total_comments,
        };

        let encoded = serde_json::to_string(&line).map_err(|source| ScanPrepareError::Json {
            path: path.clone(),
            source,
        })?;
        out.write_all(encoded.as_bytes())
            .and_then(|_| out.write_all(b"\n"))
            .map_err(|source| ScanPrepareError::Io {
                path: tmp_path.clone(),
                source,
            })?;

        report.queued += 1;
    }

    out.sync_all().map_err(|source| ScanPrepareError::Io {
        path: tmp_path.clone(),
        source,
    })?;
    drop(out);

    fs::rename(&tmp_path, &queue_path).map_err(|source| ScanPrepareError::Io {
        path: queue_path.clone(),
        source,
    })?;
    if let Ok(dir) = File::open(&forum_dir) {
        let _ = dir.sync_all();
    }

    Ok(report)
}

/// Build the compact payload: always keep title + opening_post.text in full,
/// then add comments newest-first while the serialised JSON length stays ≤
/// `budget`. Returns a `Payload` with the preserved comment subset (newest
/// first to match the reversal semantics in plan §3.2).
fn build_payload(
    title: &str,
    opening_text: String,
    comments: &[PostView],
    budget: usize,
) -> Payload {
    let mut out = Payload {
        title: title.to_string(),
        opening_post: OpeningPost { text: opening_text },
        comments: Vec::new(),
    };

    // Newest-first iteration (plan §3.2). `comments` in the topic JSON is
    // chronological (oldest → newest), so reverse.
    for c in comments.iter().rev() {
        let candidate = PayloadComment {
            author: c.author.clone(),
            date: c.date.clone(),
            text: c.text.clone(),
        };
        out.comments.push(candidate);
        let encoded_len = serde_json::to_vec(&out).map(|v| v.len()).unwrap_or(0);
        if encoded_len > budget {
            // Roll back the last push and stop.
            out.comments.pop();
            break;
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENT_SHA: &str = "abcdef0123456789";

    fn write_topic(
        dir: &Path,
        tid: &str,
        last_post_id: u64,
        n_comments: usize,
        comment_text: &str,
    ) {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(format!("{tid}.json"));
        let comments: Vec<serde_json::Value> = (0..n_comments)
            .map(|i| {
                serde_json::json!({
                    "post_id": 100 + i,
                    "author": format!("user{i}"),
                    "date": "2026-04-18",
                    "text": comment_text,
                })
            })
            .collect();
        let v = serde_json::json!({
            "schema_version": 1,
            "topic_id": tid,
            "forum_id": "252",
            "title": format!("Title {tid} / Foo (Dir) [2026, США, драма, WEB-DLRip] Dub"),
            "fetched_at": "2026-04-18T20:30:44.785551+00:00",
            "last_post_id": last_post_id,
            "last_post_at": "2026-04-18T20:00:00+00:00",
            "opening_post": {
                "post_id": 0,
                "author": "",
                "date": "",
                "text": "opening post description for this film"
            },
            "comments": comments,
            "metadata": null
        });
        fs::write(&path, serde_json::to_vec_pretty(&v).unwrap()).unwrap();
    }

    fn write_scan(dir: &Path, tid: &str, agent_sha: &str, last_post_id: u64) {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(format!("{tid}.scan.json"));
        let v = serde_json::json!({
            "schema": 1,
            "agent_sha": agent_sha,
            "scanned_at": "2026-04-18T20:00:00+00:00",
            "topic_id": tid,
            "last_post_id": last_post_id.to_string(),
            "analysis": {
                "sentiment_score": 6.0,
                "confidence": 0.5,
                "themes_positive": [],
                "themes_negative": [],
                "tech_complaints": {"audio": false, "video": false, "subtitles": false, "dubbing": false, "sync": false},
                "tech_praise":     {"audio": false, "video": false, "subtitles": false, "dubbing": false, "sync": false},
                "substantive_count": 3,
                "red_flags": [],
                "relevance": 0.8
            }
        });
        fs::write(&path, serde_json::to_vec_pretty(&v).unwrap()).unwrap();
    }

    fn read_queue_lines(forum_dir: &Path) -> Vec<QueueLine> {
        let s = fs::read_to_string(forum_dir.join("scan-queue.jsonl")).unwrap();
        s.lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn test_skips_cached_topics() {
        let _td = tempfile::TempDir::new().unwrap();
        let root = _td.path();
        let forum_dir = root.join("forums").join("252");
        let topics_dir = forum_dir.join("topics");
        let scans_dir = forum_dir.join("scans");

        // 5 topics total.
        for (i, tid) in ["1001", "1002", "1003", "1004", "1005"].iter().enumerate() {
            write_topic(&topics_dir, tid, 2000 + i as u64, 2, "short");
        }
        // 3 of them have matching cache files.
        write_scan(&scans_dir, "1001", AGENT_SHA, 2000);
        write_scan(&scans_dir, "1002", AGENT_SHA, 2001);
        write_scan(&scans_dir, "1003", AGENT_SHA, 2002);

        let report = scan_prepare(root, "252", AGENT_SHA, ScanPrepareOpts::default()).unwrap();
        assert_eq!(report.total, 5);
        assert_eq!(report.skipped_cached, 3);
        assert_eq!(report.queued, 2);

        let lines = read_queue_lines(&forum_dir);
        assert_eq!(lines.len(), 2);
        let tids: Vec<_> = lines.iter().map(|l| l.topic_id.clone()).collect();
        assert!(tids.contains(&"1004".to_string()));
        assert!(tids.contains(&"1005".to_string()));
    }

    #[test]
    fn test_truncation_preserves_title_and_opening_post() {
        let _td = tempfile::TempDir::new().unwrap();
        let root = _td.path();
        let forum_dir = root.join("forums").join("252");
        let topics_dir = forum_dir.join("topics");

        // Long comment, many of them, forces truncation.
        let long_comment = "комментарий ".repeat(20); // ~240 chars each
        write_topic(&topics_dir, "1001", 2000, 500, &long_comment);

        let report = scan_prepare(
            root,
            "252",
            AGENT_SHA,
            ScanPrepareOpts {
                max_payload_bytes: 4096,
            },
        )
        .unwrap();
        assert_eq!(report.queued, 1);

        let lines = read_queue_lines(&forum_dir);
        let line = &lines[0];
        assert!(line.payload.title.contains("Title 1001"));
        assert_eq!(
            line.payload.opening_post.text,
            "opening post description for this film"
        );
        assert_eq!(line.total_comments, 500);
        assert!(line.included_comments < line.total_comments);
        assert!(line.included_comments > 0, "should include at least some");
    }

    #[test]
    fn test_agent_sha_cache_invalidation() {
        let _td = tempfile::TempDir::new().unwrap();
        let root = _td.path();
        let forum_dir = root.join("forums").join("252");
        let topics_dir = forum_dir.join("topics");
        let scans_dir = forum_dir.join("scans");

        write_topic(&topics_dir, "1001", 2000, 2, "short");
        // Cache exists but with a stale agent_sha.
        write_scan(&scans_dir, "1001", "stalesha00000000", 2000);

        let report = scan_prepare(root, "252", AGENT_SHA, ScanPrepareOpts::default()).unwrap();
        assert_eq!(report.queued, 1);
        assert_eq!(report.skipped_cached, 0);
    }

    #[test]
    fn test_last_post_id_cache_invalidation() {
        let _td = tempfile::TempDir::new().unwrap();
        let root = _td.path();
        let forum_dir = root.join("forums").join("252");
        let topics_dir = forum_dir.join("topics");
        let scans_dir = forum_dir.join("scans");

        write_topic(&topics_dir, "1001", 2050, 2, "short");
        // Cache has old last_post_id — topic advanced.
        write_scan(&scans_dir, "1001", AGENT_SHA, 2000);

        let report = scan_prepare(root, "252", AGENT_SHA, ScanPrepareOpts::default()).unwrap();
        assert_eq!(report.queued, 1);
        assert_eq!(report.skipped_cached, 0);
    }

    #[test]
    fn test_no_topics_dir_produces_empty_queue() {
        // Forum dir with no topics/ subdirectory still produces an empty
        // scan-queue.jsonl (via the is_dir() false branch).
        let _td = tempfile::TempDir::new().unwrap();
        let root = _td.path();
        let forum_dir = root.join("forums").join("252");
        fs::create_dir_all(&forum_dir).unwrap();
        let report = scan_prepare(root, "252", AGENT_SHA, ScanPrepareOpts::default()).unwrap();
        assert_eq!(report.total, 0);
        assert_eq!(report.queued, 0);
        assert_eq!(report.skipped_cached, 0);
        assert!(forum_dir.join("scan-queue.jsonl").exists());
    }

    #[test]
    fn test_invalid_topic_json_returns_error() {
        let _td = tempfile::TempDir::new().unwrap();
        let root = _td.path();
        let forum_dir = root.join("forums").join("252");
        let topics_dir = forum_dir.join("topics");
        fs::create_dir_all(&topics_dir).unwrap();
        fs::write(topics_dir.join("1001.json"), b"{not valid json").unwrap();

        let err = scan_prepare(root, "252", AGENT_SHA, ScanPrepareOpts::default()).unwrap_err();
        assert!(matches!(err, ScanPrepareError::Json { .. }));
    }

    #[test]
    fn test_skips_non_json_files() {
        let _td = tempfile::TempDir::new().unwrap();
        let root = _td.path();
        let forum_dir = root.join("forums").join("252");
        let topics_dir = forum_dir.join("topics");
        fs::create_dir_all(&topics_dir).unwrap();
        // A spurious non-JSON file in the topics dir is ignored.
        fs::write(topics_dir.join("README.txt"), b"ignore me").unwrap();
        write_topic(&topics_dir, "1001", 2000, 1, "ok");

        let report = scan_prepare(root, "252", AGENT_SHA, ScanPrepareOpts::default()).unwrap();
        assert_eq!(report.total, 1);
        assert_eq!(report.queued, 1);
    }

    #[test]
    fn test_queueline_round_trips_via_serde() {
        // Exercise the Deserialize impls on Payload, PayloadComment, QueueLine.
        let ql = QueueLine {
            topic_id: "1".into(),
            forum_id: "2".into(),
            last_post_id: "3".into(),
            agent_sha: "deadbeef".into(),
            scan_path: "path".into(),
            payload: Payload {
                title: "t".into(),
                opening_post: OpeningPost { text: "x".into() },
                comments: vec![PayloadComment {
                    author: "a".into(),
                    date: "d".into(),
                    text: "t".into(),
                }],
            },
            included_comments: 1,
            total_comments: 1,
        };
        let json = serde_json::to_string(&ql).unwrap();
        let back: QueueLine = serde_json::from_str(&json).unwrap();
        assert_eq!(back.topic_id, "1");
        assert_eq!(back.payload.comments.len(), 1);
    }

    /// US-008: topic JSON whose `title`, `opening_post`, and `comments`
    /// are all absent (empty object `{}`) must still be parseable because
    /// every field is `#[serde(default)]`. Exercises the defaults at L62–L70.
    #[test]
    fn test_empty_topic_json_uses_serde_defaults() {
        let _td = tempfile::TempDir::new().unwrap();
        let root = _td.path();
        let topics_dir = root.join("forums").join("252").join("topics");
        fs::create_dir_all(&topics_dir).unwrap();
        // Minimal JSON — all fields default.
        fs::write(topics_dir.join("1001.json"), b"{}").unwrap();

        let report = scan_prepare(root, "252", AGENT_SHA, ScanPrepareOpts::default()).unwrap();
        assert_eq!(report.total, 1);
        assert_eq!(report.queued, 1);
        let lines = read_queue_lines(&root.join("forums").join("252"));
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].total_comments, 0, "no comments in default topic");
        assert_eq!(lines[0].included_comments, 0);
        assert_eq!(lines[0].last_post_id, "0");
    }

    /// US-008: a topic whose single comment already exceeds the budget alone
    /// causes `build_payload` to include zero comments — the first push is
    /// rolled back on L277. Covers the edge case of a per-comment payload too
    /// large to fit.
    #[test]
    fn test_build_payload_single_huge_comment_included_comments_zero() {
        let _td = tempfile::TempDir::new().unwrap();
        let root = _td.path();
        let forum_dir = root.join("forums").join("252");
        let topics_dir = forum_dir.join("topics");
        // 2 KiB comment — exceeds a 256-byte budget on its own.
        write_topic(&topics_dir, "1001", 2000, 1, &"X".repeat(2048));
        let report = scan_prepare(
            root,
            "252",
            AGENT_SHA,
            ScanPrepareOpts {
                max_payload_bytes: 256,
            },
        )
        .unwrap();
        assert_eq!(report.queued, 1);
        let lines = read_queue_lines(&forum_dir);
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0].included_comments, 0,
            "huge comment must be rolled back under a tiny budget"
        );
        assert_eq!(lines[0].total_comments, 1);
    }

    /// US-008: the `topic_paths.sort()` call at L161 guarantees deterministic
    /// ordering of the output queue. Write three topics in reverse-lexical
    /// order and assert the queue file emits them ascending.
    #[test]
    fn test_output_queue_is_sorted_by_filename() {
        let _td = tempfile::TempDir::new().unwrap();
        let root = _td.path();
        let forum_dir = root.join("forums").join("252");
        let topics_dir = forum_dir.join("topics");
        // Intentionally write in reverse order.
        write_topic(&topics_dir, "3003", 2003, 1, "x");
        write_topic(&topics_dir, "1001", 2001, 1, "x");
        write_topic(&topics_dir, "2002", 2002, 1, "x");

        scan_prepare(root, "252", AGENT_SHA, ScanPrepareOpts::default()).unwrap();
        let lines = read_queue_lines(&forum_dir);
        let order: Vec<_> = lines.iter().map(|l| l.topic_id.clone()).collect();
        assert_eq!(
            order,
            vec!["1001".to_string(), "2002".to_string(), "3003".to_string()],
            "queue must be emitted in ascending filename order"
        );
    }

    #[test]
    fn test_empty_queue_when_all_cached() {
        let _td = tempfile::TempDir::new().unwrap();
        let root = _td.path();
        let forum_dir = root.join("forums").join("252");
        let topics_dir = forum_dir.join("topics");
        let scans_dir = forum_dir.join("scans");

        for (i, tid) in ["1001", "1002", "1003"].iter().enumerate() {
            write_topic(&topics_dir, tid, 2000 + i as u64, 1, "short");
            write_scan(&scans_dir, tid, AGENT_SHA, 2000 + i as u64);
        }

        let report = scan_prepare(root, "252", AGENT_SHA, ScanPrepareOpts::default()).unwrap();
        assert_eq!(report.total, 3);
        assert_eq!(report.skipped_cached, 3);
        assert_eq!(report.queued, 0);

        let queue_path = forum_dir.join("scan-queue.jsonl");
        assert!(queue_path.exists(), "queue file must exist even when empty");
        let meta = fs::metadata(&queue_path).unwrap();
        assert_eq!(meta.len(), 0, "empty queue must be 0 bytes");
    }
}
