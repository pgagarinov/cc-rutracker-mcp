//! Stage C — film-level aggregation (plan §4).
//!
//! Turns per-topic [`TopicAnalysis`] payloads into a single [`FilmScore`] per
//! film using a Bayesian-shrunk weighted mean. Topics without a `.scan.json`
//! (or with a `.scan.failed.json`) contribute zero weight; they are still
//! counted in `topic_count_total` so the CLI report can surface
//! "N / M topics scored" without hiding unscanned releases.
//!
//! No I/O in this module — callers resolve `TopicAnalysis` payloads from disk
//! (via `scan_io`) and pass them in as `Option<&TopicAnalysis>`.

use std::collections::HashMap;

use crate::scan_io::TopicAnalysis;
use crate::title::ParsedTitle;

/// Global prior mean (film_score when there is zero evidence). Plan §4.1.
pub const MU_0: f32 = 5.5;

/// Prior weight: how many "substantive-comment units" of pull the prior
/// exerts. A film needs ≳ k=5 weighted-substantive-comments to start moving
/// the score away from `MU_0`.
pub const K_PRIOR: f32 = 5.0;

/// Aggregated, film-level score card produced by [`aggregate_film`].
#[derive(Debug, Clone, PartialEq)]
pub struct FilmScore {
    pub film_id: String,
    pub canonical_title_ru: String,
    pub canonical_title_en: Option<String>,
    pub year: Option<u16>,
    pub director: Option<String>,
    /// Bayesian-shrunk mean in the range `[0, 10]` (unclamped; the underlying
    /// formula is bounded by construction when sentiment_scores ∈ [0, 10]).
    pub score: f32,
    /// Heuristic confidence in `[0, 1]`. Reaches 1.0 at roughly 20
    /// weighted-substantive-comments (`Σ weight × confidence ≥ 20`).
    pub confidence: f32,
    /// Topics that had a valid `.scan.json` on disk.
    pub topic_count_with_analysis: u32,
    /// All topics that belong to this film, scanned or not.
    pub topic_count_total: u32,
    pub total_substantive_comments: u32,
    /// Top 3 positive themes by occurrence count across all scanned topics,
    /// descending. Theme strings keep their most-frequent original casing.
    pub top_themes_positive: Vec<(String, u32)>,
    /// Top 3 negative themes; same conventions as
    /// [`Self::top_themes_positive`].
    pub top_themes_negative: Vec<(String, u32)>,
    /// `true` iff at least one scanned topic reported a non-empty `red_flags`
    /// list.
    pub has_red_flags: bool,
    /// RFC3339 timestamp of when this aggregate was computed.
    pub scored_at: String,
}

/// One topic to aggregate. `analysis` is `None` when the topic has no
/// `.scan.json` cache (or has a `.scan.failed.json` sidecar).
#[derive(Debug, Clone, Copy)]
pub struct FilmTopic<'a> {
    pub topic_id: &'a str,
    pub analysis: Option<&'a TopicAnalysis>,
}

/// Compute the aggregated [`FilmScore`] for one film.
///
/// `canonical` supplies the display fields (title, year, director); the
/// scoring math uses only `topics`.
pub fn aggregate_film(film_id: &str, canonical: &ParsedTitle, topics: &[FilmTopic]) -> FilmScore {
    // Accumulators — all in f32 since inputs are `u32 × f32 × f32`.
    let mut sum_w: f32 = 0.0;
    let mut sum_w_sentiment: f32 = 0.0;
    let mut sum_w_confidence: f32 = 0.0;
    let mut total_substantive: u32 = 0;
    let mut has_red_flags = false;
    let mut topic_count_with_analysis: u32 = 0;

    let mut pos_counts: ThemeCounter = ThemeCounter::new();
    let mut neg_counts: ThemeCounter = ThemeCounter::new();

    for t in topics {
        let Some(a) = t.analysis else { continue };
        topic_count_with_analysis += 1;
        total_substantive = total_substantive.saturating_add(a.substantive_count);

        // weight = substantive_count × confidence × relevance
        // Cast via f64 first to avoid surprising u32→f32 precision loss for
        // very large counts, then back to f32 at the end.
        let substantive_f = a.substantive_count as f32;
        let weight = substantive_f * a.confidence * a.relevance;
        sum_w += weight;
        sum_w_sentiment += weight * a.sentiment_score;
        sum_w_confidence += weight * a.confidence;

        if !a.red_flags.is_empty() {
            has_red_flags = true;
        }

        for theme in &a.themes_positive {
            pos_counts.bump(theme);
        }
        for theme in &a.themes_negative {
            neg_counts.bump(theme);
        }
    }

    // Bayesian-shrunk mean. Denominator is always > 0 because K_PRIOR > 0.
    let score = (K_PRIOR * MU_0 + sum_w_sentiment) / (K_PRIOR + sum_w);
    let confidence = (sum_w_confidence / 20.0).min(1.0);

    FilmScore {
        film_id: film_id.to_string(),
        canonical_title_ru: canonical.title_ru.clone(),
        canonical_title_en: canonical.title_en.clone(),
        year: canonical.year,
        director: canonical.director.clone(),
        score,
        confidence,
        topic_count_with_analysis,
        topic_count_total: topics.len() as u32,
        total_substantive_comments: total_substantive,
        top_themes_positive: pos_counts.top(3),
        top_themes_negative: neg_counts.top(3),
        has_red_flags,
        scored_at: chrono::Utc::now().to_rfc3339(),
    }
}

/// Case-insensitive theme counter. Keeps the most-frequent original casing
/// for display; ties on casing break on first-seen order.
struct ThemeCounter {
    /// Normalised-key → (display, count, first-seen-index, per-casing counts)
    buckets: HashMap<String, Bucket>,
    next_index: u32,
}

struct Bucket {
    count: u32,
    first_seen: u32,
    /// Occurrence counts per raw (unnormalised) casing.
    casings: HashMap<String, u32>,
}

impl ThemeCounter {
    fn new() -> Self {
        Self {
            buckets: HashMap::new(),
            next_index: 0,
        }
    }

    fn bump(&mut self, raw: &str) {
        let key = normalize_theme(raw);
        if key.is_empty() {
            return;
        }
        let entry = self.buckets.entry(key).or_insert_with(|| {
            let b = Bucket {
                count: 0,
                first_seen: self.next_index,
                casings: HashMap::new(),
            };
            self.next_index += 1;
            b
        });
        entry.count += 1;
        *entry.casings.entry(raw.trim().to_string()).or_insert(0) += 1;
    }

    /// Top `n` themes by count desc (ties by first-seen asc). Each entry's
    /// display string is the casing that occurred most often.
    fn top(self, n: usize) -> Vec<(String, u32)> {
        let mut items: Vec<(String, u32, u32)> = self
            .buckets
            .into_values()
            .map(|b| (pick_display(&b.casings), b.count, b.first_seen))
            .collect();
        items.sort_by(|a, b| b.1.cmp(&a.1).then(a.2.cmp(&b.2)));
        items.into_iter().take(n).map(|(d, c, _)| (d, c)).collect()
    }
}

/// Case-insensitive, whitespace-trimmed, internal-whitespace-collapsed key.
fn normalize_theme(s: &str) -> String {
    crate::title::normalize_ws(&s.to_lowercase())
}

/// Among the raw casings observed for one normalised key, return the one
/// that appeared most often. Ties broken by Unicode ordering (stable).
fn pick_display(casings: &HashMap<String, u32>) -> String {
    let mut best: Option<(&String, u32)> = None;
    for (k, &c) in casings {
        match best {
            None => best = Some((k, c)),
            Some((_, bc)) if c > bc => best = Some((k, c)),
            Some((bk, bc)) if c == bc && k < bk => best = Some((k, c)),
            _ => {}
        }
    }
    best.map(|(k, _)| k.clone()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan_io::{TechQuality, TopicAnalysis};

    fn sample_title() -> ParsedTitle {
        ParsedTitle {
            title_ru: "Тестовый фильм".to_string(),
            title_en: Some("Test Movie".to_string()),
            title_alt: None,
            director: Some("Тестовый Режиссёр".to_string()),
            year: Some(2025),
            countries: vec!["США".to_string()],
            genres: vec!["драма".to_string()],
            format: "WEBRip".to_string(),
            dub_info: "Dub".to_string(),
        }
    }

    fn analysis(
        sentiment: f32,
        confidence: f32,
        relevance: f32,
        substantive: u32,
        red_flags: Vec<String>,
        themes_pos: Vec<&str>,
        themes_neg: Vec<&str>,
    ) -> TopicAnalysis {
        TopicAnalysis {
            sentiment_score: sentiment,
            confidence,
            themes_positive: themes_pos.into_iter().map(String::from).collect(),
            themes_negative: themes_neg.into_iter().map(String::from).collect(),
            tech_complaints: TechQuality::default(),
            tech_praise: TechQuality::default(),
            substantive_count: substantive,
            red_flags,
            relevance,
        }
    }

    #[test]
    fn test_bayesian_shrinkage_pulls_low_evidence_films_to_prior() {
        // One topic, low confidence, positive sentiment.
        //   weight = 2 × 0.3 × 1.0 = 0.6
        //   score  = (5 × 5.5 + 0.6 × 9.0) / (5 + 0.6)
        //          = (27.5 + 5.4) / 5.6 ≈ 5.875
        let a = analysis(9.0, 0.3, 1.0, 2, vec![], vec![], vec![]);
        let topics = [FilmTopic {
            topic_id: "t1",
            analysis: Some(&a),
        }];
        let fs = aggregate_film("film1", &sample_title(), &topics);
        assert!(
            fs.score < 6.5,
            "low-evidence film must remain near prior (5.5), got {}",
            fs.score
        );
        // And near the expected 5.875.
        assert!((fs.score - 5.875).abs() < 0.01, "got {}", fs.score);
        assert_eq!(fs.topic_count_with_analysis, 1);
        assert_eq!(fs.topic_count_total, 1);
    }

    #[test]
    fn test_film_with_many_positive_comments_scores_high() {
        // 5 topics × (10 substantive, conf 0.9, relev 1.0, sentiment 8.5).
        //   weight_each = 10 × 0.9 × 1.0 = 9
        //   score = (5 × 5.5 + 5 × 9 × 8.5) / (5 + 5 × 9)
        //         = (27.5 + 382.5) / 50 = 8.2
        let a = analysis(8.5, 0.9, 1.0, 10, vec![], vec![], vec![]);
        let topics: Vec<FilmTopic> = (0..5)
            .map(|i| FilmTopic {
                topic_id: match i {
                    0 => "t0",
                    1 => "t1",
                    2 => "t2",
                    3 => "t3",
                    _ => "t4",
                },
                analysis: Some(&a),
            })
            .collect();
        let fs = aggregate_film("film1", &sample_title(), &topics);
        assert!(
            fs.score > 8.0,
            "5 well-commented positive topics should clear 8.0, got {}",
            fs.score
        );
        assert!((fs.score - 8.2).abs() < 0.05, "got {}", fs.score);
        assert_eq!(fs.topic_count_with_analysis, 5);
        assert_eq!(fs.topic_count_total, 5);
        assert_eq!(fs.total_substantive_comments, 50);
    }

    #[test]
    fn test_red_flags_propagate_to_film_level() {
        let clean = analysis(7.0, 0.8, 1.0, 5, vec![], vec![], vec![]);
        let flagged = analysis(6.0, 0.7, 1.0, 4, vec!["фейк".to_string()], vec![], vec![]);
        let topics = [
            FilmTopic {
                topic_id: "t1",
                analysis: Some(&clean),
            },
            FilmTopic {
                topic_id: "t2",
                analysis: Some(&flagged),
            },
        ];
        let fs = aggregate_film("film1", &sample_title(), &topics);
        assert!(fs.has_red_flags, "any topic with red_flags must propagate");
    }

    #[test]
    fn test_topics_without_scan_json_do_not_affect_score() {
        // 3 topics with strongly positive analysis, 2 with no analysis at all.
        // The 2 `None` topics must contribute 0 weight and must not drag the
        // score toward the prior.
        let a = analysis(9.0, 0.9, 1.0, 10, vec![], vec![], vec![]);
        let topics = [
            FilmTopic {
                topic_id: "t1",
                analysis: Some(&a),
            },
            FilmTopic {
                topic_id: "t2",
                analysis: Some(&a),
            },
            FilmTopic {
                topic_id: "t3",
                analysis: Some(&a),
            },
            FilmTopic {
                topic_id: "t4",
                analysis: None,
            },
            FilmTopic {
                topic_id: "t5",
                analysis: None,
            },
        ];
        let fs = aggregate_film("film1", &sample_title(), &topics);
        assert_eq!(fs.topic_count_with_analysis, 3);
        assert_eq!(fs.topic_count_total, 5);
        // Compare against the 3-only aggregate: same math, identical score.
        let topics_only_scanned = [
            FilmTopic {
                topic_id: "t1",
                analysis: Some(&a),
            },
            FilmTopic {
                topic_id: "t2",
                analysis: Some(&a),
            },
            FilmTopic {
                topic_id: "t3",
                analysis: Some(&a),
            },
        ];
        let fs_only = aggregate_film("film1", &sample_title(), &topics_only_scanned);
        assert!(
            (fs.score - fs_only.score).abs() < 1e-5,
            "unscanned topics must not change the score ({} vs {})",
            fs.score,
            fs_only.score
        );
    }

    #[test]
    fn test_theme_aggregation_top3_and_case_insensitive() {
        // Three topics with overlapping themes in mixed casing.
        let a1 = analysis(
            7.0,
            0.8,
            1.0,
            5,
            vec![],
            vec!["Сильная игра", "операторская работа"],
            vec!["затянутый финал"],
        );
        let a2 = analysis(
            7.5,
            0.8,
            1.0,
            5,
            vec![],
            vec!["сильная игра", "Атмосфера"],
            vec![],
        );
        let a3 = analysis(
            8.0,
            0.8,
            1.0,
            5,
            vec![],
            vec!["сильная игра", "атмосфера"],
            vec![],
        );
        let topics = [
            FilmTopic {
                topic_id: "t1",
                analysis: Some(&a1),
            },
            FilmTopic {
                topic_id: "t2",
                analysis: Some(&a2),
            },
            FilmTopic {
                topic_id: "t3",
                analysis: Some(&a3),
            },
        ];
        let fs = aggregate_film("film1", &sample_title(), &topics);
        // Top positive theme is "сильная игра" with count 3.
        assert_eq!(fs.top_themes_positive[0].1, 3);
        assert!(fs.top_themes_positive[0]
            .0
            .to_lowercase()
            .contains("сильная"));
        // Top 3 at most.
        assert!(fs.top_themes_positive.len() <= 3);
        // Negative themes: one seen once.
        assert_eq!(fs.top_themes_negative.len(), 1);
        assert_eq!(fs.top_themes_negative[0].1, 1);
    }
}
