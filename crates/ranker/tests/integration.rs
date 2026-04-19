//! Full-pipeline fixture integration (plan §6.3 `test_full_pipeline_on_fixture_forum`).
//!
//! This test copies a committed 3-film × 3-rips fixture directory into a tempdir,
//! runs `scan_prepare` (asserts cache-aware queueing behaviour), then calls the
//! pure-Rust aggregator + rip ranker directly on the scan outputs. NO agent
//! invocation anywhere — the `.scan.json` files are canned.

use std::fs;
use std::path::{Path, PathBuf};

use rutracker_mirror::topic_io::TopicFile;
use rutracker_ranker::{
    aggregate_film, film_id, film_key, parse_title, rank_rips, read_scan, scan_prepare, FilmTopic,
    RipCandidate, RipMetadata, ScanPrepareOpts,
};

const FIXTURE_FORUM_ID: &str = "252";
const FIXTURE_AGENT_SHA: &str = "fixtureintegrat";

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("integration")
}

fn tempdir_unique(_suffix: &str) -> tempfile::TempDir {
    tempfile::TempDir::new().unwrap()
}

fn copy_fixture_to(root: &Path) {
    let forum_dir = root.join("forums").join(FIXTURE_FORUM_ID);
    let topics_dst = forum_dir.join("topics");
    let scans_dst = forum_dir.join("scans");
    fs::create_dir_all(&topics_dst).unwrap();
    fs::create_dir_all(&scans_dst).unwrap();

    let src = fixture_dir();
    for entry in fs::read_dir(src.join("topics")).unwrap() {
        let e = entry.unwrap();
        fs::copy(e.path(), topics_dst.join(e.file_name())).unwrap();
    }
    for entry in fs::read_dir(src.join("scans")).unwrap() {
        let e = entry.unwrap();
        fs::copy(e.path(), scans_dst.join(e.file_name())).unwrap();
    }
}

/// For one film_id, build `(canonical ParsedTitle, Vec<(topic_id, RipMetadata, Option<TopicAnalysis>)>)`.
fn collect_film(
    root: &Path,
    film_target: &str,
) -> (
    rutracker_ranker::ParsedTitle,
    Vec<(String, RipMetadata, Option<rutracker_ranker::TopicAnalysis>)>,
) {
    let topics_dir = root.join("forums").join(FIXTURE_FORUM_ID).join("topics");
    let scans_dir = root.join("forums").join(FIXTURE_FORUM_ID).join("scans");

    let mut canonical: Option<rutracker_ranker::ParsedTitle> = None;
    let mut rows: Vec<(String, RipMetadata, Option<rutracker_ranker::TopicAnalysis>)> = Vec::new();
    let mut best_seeds: i64 = -1;

    let mut entries: Vec<PathBuf> = fs::read_dir(&topics_dir)
        .unwrap()
        .filter_map(|r| r.ok())
        .map(|e| e.path())
        .collect();
    entries.sort();

    for path in entries {
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let bytes = fs::read(&path).unwrap();
        let tf: TopicFile = serde_json::from_slice(&bytes).unwrap();
        let parsed = parse_title(&tf.title).expect("fixture title must parse");
        let fid = film_id(&film_key(&parsed));
        if fid != film_target {
            continue;
        }
        let md = RipMetadata::from_topic_file(&tf, &parsed);
        // Pick canonical as the highest-seeds topic.
        let seeds = tf.seeds.map(|v| v as i64).unwrap_or(0);
        if seeds > best_seeds {
            best_seeds = seeds;
            canonical = Some(parsed.clone());
        }

        let scan_path = scans_dir.join(format!("{stem}.scan.json"));
        let analysis = read_scan(&scan_path).ok().map(|sf| sf.analysis);
        rows.push((stem.to_string(), md, analysis));
    }

    (canonical.expect("at least one topic per film"), rows)
}

fn film_id_for(title: &str) -> String {
    let parsed = parse_title(title).expect("fixture title must parse");
    film_id(&film_key(&parsed))
}

#[test]
fn test_full_pipeline_on_fixture_forum() {
    let _td = tempdir_unique("full-pipeline");
    let root = _td.path();
    copy_fixture_to(root);

    // 1. scan_prepare — all 9 topics already have matching .scan.json, so the
    //    queue should be empty.
    let report = scan_prepare(
        root,
        FIXTURE_FORUM_ID,
        FIXTURE_AGENT_SHA,
        ScanPrepareOpts::default(),
    )
    .unwrap();
    assert_eq!(report.total, 9, "expected 9 topic fixtures");
    assert_eq!(
        report.skipped_cached, 9,
        "all topics must hit the cache on first scan_prepare"
    );
    assert_eq!(report.queued, 0, "nothing should be queued");

    // 2. Resolve the 3 film_ids from known fixture titles.
    let film_a = film_id_for(
        "Альфа / Alpha (Александр Первый / Aleksandr Pervyj) [2025, США, драма, WEB-DLRip] Dub (Videofilm)",
    );
    let film_b = film_id_for(
        "Бета / Beta (Борис Второй / Boris Vtoroj) [2024, США, комедия, WEB-DLRip] Dub (Videofilm)",
    );
    let film_c = film_id_for(
        "Гамма / Gamma (Григорий Третий / Grigorij Tretij) [2023, США, ужасы, WEB-DLRip] Dub (Videofilm)",
    );
    assert_ne!(film_a, film_b);
    assert_ne!(film_b, film_c);
    assert_ne!(film_a, film_c);

    // 3. Aggregate each film via the public Rust API.
    let now = chrono::Utc::now();

    let (canon_a, rows_a) = collect_film(root, &film_a);
    let topics_a: Vec<FilmTopic> = rows_a
        .iter()
        .map(|(tid, _, a)| FilmTopic {
            topic_id: tid.as_str(),
            analysis: a.as_ref(),
        })
        .collect();
    let score_a = aggregate_film(&film_a, &canon_a, &topics_a);

    let (canon_b, rows_b) = collect_film(root, &film_b);
    let topics_b: Vec<FilmTopic> = rows_b
        .iter()
        .map(|(tid, _, a)| FilmTopic {
            topic_id: tid.as_str(),
            analysis: a.as_ref(),
        })
        .collect();
    let score_b = aggregate_film(&film_b, &canon_b, &topics_b);

    let (canon_c, rows_c) = collect_film(root, &film_c);
    let topics_c: Vec<FilmTopic> = rows_c
        .iter()
        .map(|(tid, _, a)| FilmTopic {
            topic_id: tid.as_str(),
            analysis: a.as_ref(),
        })
        .collect();
    let score_c = aggregate_film(&film_c, &canon_c, &topics_c);

    // 4. Distinct film_ids, 3 topics each.
    assert_eq!(score_a.topic_count_total, 3);
    assert_eq!(score_b.topic_count_total, 3);
    assert_eq!(score_c.topic_count_total, 3);

    // 5. Score bands per plan §6.3 / US-004 criterion.
    assert!(
        score_a.score > 8.0,
        "top film must score above 8.0, got {}",
        score_a.score
    );
    assert!(
        (score_b.score - 6.0).abs() < 1.0,
        "middle film should land near 6.0, got {}",
        score_b.score
    );
    assert!(
        score_c.score < 4.0,
        "bottom film should fall below 4.0, got {}",
        score_c.score
    );

    // Canonical of Film A must be the Русское/English titles we expect.
    assert_eq!(score_a.canonical_title_ru, "Альфа");
    assert_eq!(score_a.canonical_title_en.as_deref(), Some("Alpha"));

    // 6. For each film, rank_rips returns 3 candidates sorted descending.
    let cands_a: Vec<RipCandidate> = rows_a
        .iter()
        .map(|(tid, md, a)| RipCandidate {
            topic_id: tid.as_str(),
            metadata: md,
            analysis: a.as_ref(),
        })
        .collect();
    let ranked_a = rank_rips(&cands_a, now);
    assert_eq!(ranked_a.len(), 3);
    for w in ranked_a.windows(2) {
        assert!(
            w[0].score >= w[1].score,
            "ranks must be descending: {} vs {}",
            w[0].score,
            w[1].score
        );
    }

    // 7. Film A best rip must be topic 6000001 (WEB-DLRip + top seeds → strictly best).
    assert_eq!(
        ranked_a[0].topic_id, "6000001",
        "Film A best rip must be WEB-DLRip topic 6000001"
    );

    let cands_b: Vec<RipCandidate> = rows_b
        .iter()
        .map(|(tid, md, a)| RipCandidate {
            topic_id: tid.as_str(),
            metadata: md,
            analysis: a.as_ref(),
        })
        .collect();
    let ranked_b = rank_rips(&cands_b, now);
    assert_eq!(ranked_b.len(), 3);
    assert_eq!(
        ranked_b[0].topic_id, "6000004",
        "Film B best rip must be WEB-DLRip + Dub topic 6000004"
    );

    let cands_c: Vec<RipCandidate> = rows_c
        .iter()
        .map(|(tid, md, a)| RipCandidate {
            topic_id: tid.as_str(),
            metadata: md,
            analysis: a.as_ref(),
        })
        .collect();
    let ranked_c = rank_rips(&cands_c, now);
    assert_eq!(ranked_c.len(), 3);
    assert_eq!(
        ranked_c[0].topic_id, "6000007",
        "Film C best rip must be WEB-DLRip topic 6000007"
    );
}
