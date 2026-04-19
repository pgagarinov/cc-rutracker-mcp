//! Stage D — within-film rip selection (plan §5).
//!
//! Composite-scores individual topics of the same film using five axes:
//! tech quality, format preference, audio preference, torrent health, and
//! recency of last fetch. No I/O — callers prepare [`RipMetadata`] (from
//! [`crate::rip_metadata`]) and optional [`TopicAnalysis`] (from
//! [`crate::scan_io`]) and hand them in.

use chrono::{DateTime, Utc};

use crate::rip_metadata::RipMetadata;
use crate::scan_io::TopicAnalysis;

/// One candidate release to rank within a film.
#[derive(Debug, Clone, Copy)]
pub struct RipCandidate<'a> {
    pub topic_id: &'a str,
    pub metadata: &'a RipMetadata,
    pub analysis: Option<&'a TopicAnalysis>,
}

/// Per-candidate composite score + per-axis rationale.
#[derive(Debug, Clone, PartialEq)]
pub struct RipRanking {
    pub topic_id: String,
    /// Composite 0..1 score (plan §5.1 weights).
    pub score: f32,
    pub rationale: RipRationale,
}

/// Per-axis breakdown in `[0, 1]` — persisted so the CLI can explain *why*
/// a rip ranks where it does.
#[derive(Debug, Clone, PartialEq)]
pub struct RipRationale {
    pub tech_quality: f32,
    pub format_preference: f32,
    pub audio_preference: f32,
    pub health: f32,
    pub recency: f32,
}

/// Weights from plan §5.1. Summed: 0.40 + 0.20 + 0.15 + 0.15 + 0.10 = 1.00.
const W_TECH: f32 = 0.40;
const W_FORMAT: f32 = 0.20;
const W_AUDIO: f32 = 0.15;
const W_HEALTH: f32 = 0.15;
const W_RECENCY: f32 = 0.10;

/// Score every candidate and return them sorted by `score` desc. Ties broken
/// by `seeds` desc so a healthier release wins at the same composite score.
pub fn rank_rips(candidates: &[RipCandidate], now: DateTime<Utc>) -> Vec<RipRanking> {
    let mut out: Vec<(RipRanking, u32)> = candidates
        .iter()
        .map(|c| {
            let rationale = RipRationale {
                tech_quality: score_tech_quality(c.analysis),
                format_preference: score_format(c.metadata.format_tag.as_deref()),
                audio_preference: score_audio(c.metadata.dub_info.as_deref()),
                health: score_health(c.metadata.seeds, c.metadata.downloads),
                recency: score_recency(c.metadata.fetched_at.as_deref(), now),
            };
            let score = W_TECH * rationale.tech_quality
                + W_FORMAT * rationale.format_preference
                + W_AUDIO * rationale.audio_preference
                + W_HEALTH * rationale.health
                + W_RECENCY * rationale.recency;
            let seeds = c.metadata.seeds.unwrap_or(0);
            (
                RipRanking {
                    topic_id: c.topic_id.to_string(),
                    score,
                    rationale,
                },
                seeds,
            )
        })
        .collect();

    out.sort_by(|(a, a_seeds), (b, b_seeds)| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b_seeds.cmp(a_seeds))
    });

    out.into_iter().map(|(r, _)| r).collect()
}

/// `(tech_praise_true − tech_complaints_true) / 5` clamped to `[0, 1]`.
/// Missing analysis → 0.5 (neutral — no positive or negative evidence).
fn score_tech_quality(analysis: Option<&TopicAnalysis>) -> f32 {
    match analysis {
        None => 0.5,
        Some(a) => {
            let praise = count_tech_flags(&a.tech_praise) as f32;
            let complaints = count_tech_flags(&a.tech_complaints) as f32;
            let raw = (praise - complaints) / 5.0;
            raw.clamp(0.0, 1.0)
        }
    }
}

fn count_tech_flags(q: &crate::scan_io::TechQuality) -> u32 {
    [q.audio, q.video, q.subtitles, q.dubbing, q.sync]
        .iter()
        .filter(|&&x| x)
        .count() as u32
}

/// Plan §5.1 format preference. Matches are substring, case-sensitive against
/// the canonical rutracker spelling.
fn score_format(tag: Option<&str>) -> f32 {
    let Some(t) = tag else {
        return 0.5;
    };
    // Order matters: more-specific tokens first so "WEB-DLRip-AVC" lands on
    // the WEB-DLRip branch (not WEBRip, not HDRip).
    if t.contains("WEB-DLRip") {
        0.9
    } else if t.contains("HDRip") {
        0.7
    } else if t.contains("WEBRip") {
        0.6
    } else if t.contains("TS") {
        0.3
    } else if t.contains("CAMRip") || t.contains("CAM") {
        0.1
    } else {
        // BDRip, DVDRip, HDTVRip, UHDRip, anything exotic → mid.
        0.5
    }
}

/// Plan §5.1 audio preference. Input is the raw `dub_info` slice from the
/// parsed title.
fn score_audio(dub: Option<&str>) -> f32 {
    let Some(d) = dub else {
        return 0.2;
    };
    // Count tokens matching "Dub" on whole-word boundaries (ASCII-ish).
    let dub_count = count_word(d, "Dub");
    if d.contains("Dub") && dub_count >= 2 {
        0.9
    } else if d.contains("Dub") {
        0.8
    } else if d.contains("MVO") {
        0.6
    } else if d.contains("VO") {
        0.4
    } else {
        0.2
    }
}

/// Whole-word occurrence count — used so the substring `MVO` doesn't trigger
/// a `VO` match and a stray `Dubstep` (unlikely in titles but cheap to guard)
/// doesn't count as a Dub token.
fn count_word(haystack: &str, needle: &str) -> usize {
    let bytes = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || bytes.len() < n.len() {
        return 0;
    }
    let mut count = 0;
    let mut i = 0;
    while i + n.len() <= bytes.len() {
        if &bytes[i..i + n.len()] == n {
            let before_ok = i == 0 || !is_word_char(bytes[i - 1]);
            let after = i + n.len();
            let after_ok = after == bytes.len() || !is_word_char(bytes[after]);
            if before_ok && after_ok {
                count += 1;
                i += n.len();
                continue;
            }
        }
        i += 1;
    }
    count
}

fn is_word_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Torrent health = `(seeds + 0.1 × downloads) / 50`, capped at 1.0.
/// Missing → 0.
fn score_health(seeds: Option<u32>, downloads: Option<u32>) -> f32 {
    let s = seeds.unwrap_or(0) as f32;
    let d = downloads.unwrap_or(0) as f32;
    ((s + 0.1 * d) / 50.0).min(1.0)
}

/// `exp(-days_since_fetched_at / 30)`. Unparseable → 0.5.
fn score_recency(fetched_at: Option<&str>, now: DateTime<Utc>) -> f32 {
    let Some(s) = fetched_at else {
        return 0.5;
    };
    let Ok(ts) = DateTime::parse_from_rfc3339(s) else {
        return 0.5;
    };
    let ts_utc = ts.with_timezone(&Utc);
    let seconds = (now - ts_utc).num_seconds();
    let days = seconds.max(0) as f32 / 86_400.0;
    (-days / 30.0).exp()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan_io::{TechQuality, TopicAnalysis};
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 4, 18, 12, 0, 0).unwrap()
    }

    fn base_metadata() -> RipMetadata {
        RipMetadata {
            seeds: Some(50),
            leeches: Some(5),
            downloads: Some(100),
            size_bytes: Some(2_000_000_000),
            format_tag: Some("WEB-DLRip".to_string()),
            fetched_at: Some("2026-04-10T12:00:00+00:00".to_string()),
            dub_info: Some("Dub".to_string()),
        }
    }

    fn clean_analysis() -> TopicAnalysis {
        // Baseline with two tech-praise flags so a single complaint moves the
        // tech_quality axis down from 0.4 → 0.2 (still in [0,1] range,
        // not squashed by the clamp).
        TopicAnalysis {
            sentiment_score: 7.0,
            confidence: 0.8,
            themes_positive: vec![],
            themes_negative: vec![],
            tech_complaints: TechQuality::default(),
            tech_praise: TechQuality {
                audio: true,
                video: true,
                subtitles: false,
                dubbing: false,
                sync: false,
            },
            substantive_count: 10,
            red_flags: vec![],
            relevance: 1.0,
        }
    }

    #[test]
    fn test_dlrip_beats_webrip_all_else_equal() {
        let md_dlrip = base_metadata();
        let mut md_webrip = base_metadata();
        md_webrip.format_tag = Some("WEBRip".to_string());

        let candidates = [
            RipCandidate {
                topic_id: "dlrip",
                metadata: &md_dlrip,
                analysis: None,
            },
            RipCandidate {
                topic_id: "webrip",
                metadata: &md_webrip,
                analysis: None,
            },
        ];
        let ranked = rank_rips(&candidates, now());
        assert_eq!(ranked[0].topic_id, "dlrip");
        let dlrip = ranked.iter().find(|r| r.topic_id == "dlrip").unwrap();
        let webrip = ranked.iter().find(|r| r.topic_id == "webrip").unwrap();
        assert!(
            dlrip.score > webrip.score,
            "WEB-DLRip ({}) must outrank WEBRip ({})",
            dlrip.score,
            webrip.score
        );
    }

    #[test]
    fn test_audio_complaint_ranks_rip_lower() {
        let md = base_metadata();
        let clean = clean_analysis();
        let mut complaining = clean_analysis();
        complaining.tech_complaints.audio = true;

        let candidates = [
            RipCandidate {
                topic_id: "clean",
                metadata: &md,
                analysis: Some(&clean),
            },
            RipCandidate {
                topic_id: "audio_complaint",
                metadata: &md,
                analysis: Some(&complaining),
            },
        ];
        let ranked = rank_rips(&candidates, now());
        let clean_r = ranked.iter().find(|r| r.topic_id == "clean").unwrap();
        let bad = ranked
            .iter()
            .find(|r| r.topic_id == "audio_complaint")
            .unwrap();
        assert!(
            clean_r.score > bad.score,
            "clean sibling ({}) must outrank the one with audio complaint ({})",
            clean_r.score,
            bad.score
        );
    }

    #[test]
    fn test_dead_torrent_deprioritized() {
        let mut md_alive = base_metadata();
        md_alive.seeds = Some(100);
        let mut md_dead = base_metadata();
        md_dead.seeds = Some(0);

        let candidates = [
            RipCandidate {
                topic_id: "alive",
                metadata: &md_alive,
                analysis: None,
            },
            RipCandidate {
                topic_id: "dead",
                metadata: &md_dead,
                analysis: None,
            },
        ];
        let ranked = rank_rips(&candidates, now());
        assert_eq!(ranked[0].topic_id, "alive");
        let alive = ranked.iter().find(|r| r.topic_id == "alive").unwrap();
        let dead = ranked.iter().find(|r| r.topic_id == "dead").unwrap();
        assert!(
            alive.score > dead.score,
            "seeds=100 ({}) must outrank seeds=0 ({})",
            alive.score,
            dead.score
        );
    }

    #[test]
    fn test_recency_decays_exponentially() {
        // Very old fetch → recency → 0. Fresh fetch → recency → 1.
        let mut md_fresh = base_metadata();
        md_fresh.fetched_at = Some("2026-04-18T12:00:00+00:00".to_string());
        let mut md_old = base_metadata();
        md_old.fetched_at = Some("2025-04-18T12:00:00+00:00".to_string()); // 365 days

        let candidates = [
            RipCandidate {
                topic_id: "fresh",
                metadata: &md_fresh,
                analysis: None,
            },
            RipCandidate {
                topic_id: "old",
                metadata: &md_old,
                analysis: None,
            },
        ];
        let ranked = rank_rips(&candidates, now());
        let fresh = ranked.iter().find(|r| r.topic_id == "fresh").unwrap();
        let old = ranked.iter().find(|r| r.topic_id == "old").unwrap();
        assert!(fresh.rationale.recency > 0.95, "fresh should be ≈ 1.0");
        assert!(old.rationale.recency < 0.01, "year-old should decay to ~0");
    }

    #[test]
    fn test_audio_multi_dub_beats_single_dub() {
        // Two separate Dub tokens → 0.9; one → 0.8.
        let mut md_multi = base_metadata();
        md_multi.dub_info = Some("Dub | Dub (Videofilm)".to_string());
        let md_single = base_metadata();

        assert_eq!(score_audio(md_multi.dub_info.as_deref()), 0.9);
        assert_eq!(score_audio(md_single.dub_info.as_deref()), 0.8);
    }

    #[test]
    fn test_format_preference_known_tokens() {
        assert_eq!(score_format(Some("WEB-DLRip-AVC")), 0.9);
        assert_eq!(score_format(Some("WEB-DLRip")), 0.9);
        assert_eq!(score_format(Some("HDRip")), 0.7);
        assert_eq!(score_format(Some("WEBRip")), 0.6);
        assert_eq!(score_format(Some("TS")), 0.3);
        assert_eq!(score_format(Some("CAMRip")), 0.1);
        assert_eq!(score_format(Some("BDRip")), 0.5);
        assert_eq!(score_format(Some("DVDRip")), 0.5);
        assert_eq!(score_format(None), 0.5);
    }
}
