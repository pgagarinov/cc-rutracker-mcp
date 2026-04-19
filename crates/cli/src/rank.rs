//! CLI handlers for the `rutracker rank …` subcommand tree.
//!
//! The ranker commands are local-only — they read topic JSONs, `.scan.json`
//! files, and SQLite from the mirror root, and write back to SQLite. No
//! network. No cookies. All stages (match / scan-prepare / aggregate / list /
//! show / parse-failures) live here so main.rs stays a thin clap wrapper.
//!
//! Stage boundaries match plan §1 / §6.2:
//! - `run_rank_match` — parse titles, populate `film_index` + `film_topic`.
//! - `run_rank_scan_prepare` — wrap `ranker::scan_prepare::scan_prepare`.
//! - `run_rank_aggregate` — read scan files, compute Bayesian film scores, rank rips.
//! - `run_rank_list` — query `film_score` with filters.
//! - `run_rank_show` — detail view for one film.
//! - `run_rank_parse_failures` — dump the parse-failure log.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::Serialize;
use serde_json::json;

use rutracker_mirror::topic_io::TopicFile;
use rutracker_ranker::{
    agent_sha_current, aggregate_film, film_id, film_key, parse_title, rank_rips, read_scan,
    scan_prepare, FilmTopic, ParsedTitle, RipCandidate, RipMetadata, ScanPrepareOpts,
    TopicAnalysis,
};

use crate::{emit, mirror_root_for, render, CliConfig};

// ---------- arg structs ----------

#[derive(Debug, Clone, Default)]
pub struct RankMatchArgs {
    pub forum: Option<String>,
    pub root: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct RankScanPrepareArgs {
    pub forum: String,
    pub max_payload_bytes: Option<usize>,
    pub root: Option<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct RankAggregateArgs {
    pub forum: Option<String>,
    pub root: Option<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct RankListArgs {
    pub forum: Option<String>,
    pub min_score: Option<f32>,
    pub top: Option<u32>,
    pub root: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct RankShowArgs {
    pub query: String,
    pub root: Option<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct RankParseFailuresArgs {
    pub root: Option<PathBuf>,
}

// ---------- helpers shared by all rank handlers ----------

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339()
}

fn parse_failures_log_path(root: &Path) -> PathBuf {
    root.join("logs").join("rank-parse-failures.log")
}

fn append_parse_failure(root: &Path, forum_id: &str, topic_id: &str, title: &str, err: &str) {
    let path = parse_failures_log_path(root);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let line = format!(
        "{}\tforum={}\ttopic={}\terr={}\ttitle={}\n",
        now_rfc3339(),
        forum_id,
        topic_id,
        err,
        title,
    );
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// List the forum ids to process for the given `--forum` argument. When
/// `None`, returns the union of the watchlist + any forum directory that
/// already exists on disk. Deterministic order (sorted).
fn forum_ids_for(root: &Path, forum: Option<&str>) -> Result<Vec<String>> {
    if let Some(f) = forum {
        return Ok(vec![f.to_string()]);
    }
    let mut set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    if let Ok(wl) = rutracker_mirror::watchlist::load(root) {
        for e in wl.forums {
            set.insert(e.forum_id);
        }
    }
    let forums_dir = root.join("forums");
    if forums_dir.is_dir() {
        for entry in fs::read_dir(&forums_dir)
            .with_context(|| format!("reading {}", forums_dir.display()))?
        {
            let entry = entry?;
            if entry.path().is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    set.insert(name.to_string());
                }
            }
        }
    }
    Ok(set.into_iter().collect())
}

fn open_db(root: &Path) -> Result<Connection> {
    // ensure_schema is called inside Mirror::open; use it so the ranker tables
    // exist even if this CLI process is the first to touch the DB on a fresh
    // mirror.
    let m = rutracker_mirror::Mirror::open(root, None)
        .with_context(|| format!("opening mirror at {}", root.display()))?;
    drop(m); // we only needed the ensure_schema side-effect; reopen fresh
    let db_path = root.join("state.db");
    let conn =
        Connection::open(&db_path).with_context(|| format!("opening {}", db_path.display()))?;
    Ok(conn)
}

/// Read and parse a topic JSON from `forums/<fid>/topics/<tid>.json`.
fn load_topic_file(root: &Path, forum_id: &str, topic_id: &str) -> Result<TopicFile> {
    let path = root
        .join("forums")
        .join(forum_id)
        .join("topics")
        .join(format!("{topic_id}.json"));
    let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let tf: TopicFile =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    Ok(tf)
}

// ---------- run_rank_match ----------

#[derive(Debug, Serialize)]
pub struct MatchReport {
    pub forums: Vec<String>,
    pub topics_seen: u32,
    pub topics_inserted_or_updated: u32,
    pub parse_failures: u32,
}

/// Walk each forum's topic JSONs, parse titles, and upsert into
/// `film_index` + `film_topic`. Idempotent: running twice with unchanged
/// inputs produces the same row count (no duplicates).
pub async fn run_rank_match(cfg: &CliConfig, args: &RankMatchArgs) -> Result<String> {
    let root = mirror_root_for(args.root.as_ref());
    let forum_ids = forum_ids_for(&root, args.forum.as_deref())?;
    let mut conn = open_db(&root)?;

    let mut topics_seen: u32 = 0;
    let mut upserts: u32 = 0;
    let mut parse_failures: u32 = 0;

    let tx = conn.transaction()?;
    for forum_id in &forum_ids {
        let topics_dir = root.join("forums").join(forum_id).join("topics");
        if !topics_dir.is_dir() {
            continue;
        }
        let mut entries: Vec<PathBuf> = fs::read_dir(&topics_dir)?
            .filter_map(|r| r.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
            .collect();
        entries.sort();

        for path in entries {
            topics_seen += 1;
            let Some(tid) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            let bytes = match fs::read(&path) {
                Ok(b) => b,
                Err(e) => {
                    append_parse_failure(&root, forum_id, &tid, "<io error>", &e.to_string());
                    parse_failures += 1;
                    continue;
                }
            };
            let tf: TopicFile = match serde_json::from_slice(&bytes) {
                Ok(v) => v,
                Err(e) => {
                    append_parse_failure(&root, forum_id, &tid, "<json error>", &e.to_string());
                    parse_failures += 1;
                    continue;
                }
            };
            let parsed = match parse_title(&tf.title) {
                Ok(p) => p,
                Err(e) => {
                    append_parse_failure(&root, forum_id, &tid, &tf.title, &e.to_string());
                    parse_failures += 1;
                    continue;
                }
            };
            let fid = film_id(&film_key(&parsed));
            let now = now_rfc3339();

            // Upsert film_index: insert or update last_seen + metadata fields.
            tx.execute(
                "INSERT INTO film_index (film_id, title_ru, title_en, title_alt, year, director, first_seen, last_seen) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7) \
                 ON CONFLICT(film_id) DO UPDATE SET \
                   title_ru = excluded.title_ru, \
                   title_en = COALESCE(excluded.title_en, film_index.title_en), \
                   title_alt = COALESCE(excluded.title_alt, film_index.title_alt), \
                   year = COALESCE(excluded.year, film_index.year), \
                   director = COALESCE(excluded.director, film_index.director), \
                   last_seen = excluded.last_seen",
                params![
                    fid,
                    parsed.title_ru,
                    parsed.title_en,
                    parsed.title_alt,
                    parsed.year.map(|y| y as i64),
                    parsed.director,
                    now,
                ],
            )?;

            let md = RipMetadata::from_topic_file(&tf, &parsed);
            // Also pull seeds/leeches/downloads/size directly from the topic
            // file (Phase R2 added those columns to TopicFile itself).
            let seeds = tf.seeds.map(|v| v as i64).or(md.seeds.map(|v| v as i64));
            let leeches = tf
                .leeches
                .map(|v| v as i64)
                .or(md.leeches.map(|v| v as i64));
            let downloads = tf
                .downloads
                .map(|v| v as i64)
                .or(md.downloads.map(|v| v as i64));
            let size_bytes = tf
                .size_bytes
                .map(|v| v as i64)
                .or(md.size_bytes.map(|v| v as i64));

            tx.execute(
                "INSERT INTO film_topic (film_id, topic_id, forum_id, seeds, leeches, downloads, size_bytes, format_tag, fetched_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
                 ON CONFLICT(film_id, topic_id) DO UPDATE SET \
                   forum_id = excluded.forum_id, \
                   seeds = excluded.seeds, \
                   leeches = excluded.leeches, \
                   downloads = excluded.downloads, \
                   size_bytes = excluded.size_bytes, \
                   format_tag = excluded.format_tag, \
                   fetched_at = excluded.fetched_at",
                params![
                    fid,
                    tid,
                    forum_id,
                    seeds,
                    leeches,
                    downloads,
                    size_bytes,
                    md.format_tag,
                    md.fetched_at,
                ],
            )?;
            upserts += 1;
        }
    }
    tx.commit()?;

    let payload = MatchReport {
        forums: forum_ids.clone(),
        topics_seen,
        topics_inserted_or_updated: upserts,
        parse_failures,
    };
    let text = format!(
        "rank match: forums={:?} topics_seen={} upserts={} parse_failures={}\n",
        forum_ids, topics_seen, upserts, parse_failures
    );
    let out = render(cfg, &payload, &text)?;
    emit(cfg, &out).await?;
    Ok(out)
}

// ---------- run_rank_scan_prepare ----------

pub async fn run_rank_scan_prepare(cfg: &CliConfig, args: &RankScanPrepareArgs) -> Result<String> {
    let root = mirror_root_for(args.root.as_ref());
    let sha = agent_sha_current();
    let opts = ScanPrepareOpts {
        max_payload_bytes: args.max_payload_bytes.unwrap_or(8192),
    };
    let report = scan_prepare(&root, &args.forum, &sha, opts)?;
    let payload = json!({
        "forum_id": args.forum,
        "agent_sha": sha,
        "queued": report.queued,
        "skipped_cached": report.skipped_cached,
        "total": report.total,
    });
    let text = format!(
        "rank scan-prepare forum={} queued={} skipped_cached={} total={}\n",
        args.forum, report.queued, report.skipped_cached, report.total,
    );
    let out = render(cfg, &payload, &text)?;
    emit(cfg, &out).await?;
    Ok(out)
}

// ---------- run_rank_aggregate ----------

#[derive(Debug, Serialize)]
pub struct AggregateReport {
    pub films_scored: u32,
    pub topics_missing_scan: u32,
    pub forums: Vec<String>,
    pub top_film_example: Option<serde_json::Value>,
}

/// Inner state of one film while aggregating.
struct FilmBucket {
    canonical: ParsedTitle,
    /// (topic_id, parsed title, metadata, analysis?) per topic.
    rows: Vec<FilmRow>,
}

struct FilmRow {
    topic_id: String,
    parsed: ParsedTitle,
    metadata: RipMetadata,
    analysis: Option<TopicAnalysis>,
    seeds: u32,
    has_scan: bool,
}

pub async fn run_rank_aggregate(cfg: &CliConfig, args: &RankAggregateArgs) -> Result<String> {
    let root = mirror_root_for(args.root.as_ref());
    let forum_ids = forum_ids_for(&root, args.forum.as_deref())?;
    let mut conn = open_db(&root)?;

    // Collect all (film_id, topic_id, forum_id) rows where forum_id ∈ forum_ids.
    // When no forum filter is given, include everything.
    let mut buckets: BTreeMap<String, FilmBucket> = BTreeMap::new();
    let mut topics_missing_scan: u32 = 0;

    let rows: Vec<(String, String, String)> = {
        let placeholders = if forum_ids.is_empty() {
            String::new()
        } else {
            let ph = vec!["?"; forum_ids.len()].join(",");
            format!(" WHERE forum_id IN ({ph})")
        };
        let sql = format!("SELECT film_id, topic_id, forum_id FROM film_topic{placeholders}");
        let params_owned: Vec<&dyn rusqlite::ToSql> = forum_ids
            .iter()
            .map(|s| s as &dyn rusqlite::ToSql)
            .collect();
        let mut stmt = conn.prepare(&sql)?;
        let iter = stmt.query_map(rusqlite::params_from_iter(params_owned), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        iter.collect::<std::result::Result<Vec<_>, _>>()?
    };

    for (fid, tid, forum_id) in rows {
        let tf = match load_topic_file(&root, &forum_id, &tid) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let parsed = match parse_title(&tf.title) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let md = RipMetadata::from_topic_file(&tf, &parsed);
        let scan_path = root
            .join("forums")
            .join(&forum_id)
            .join("scans")
            .join(format!("{tid}.scan.json"));
        let (analysis, has_scan) = match read_scan(&scan_path) {
            Ok(sf) => (Some(sf.analysis), true),
            Err(_) => {
                topics_missing_scan += 1;
                (None, false)
            }
        };
        let seeds = tf.seeds.unwrap_or(0);
        let row = FilmRow {
            topic_id: tid,
            parsed: parsed.clone(),
            metadata: md,
            analysis,
            seeds,
            has_scan,
        };
        buckets
            .entry(fid)
            .or_insert_with(|| FilmBucket {
                canonical: parsed,
                rows: Vec::new(),
            })
            .rows
            .push(row);
    }

    // Pick canonical: highest-seeds row; fall back to first.
    for bucket in buckets.values_mut() {
        if let Some(best) = bucket.rows.iter().max_by_key(|r| r.seeds) {
            bucket.canonical = best.parsed.clone();
        }
    }

    let tx = conn.transaction()?;
    let mut films_scored: u32 = 0;
    let mut top_example: Option<serde_json::Value> = None;
    let mut top_example_score: f32 = f32::MIN;

    let now = chrono::Utc::now();
    for (fid, bucket) in &buckets {
        let film_topics: Vec<FilmTopic> = bucket
            .rows
            .iter()
            .map(|r| FilmTopic {
                topic_id: r.topic_id.as_str(),
                analysis: r.analysis.as_ref(),
            })
            .collect();
        let fs = aggregate_film(fid, &bucket.canonical, &film_topics);

        // Upsert film_score.
        tx.execute(
            "INSERT INTO film_score (film_id, score, confidence, topic_count_with_analysis, topic_count_total, total_substantive_comments, top_themes_positive, top_themes_negative, has_red_flags, scored_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
             ON CONFLICT(film_id) DO UPDATE SET \
               score = excluded.score, \
               confidence = excluded.confidence, \
               topic_count_with_analysis = excluded.topic_count_with_analysis, \
               topic_count_total = excluded.topic_count_total, \
               total_substantive_comments = excluded.total_substantive_comments, \
               top_themes_positive = excluded.top_themes_positive, \
               top_themes_negative = excluded.top_themes_negative, \
               has_red_flags = excluded.has_red_flags, \
               scored_at = excluded.scored_at",
            params![
                fs.film_id,
                fs.score as f64,
                fs.confidence as f64,
                fs.topic_count_with_analysis as i64,
                fs.topic_count_total as i64,
                fs.total_substantive_comments as i64,
                serde_json::to_string(&fs.top_themes_positive)?,
                serde_json::to_string(&fs.top_themes_negative)?,
                if fs.has_red_flags { 1_i64 } else { 0_i64 },
                fs.scored_at,
            ],
        )?;
        films_scored += 1;

        // Compute best rip for logging / example surfacing.
        let candidates: Vec<RipCandidate> = bucket
            .rows
            .iter()
            .map(|r| RipCandidate {
                topic_id: r.topic_id.as_str(),
                metadata: &r.metadata,
                analysis: r.analysis.as_ref(),
            })
            .collect();
        let ranked = rank_rips(&candidates, now);
        let best_topic_id = ranked.first().map(|r| r.topic_id.clone());
        let best_rip_score = ranked.first().map(|r| r.score).unwrap_or(0.0);

        if fs.score > top_example_score {
            top_example_score = fs.score;
            top_example = Some(json!({
                "film_id": fs.film_id,
                "title_ru": fs.canonical_title_ru,
                "title_en": fs.canonical_title_en,
                "year": fs.year,
                "score": fs.score,
                "confidence": fs.confidence,
                "topic_count_with_analysis": fs.topic_count_with_analysis,
                "topic_count_total": fs.topic_count_total,
                "best_topic_id": best_topic_id,
                "best_rip_score": best_rip_score,
            }));
        }
    }
    tx.commit()?;

    // Per-forum missing-scan diagnostic (plan §6.2 copy).
    let mut warnings: Vec<String> = Vec::new();
    if !forum_ids.is_empty() {
        for fid in &forum_ids {
            let missing = buckets
                .values()
                .flat_map(|b| b.rows.iter())
                .filter(|r| {
                    // only rows whose topic file resides in this forum are
                    // counted against it.
                    !r.has_scan
                        && root
                            .join("forums")
                            .join(fid)
                            .join("topics")
                            .join(format!("{}.json", r.topic_id))
                            .exists()
                })
                .count();
            if missing > 0 {
                warnings.push(format!(
                    "{} topics in forum {} have no scan.json — run 'rutracker rank scan-prepare' then '/rank-scan-run' in Claude Code",
                    missing, fid
                ));
            }
        }
    } else if topics_missing_scan > 0 {
        warnings.push(format!(
            "{} topics are missing scan.json — run 'rutracker rank scan-prepare' then '/rank-scan-run' in Claude Code",
            topics_missing_scan
        ));
    }
    for w in &warnings {
        eprintln!("{w}");
        if cfg.emit_stdout {
            println!("{w}");
        }
    }

    let report = AggregateReport {
        films_scored,
        topics_missing_scan,
        forums: forum_ids,
        top_film_example: top_example,
    };
    let text = format!(
        "rank aggregate: films_scored={} topics_missing_scan={}\n",
        films_scored, topics_missing_scan,
    );
    let out = render(cfg, &report, &text)?;
    emit(cfg, &out).await?;
    Ok(out)
}

// ---------- run_rank_list ----------

#[derive(Debug, Serialize)]
pub struct ListEntry {
    pub film_id: String,
    pub title_ru: String,
    pub title_en: Option<String>,
    pub year: Option<u16>,
    pub director: Option<String>,
    pub score: f32,
    pub confidence: f32,
    pub topic_count_with_analysis: u32,
    pub topic_count_total: u32,
}

pub async fn run_rank_list(cfg: &CliConfig, args: &RankListArgs) -> Result<String> {
    let root = mirror_root_for(args.root.as_ref());
    let conn = open_db(&root)?;

    let min_score = args.min_score.unwrap_or(f32::MIN);
    let top = args.top.unwrap_or(u32::MAX) as i64;

    // Join film_index so we get titles/year/director. film_score has the score
    // side. Filter by forum by also joining film_topic on match — we do this
    // only when a forum filter is given to keep the default path cheap.
    let rows: Vec<ListEntry> = if let Some(fid_filter) = &args.forum {
        let mut stmt = conn.prepare(
            "SELECT DISTINCT fs.film_id, fi.title_ru, fi.title_en, fi.year, fi.director, \
                    fs.score, fs.confidence, fs.topic_count_with_analysis, fs.topic_count_total \
             FROM film_score fs \
             JOIN film_index fi ON fi.film_id = fs.film_id \
             JOIN film_topic ft ON ft.film_id = fs.film_id \
             WHERE fs.score >= ?1 AND ft.forum_id = ?2 \
             ORDER BY fs.score DESC \
             LIMIT ?3",
        )?;
        let iter = stmt.query_map(params![min_score as f64, fid_filter, top], map_list_row)?;
        iter.collect::<std::result::Result<Vec<_>, _>>()?
    } else {
        let mut stmt = conn.prepare(
            "SELECT fs.film_id, fi.title_ru, fi.title_en, fi.year, fi.director, \
                    fs.score, fs.confidence, fs.topic_count_with_analysis, fs.topic_count_total \
             FROM film_score fs \
             JOIN film_index fi ON fi.film_id = fs.film_id \
             WHERE fs.score >= ?1 \
             ORDER BY fs.score DESC \
             LIMIT ?2",
        )?;
        let iter = stmt.query_map(params![min_score as f64, top], map_list_row)?;
        iter.collect::<std::result::Result<Vec<_>, _>>()?
    };

    let text = format_list_text(&rows);
    let payload = serde_json::json!({ "films": rows });
    let out = render(cfg, &payload, &text)?;
    emit(cfg, &out).await?;
    Ok(out)
}

fn map_list_row(r: &rusqlite::Row) -> rusqlite::Result<ListEntry> {
    Ok(ListEntry {
        film_id: r.get(0)?,
        title_ru: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
        title_en: r.get(2)?,
        year: r.get::<_, Option<i64>>(3)?.map(|v| v as u16),
        director: r.get(4)?,
        score: r.get::<_, f64>(5)? as f32,
        confidence: r.get::<_, f64>(6)? as f32,
        topic_count_with_analysis: r.get::<_, i64>(7)? as u32,
        topic_count_total: r.get::<_, i64>(8)? as u32,
    })
}

fn format_list_text(rows: &[ListEntry]) -> String {
    if rows.is_empty() {
        return "(no films ranked)\n".to_string();
    }
    let mut s = String::new();
    s.push_str("score  conf  n/tot  year  title\n");
    for r in rows {
        s.push_str(&format!(
            "{:5.2}  {:4.2}  {:>2}/{:<3}  {:>4}  {}\n",
            r.score,
            r.confidence,
            r.topic_count_with_analysis,
            r.topic_count_total,
            r.year
                .map(|v| v.to_string())
                .unwrap_or_else(|| "    ".to_string()),
            r.title_ru,
        ));
    }
    s
}

// ---------- run_rank_show ----------

/// Row shape pulled from `film_score` for `run_rank_show`. Kept as a named
/// struct (rather than a long tuple) to satisfy `clippy::type_complexity`.
struct ScoreRow {
    score: f64,
    confidence: f64,
    n_with: i64,
    n_total: i64,
    substantive: i64,
    themes_pos_json: String,
    themes_neg_json: String,
    red_flags_flag: i64,
}

pub async fn run_rank_show(cfg: &CliConfig, args: &RankShowArgs) -> Result<String> {
    let root = mirror_root_for(args.root.as_ref());
    let conn = open_db(&root)?;

    // Resolve film_id from the query: try exact match first, then substring.
    let film_id_resolved: String = {
        let exact: Option<String> = conn
            .query_row(
                "SELECT film_id FROM film_index WHERE film_id = ?1",
                params![args.query],
                |r| r.get::<_, String>(0),
            )
            .ok();
        if let Some(f) = exact {
            f
        } else {
            let like = format!("%{}%", args.query);
            conn.query_row(
                "SELECT film_id FROM film_index \
                 WHERE title_ru LIKE ?1 OR title_en LIKE ?1 \
                 ORDER BY last_seen DESC LIMIT 1",
                params![like],
                |r| r.get::<_, String>(0),
            )
            .map_err(|_| anyhow!("no film matches {}", args.query))?
        }
    };

    // film_index.*
    let (title_ru, title_en, title_alt, year, director): (
        String,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<String>,
    ) = conn.query_row(
        "SELECT title_ru, title_en, title_alt, year, director FROM film_index WHERE film_id = ?1",
        params![film_id_resolved],
        |r| {
            Ok((
                r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
            ))
        },
    )?;

    // film_score.*
    let score_row: Option<ScoreRow> = conn
        .query_row(
            "SELECT score, confidence, topic_count_with_analysis, topic_count_total, \
                    total_substantive_comments, top_themes_positive, top_themes_negative, has_red_flags \
             FROM film_score WHERE film_id = ?1",
            params![film_id_resolved],
            |r| {
                Ok(ScoreRow {
                    score: r.get(0)?,
                    confidence: r.get(1)?,
                    n_with: r.get(2)?,
                    n_total: r.get(3)?,
                    substantive: r.get(4)?,
                    themes_pos_json: r.get::<_, Option<String>>(5)?.unwrap_or_default(),
                    themes_neg_json: r.get::<_, Option<String>>(6)?.unwrap_or_default(),
                    red_flags_flag: r.get(7)?,
                })
            },
        )
        .ok();

    // All topics belonging to this film. Read their topic files + scans to
    // rank rips.
    let topic_rows: Vec<(String, String)> = {
        let mut stmt =
            conn.prepare("SELECT topic_id, forum_id FROM film_topic WHERE film_id = ?1")?;
        let iter = stmt.query_map(params![film_id_resolved], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        iter.collect::<std::result::Result<Vec<_>, _>>()?
    };

    #[derive(Serialize)]
    struct RipOut {
        topic_id: String,
        forum_id: String,
        format_tag: Option<String>,
        seeds: Option<u32>,
        score: f32,
        tech_quality: f32,
        format_preference: f32,
        audio_preference: f32,
        health: f32,
        recency: f32,
        has_scan: bool,
    }

    let mut rows: Vec<(FilmRow, String)> = Vec::new();
    for (tid, forum_id) in topic_rows {
        let Ok(tf) = load_topic_file(&root, &forum_id, &tid) else {
            continue;
        };
        let Ok(parsed) = parse_title(&tf.title) else {
            continue;
        };
        let md = RipMetadata::from_topic_file(&tf, &parsed);
        let scan_path = root
            .join("forums")
            .join(&forum_id)
            .join("scans")
            .join(format!("{tid}.scan.json"));
        let (analysis, has_scan) = match read_scan(&scan_path) {
            Ok(sf) => (Some(sf.analysis), true),
            Err(_) => (None, false),
        };
        let seeds = tf.seeds.unwrap_or(0);
        rows.push((
            FilmRow {
                topic_id: tid,
                parsed,
                metadata: md,
                analysis,
                seeds,
                has_scan,
            },
            forum_id,
        ));
    }
    let candidates: Vec<RipCandidate> = rows
        .iter()
        .map(|(r, _)| RipCandidate {
            topic_id: r.topic_id.as_str(),
            metadata: &r.metadata,
            analysis: r.analysis.as_ref(),
        })
        .collect();
    let ranked = rank_rips(&candidates, chrono::Utc::now());
    let by_id: std::collections::HashMap<String, &(FilmRow, String)> = rows
        .iter()
        .map(|row| (row.0.topic_id.clone(), row))
        .collect();
    let rips: Vec<RipOut> = ranked
        .iter()
        .filter_map(|r| {
            let (filmrow, forum_id) = by_id.get(&r.topic_id)?;
            Some(RipOut {
                topic_id: r.topic_id.clone(),
                forum_id: forum_id.clone(),
                format_tag: filmrow.metadata.format_tag.clone(),
                seeds: filmrow.metadata.seeds,
                score: r.score,
                tech_quality: r.rationale.tech_quality,
                format_preference: r.rationale.format_preference,
                audio_preference: r.rationale.audio_preference,
                health: r.rationale.health,
                recency: r.rationale.recency,
                has_scan: filmrow.has_scan,
            })
        })
        .collect();

    let payload = json!({
        "film_id": film_id_resolved,
        "title_ru": title_ru,
        "title_en": title_en,
        "title_alt": title_alt,
        "year": year,
        "director": director,
        "score": score_row.as_ref().map(|s| s.score),
        "confidence": score_row.as_ref().map(|s| s.confidence),
        "topic_count_with_analysis": score_row.as_ref().map(|s| s.n_with),
        "topic_count_total": score_row.as_ref().map(|s| s.n_total),
        "total_substantive_comments": score_row.as_ref().map(|s| s.substantive),
        "top_themes_positive": score_row.as_ref()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s.themes_pos_json).ok())
            .unwrap_or(serde_json::Value::Array(vec![])),
        "top_themes_negative": score_row.as_ref()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s.themes_neg_json).ok())
            .unwrap_or(serde_json::Value::Array(vec![])),
        "has_red_flags": score_row.as_ref().map(|s| s.red_flags_flag != 0).unwrap_or(false),
        "rips": rips,
    });

    let mut text = String::new();
    text.push_str(&format!("{} (film_id {})\n", title_ru, film_id_resolved));
    if let Some(en) = &title_en {
        text.push_str(&format!("  en: {en}\n"));
    }
    if let Some(y) = year {
        text.push_str(&format!("  year: {y}\n"));
    }
    if let Some(d) = &director {
        text.push_str(&format!("  director: {d}\n"));
    }
    if let Some(s) = &score_row {
        text.push_str(&format!(
            "  score: {:.2} ± {:.2}  ({}/{} topics)  red_flags={}\n",
            s.score,
            s.confidence,
            s.n_with,
            s.n_total,
            if s.red_flags_flag != 0 { "yes" } else { "no" },
        ));
    }
    text.push_str("  rips (best first):\n");
    for r in &payload["rips"].as_array().cloned().unwrap_or_default() {
        text.push_str(&format!(
            "    {} [{}] seeds={:?} score={:.3} fmt={:.2} aud={:.2} tech={:.2} health={:.2} recency={:.2}\n",
            r["topic_id"].as_str().unwrap_or("?"),
            r["format_tag"].as_str().unwrap_or(""),
            r["seeds"].as_u64(),
            r["score"].as_f64().unwrap_or(0.0),
            r["format_preference"].as_f64().unwrap_or(0.0),
            r["audio_preference"].as_f64().unwrap_or(0.0),
            r["tech_quality"].as_f64().unwrap_or(0.0),
            r["health"].as_f64().unwrap_or(0.0),
            r["recency"].as_f64().unwrap_or(0.0),
        ));
    }

    let out = render(cfg, &payload, &text)?;
    emit(cfg, &out).await?;
    Ok(out)
}

// ---------- run_rank_parse_failures ----------

pub async fn run_rank_parse_failures(
    cfg: &CliConfig,
    args: &RankParseFailuresArgs,
) -> Result<String> {
    let root = mirror_root_for(args.root.as_ref());
    let path = parse_failures_log_path(&root);
    let contents = fs::read_to_string(&path).unwrap_or_default();
    let lines: Vec<&str> = contents.lines().collect();
    let payload = json!({
        "log_path": path.display().to_string(),
        "count": lines.len(),
        "lines": lines,
    });
    let text = if lines.is_empty() {
        format!("(no parse failures recorded at {})\n", path.display())
    } else {
        let mut s = format!("{} parse failures in {}:\n", lines.len(), path.display());
        for l in &lines {
            s.push_str(l);
            s.push('\n');
        }
        s
    };
    let out = render(cfg, &payload, &text)?;
    emit(cfg, &out).await?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CliConfig;
    use rutracker_mirror::topic_io::{Post, TopicFile};
    use rutracker_mirror::Mirror;
    use std::collections::HashMap;

    fn tempdir_unique(_suffix: &str) -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }

    fn init_mirror(root: &Path) {
        Mirror::init(root).unwrap();
    }

    fn write_topic_json(root: &Path, forum_id: &str, topic_id: &str, title: &str, seeds: u32) {
        let topics_dir = root.join("forums").join(forum_id).join("topics");
        std::fs::create_dir_all(&topics_dir).unwrap();
        let tf = TopicFile {
            schema_version: 1,
            topic_id: topic_id.to_string(),
            forum_id: forum_id.to_string(),
            title: title.to_string(),
            fetched_at: "2026-04-18T12:00:00+00:00".into(),
            last_post_id: 100,
            last_post_at: "2026-04-18T12:00:00+00:00".into(),
            opening_post: Post::default(),
            comments: Vec::new(),
            metadata: serde_json::Value::Null,
            size_bytes: Some(2_000_000_000),
            seeds: Some(seeds),
            leeches: Some(1),
            downloads: Some(100),
        };
        rutracker_mirror::topic_io::write_json_atomic(
            &topics_dir.join(format!("{topic_id}.json")),
            &tf,
        )
        .unwrap();
    }

    fn write_scan_json(
        root: &Path,
        forum_id: &str,
        topic_id: &str,
        sentiment: f32,
        conf: f32,
        substantive: u32,
    ) {
        let scans_dir = root.join("forums").join(forum_id).join("scans");
        std::fs::create_dir_all(&scans_dir).unwrap();
        let sf = serde_json::json!({
            "schema": 1,
            "agent_sha": "fixtureshafixture",
            "scanned_at": "2026-04-18T12:00:00+00:00",
            "topic_id": topic_id,
            "last_post_id": "100",
            "analysis": {
                "sentiment_score": sentiment,
                "confidence": conf,
                "themes_positive": [],
                "themes_negative": [],
                "tech_complaints": {"audio": false, "video": false, "subtitles": false, "dubbing": false, "sync": false},
                "tech_praise":     {"audio": false, "video": false, "subtitles": false, "dubbing": false, "sync": false},
                "substantive_count": substantive,
                "red_flags": [],
                "relevance": 1.0
            }
        });
        std::fs::write(
            scans_dir.join(format!("{topic_id}.scan.json")),
            serde_json::to_vec_pretty(&sf).unwrap(),
        )
        .unwrap();
    }

    fn cfg_for_test() -> CliConfig {
        CliConfig {
            base_url: "https://example.test/forum/".into(),
            format: crate::OutputFormat::Json,
            out: None,
            cookies: HashMap::new(),
            emit_stdout: false,
        }
    }

    #[tokio::test]
    async fn test_match_is_incremental() {
        let _td = tempdir_unique("match-incremental");
        let root = _td.path().to_path_buf();
        init_mirror(&root);
        // Two rips of the same film.
        let title_a = "Фильм А / Film A (Director X) [2025, США, драма, WEB-DLRip] Dub";
        write_topic_json(&root, "252", "1001", title_a, 50);
        write_topic_json(&root, "252", "1002", title_a, 10);

        let cfg = cfg_for_test();
        run_rank_match(
            &cfg,
            &RankMatchArgs {
                forum: Some("252".into()),
                root: Some(root.clone()),
            },
        )
        .await
        .unwrap();

        let conn = Connection::open(root.join("state.db")).unwrap();
        let films_1: i64 = conn
            .query_row("SELECT COUNT(*) FROM film_index", [], |r| r.get(0))
            .unwrap();
        let topics_1: i64 = conn
            .query_row("SELECT COUNT(*) FROM film_topic", [], |r| r.get(0))
            .unwrap();
        drop(conn);

        // Second run — must be idempotent.
        run_rank_match(
            &cfg,
            &RankMatchArgs {
                forum: Some("252".into()),
                root: Some(root.clone()),
            },
        )
        .await
        .unwrap();

        let conn = Connection::open(root.join("state.db")).unwrap();
        let films_2: i64 = conn
            .query_row("SELECT COUNT(*) FROM film_index", [], |r| r.get(0))
            .unwrap();
        let topics_2: i64 = conn
            .query_row("SELECT COUNT(*) FROM film_topic", [], |r| r.get(0))
            .unwrap();
        assert_eq!(films_1, films_2, "second match must not duplicate films");
        assert_eq!(topics_1, topics_2, "second match must not duplicate topics");
        assert_eq!(films_1, 1);
        assert_eq!(topics_1, 2);
    }

    #[tokio::test]
    async fn test_aggregate_warns_about_missing_scans() {
        let _td = tempdir_unique("agg-missing");
        let root = _td.path().to_path_buf();
        init_mirror(&root);
        let title = "Фильм Б / Film B (Director Y) [2025, США, драма, WEB-DLRip] Dub";
        write_topic_json(&root, "252", "2001", title, 50);
        write_topic_json(&root, "252", "2002", title, 30);
        write_topic_json(&root, "252", "2003", title, 20);
        // Only 1 scan out of 3.
        write_scan_json(&root, "252", "2001", 8.0, 0.9, 10);

        let cfg = cfg_for_test();
        run_rank_match(
            &cfg,
            &RankMatchArgs {
                forum: Some("252".into()),
                root: Some(root.clone()),
            },
        )
        .await
        .unwrap();
        let out = run_rank_aggregate(
            &cfg,
            &RankAggregateArgs {
                forum: Some("252".into()),
                root: Some(root.clone()),
            },
        )
        .await
        .unwrap();
        // The warning is emitted to stderr, which isn't captured here. The JSON
        // report carries topics_missing_scan which the user can inspect.
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["topics_missing_scan"].as_u64().unwrap(), 2);
        assert_eq!(v["films_scored"].as_u64().unwrap(), 1);
    }

    #[tokio::test]
    async fn test_list_respects_min_score() {
        let _td = tempdir_unique("list-minscore");
        let root = _td.path().to_path_buf();
        init_mirror(&root);
        // Seed film_score directly with 3 films at 4.0 / 7.5 / 8.8.
        {
            let m = Mirror::open(&root, None).unwrap();
            drop(m);
            let conn = Connection::open(root.join("state.db")).unwrap();
            for (fid, title, score) in [
                ("aaaa000000000001", "Low Film", 4.0_f64),
                ("aaaa000000000002", "Mid Film", 7.5_f64),
                ("aaaa000000000003", "High Film", 8.8_f64),
            ] {
                conn.execute(
                    "INSERT INTO film_index (film_id, title_ru, first_seen, last_seen) VALUES (?1, ?2, ?3, ?3)",
                    params![fid, title, "2026-04-18T12:00:00+00:00"],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO film_score (film_id, score, confidence, topic_count_with_analysis, topic_count_total, total_substantive_comments, top_themes_positive, top_themes_negative, has_red_flags, scored_at) \
                     VALUES (?1, ?2, 0.8, 1, 1, 5, '[]', '[]', 0, ?3)",
                    params![fid, score, "2026-04-18T12:00:00+00:00"],
                )
                .unwrap();
            }
        }

        let cfg = cfg_for_test();
        let out = run_rank_list(
            &cfg,
            &RankListArgs {
                forum: None,
                min_score: Some(7.0),
                top: None,
                root: Some(root.clone()),
            },
        )
        .await
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        let films = v["films"].as_array().unwrap();
        assert_eq!(films.len(), 2, "only scores >= 7.0 should be returned");
        // Descending order.
        assert!(films[0]["score"].as_f64().unwrap() >= films[1]["score"].as_f64().unwrap());
        assert_eq!(films[0]["title_ru"].as_str().unwrap(), "High Film");
        assert_eq!(films[1]["title_ru"].as_str().unwrap(), "Mid Film");
    }

    // ---------- US-008 additional rank coverage ----------

    #[tokio::test]
    async fn test_parse_failures_empty_when_no_log() {
        let _td = tempdir_unique("pf-empty");
        let root = _td.path().to_path_buf();
        init_mirror(&root);

        let cfg = cfg_for_test();
        let out = run_rank_parse_failures(
            &cfg,
            &RankParseFailuresArgs {
                root: Some(root.clone()),
            },
        )
        .await
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["count"].as_u64().unwrap(), 0);
        assert!(v["log_path"]
            .as_str()
            .unwrap()
            .ends_with("rank-parse-failures.log"));
    }

    #[tokio::test]
    async fn test_parse_failures_with_seeded_log() {
        let _td = tempdir_unique("pf-seeded");
        let root = _td.path().to_path_buf();
        init_mirror(&root);
        let log = root.join("logs").join("rank-parse-failures.log");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, "ts\tforum=1\ttopic=2\terr=bad\ttitle=X\n").unwrap();

        let cfg = cfg_for_test();
        let out = run_rank_parse_failures(
            &cfg,
            &RankParseFailuresArgs {
                root: Some(root.clone()),
            },
        )
        .await
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["count"].as_u64().unwrap(), 1);
    }

    #[tokio::test]
    async fn test_parse_failures_text_mode_no_log() {
        let _td = tempdir_unique("pf-text-none");
        let root = _td.path().to_path_buf();
        init_mirror(&root);
        let mut cfg = cfg_for_test();
        cfg.format = crate::OutputFormat::Text;
        let out = run_rank_parse_failures(
            &cfg,
            &RankParseFailuresArgs {
                root: Some(root.clone()),
            },
        )
        .await
        .unwrap();
        assert!(
            out.contains("(no parse failures recorded"),
            "expected empty hint, got: {out}"
        );
    }

    #[tokio::test]
    async fn test_parse_failures_text_mode_with_seeded_log() {
        let _td = tempdir_unique("pf-text-seeded");
        let root = _td.path().to_path_buf();
        init_mirror(&root);
        let log = root.join("logs").join("rank-parse-failures.log");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, "ts\tforum=1\ttopic=2\terr=bad\ttitle=X\n").unwrap();

        let mut cfg = cfg_for_test();
        cfg.format = crate::OutputFormat::Text;
        let out = run_rank_parse_failures(
            &cfg,
            &RankParseFailuresArgs {
                root: Some(root.clone()),
            },
        )
        .await
        .unwrap();
        assert!(out.starts_with("1 parse failures in "));
        assert!(out.contains("forum=1"));
    }

    #[tokio::test]
    async fn test_scan_prepare_emits_queue() {
        let _td = tempdir_unique("sp-emit");
        let root = _td.path().to_path_buf();
        init_mirror(&root);
        write_topic_json(
            &root,
            "252",
            "1001",
            "Фильм / Film (Dir) [2025, США, WEB-DLRip]",
            5,
        );

        let cfg = cfg_for_test();
        let out = run_rank_scan_prepare(
            &cfg,
            &RankScanPrepareArgs {
                forum: "252".into(),
                max_payload_bytes: Some(8192),
                root: Some(root.clone()),
            },
        )
        .await
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["queued"].as_u64().unwrap(), 1);
        assert_eq!(v["forum_id"].as_str().unwrap(), "252");
        assert!(
            root.join("forums/252/scan-queue.jsonl").exists(),
            "scan-queue.jsonl must be written"
        );
    }

    #[tokio::test]
    async fn test_rank_show_resolves_by_substring() {
        // Build a complete pipeline: topic -> rank match -> direct score seed -> show.
        let _td = tempdir_unique("show-by-substring");
        let root = _td.path().to_path_buf();
        init_mirror(&root);
        let title =
            "Интерстеллар / Interstellar (Christopher Nolan) [2014, США, sci-fi, WEB-DLRip] Dub";
        write_topic_json(&root, "252", "7001", title, 100);
        write_scan_json(&root, "252", "7001", 8.5, 0.9, 20);

        let cfg = cfg_for_test();
        run_rank_match(
            &cfg,
            &RankMatchArgs {
                forum: Some("252".into()),
                root: Some(root.clone()),
            },
        )
        .await
        .unwrap();
        run_rank_aggregate(
            &cfg,
            &RankAggregateArgs {
                forum: Some("252".into()),
                root: Some(root.clone()),
            },
        )
        .await
        .unwrap();

        // Search by a substring of the Russian title.
        let out = run_rank_show(
            &cfg,
            &RankShowArgs {
                query: "Интерстеллар".into(),
                root: Some(root.clone()),
            },
        )
        .await
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert!(v["title_ru"].as_str().unwrap().contains("Интерстеллар"));
        assert!(v["score"].is_number());
        assert_eq!(v["rips"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_rank_show_not_found_errors() {
        let _td = tempdir_unique("show-not-found");
        let root = _td.path().to_path_buf();
        init_mirror(&root);

        let cfg = cfg_for_test();
        let err = run_rank_show(
            &cfg,
            &RankShowArgs {
                query: "no-such-film".into(),
                root: Some(root.clone()),
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no film matches"));
    }

    #[tokio::test]
    async fn test_rank_show_text_mode_prints_table() {
        let _td = tempdir_unique("show-text");
        let root = _td.path().to_path_buf();
        init_mirror(&root);
        let title = "Дюна / Dune (Denis Villeneuve) [2021, США, sci-fi, WEB-DLRip] Dub";
        write_topic_json(&root, "252", "8001", title, 99);
        write_scan_json(&root, "252", "8001", 7.0, 0.6, 12);
        let mut cfg = cfg_for_test();
        run_rank_match(
            &cfg,
            &RankMatchArgs {
                forum: Some("252".into()),
                root: Some(root.clone()),
            },
        )
        .await
        .unwrap();
        run_rank_aggregate(
            &cfg,
            &RankAggregateArgs {
                forum: Some("252".into()),
                root: Some(root.clone()),
            },
        )
        .await
        .unwrap();
        cfg.format = crate::OutputFormat::Text;
        let out = run_rank_show(
            &cfg,
            &RankShowArgs {
                query: "Дюна".into(),
                root: Some(root.clone()),
            },
        )
        .await
        .unwrap();
        assert!(out.contains("rips (best first)"));
        assert!(out.contains("Дюна"));
    }

    #[tokio::test]
    async fn test_rank_list_empty_text_mode() {
        let _td = tempdir_unique("list-empty-text");
        let root = _td.path().to_path_buf();
        init_mirror(&root);

        let mut cfg = cfg_for_test();
        cfg.format = crate::OutputFormat::Text;
        let out = run_rank_list(
            &cfg,
            &RankListArgs {
                forum: None,
                min_score: None,
                top: None,
                root: Some(root.clone()),
            },
        )
        .await
        .unwrap();
        assert!(out.contains("(no films ranked)"));
    }

    #[tokio::test]
    async fn test_rank_list_with_forum_filter() {
        let _td = tempdir_unique("list-forum-filter");
        let root = _td.path().to_path_buf();
        init_mirror(&root);
        // Two films, one per forum; filter must return only matching forum.
        {
            let m = Mirror::open(&root, None).unwrap();
            drop(m);
            let conn = Connection::open(root.join("state.db")).unwrap();
            for (fid, title, score, forum) in [
                ("aaaa000000000111", "Film A", 8.0_f64, "252"),
                ("aaaa000000000222", "Film B", 9.0_f64, "251"),
            ] {
                conn.execute(
                    "INSERT INTO film_index (film_id, title_ru, first_seen, last_seen) VALUES (?1, ?2, ?3, ?3)",
                    params![fid, title, "2026-04-18T12:00:00+00:00"],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO film_score (film_id, score, confidence, topic_count_with_analysis, topic_count_total, total_substantive_comments, top_themes_positive, top_themes_negative, has_red_flags, scored_at) \
                     VALUES (?1, ?2, 0.7, 1, 1, 3, '[]', '[]', 0, ?3)",
                    params![fid, score, "2026-04-18T12:00:00+00:00"],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO film_topic (film_id, topic_id, forum_id, seeds, leeches, downloads, size_bytes, format_tag, fetched_at) \
                     VALUES (?1, ?2, ?3, 0, 0, 0, 0, 'WEB-DLRip', ?4)",
                    params![fid, "t-id", forum, "2026-04-18T12:00:00+00:00"],
                )
                .unwrap();
            }
        }
        let cfg = cfg_for_test();
        let out = run_rank_list(
            &cfg,
            &RankListArgs {
                forum: Some("252".into()),
                min_score: None,
                top: None,
                root: Some(root.clone()),
            },
        )
        .await
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        let films = v["films"].as_array().unwrap();
        assert_eq!(films.len(), 1);
        assert_eq!(films[0]["title_ru"].as_str().unwrap(), "Film A");
    }

    #[test]
    fn test_forum_ids_for_specific_forum_returns_single() {
        let _td = tempdir_unique("fids-specific");
        let ids = forum_ids_for(_td.path(), Some("252")).unwrap();
        assert_eq!(ids, vec!["252".to_string()]);
    }

    #[test]
    fn test_forum_ids_for_collects_disk_and_watchlist() {
        let _td = tempdir_unique("fids-collect");
        let root = _td.path().to_path_buf();
        init_mirror(&root);
        // Seed forum dir for 252.
        std::fs::create_dir_all(root.join("forums").join("252")).unwrap();
        // Seed watchlist with 251 via Structure + watchlist API.
        let structure = serde_json::json!({
            "schema_version": 1,
            "groups": [{
                "group_id": "1",
                "title": "G",
                "forums": [
                    {"forum_id": "251", "name": "F 251", "parent_id": null},
                ]
            }],
            "fetched_at": null
        });
        std::fs::write(
            root.join("structure.json"),
            serde_json::to_vec_pretty(&structure).unwrap(),
        )
        .unwrap();
        let s: rutracker_mirror::structure::Structure =
            serde_json::from_slice(&std::fs::read(root.join("structure.json")).unwrap()).unwrap();
        let mut wl = rutracker_mirror::watchlist::load(&root).unwrap();
        rutracker_mirror::watchlist::add(&mut wl, &s, "251").unwrap();
        rutracker_mirror::watchlist::save(&root, &wl).unwrap();

        let ids = forum_ids_for(&root, None).unwrap();
        assert!(ids.contains(&"252".into()), "must include on-disk 252");
        assert!(ids.contains(&"251".into()), "must include watchlisted 251");
        // Deterministic (sorted).
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
    }

    #[tokio::test]
    async fn test_match_records_parse_failures_for_bad_title_and_json() {
        let _td = tempdir_unique("match-parse-fail");
        let root = _td.path().to_path_buf();
        init_mirror(&root);
        let topics_dir = root.join("forums").join("252").join("topics");
        std::fs::create_dir_all(&topics_dir).unwrap();
        // Case 1: bad JSON.
        std::fs::write(topics_dir.join("5001.json"), b"{invalid").unwrap();
        // Case 2: valid JSON but unparseable title (no / separator, no year).
        let tf = rutracker_mirror::topic_io::TopicFile {
            schema_version: 1,
            topic_id: "5002".into(),
            forum_id: "252".into(),
            title: "Короткий".into(), // no year / format => parse_title errs
            fetched_at: "2026-04-18T12:00:00+00:00".into(),
            last_post_id: 100,
            last_post_at: "2026-04-18T12:00:00+00:00".into(),
            opening_post: rutracker_mirror::topic_io::Post::default(),
            comments: Vec::new(),
            metadata: serde_json::Value::Null,
            size_bytes: None,
            seeds: Some(1),
            leeches: None,
            downloads: None,
        };
        rutracker_mirror::topic_io::write_json_atomic(&topics_dir.join("5002.json"), &tf).unwrap();
        // Case 3: good row to prove we don't abort the whole forum.
        let title = "Фильм / Film (Dir) [2025, США, WEB-DLRip]";
        write_topic_json(&root, "252", "5003", title, 10);

        let cfg = cfg_for_test();
        let out = run_rank_match(
            &cfg,
            &RankMatchArgs {
                forum: Some("252".into()),
                root: Some(root.clone()),
            },
        )
        .await
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert!(v["parse_failures"].as_u64().unwrap() >= 2);
        assert_eq!(v["topics_seen"].as_u64().unwrap(), 3);
        let log = root.join("logs").join("rank-parse-failures.log");
        assert!(log.exists(), "parse-failures log must be written");
        let content = std::fs::read_to_string(&log).unwrap();
        assert!(content.contains("topic=5001"));
        assert!(content.contains("topic=5002"));
    }

    #[tokio::test]
    async fn test_aggregate_skips_topics_with_bad_json() {
        let _td = tempdir_unique("agg-bad-json");
        let root = _td.path().to_path_buf();
        init_mirror(&root);
        // Valid topic + scan first (to have at least one film).
        let title = "Ф / F (Dir) [2025, США, WEB-DLRip]";
        write_topic_json(&root, "252", "4001", title, 50);
        write_scan_json(&root, "252", "4001", 7.0, 0.8, 5);
        // Also insert a film_topic row pointing to a non-existent/unreadable topic file
        // to exercise the `load_topic_file` error branch.
        let cfg = cfg_for_test();
        run_rank_match(
            &cfg,
            &RankMatchArgs {
                forum: Some("252".into()),
                root: Some(root.clone()),
            },
        )
        .await
        .unwrap();
        // Corrupt one topic JSON.
        let bad = root
            .join("forums")
            .join("252")
            .join("topics")
            .join("4002.json");
        std::fs::write(&bad, b"{not valid json").unwrap();
        {
            // Insert orphan film_topic row so aggregate tries to read it.
            let conn = Connection::open(root.join("state.db")).unwrap();
            conn.execute(
                "INSERT INTO film_topic (film_id, topic_id, forum_id, seeds, leeches, downloads, size_bytes, format_tag, fetched_at) \
                 VALUES ('orph000000000000', '4002', '252', 0, 0, 0, 0, NULL, ?1)",
                params!["2026-04-18T12:00:00+00:00"],
            )
            .unwrap();
        }

        let out = run_rank_aggregate(
            &cfg,
            &RankAggregateArgs {
                forum: Some("252".into()),
                root: Some(root.clone()),
            },
        )
        .await
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        // Only the valid film is scored; bad-json topic is silently skipped.
        assert_eq!(v["films_scored"].as_u64().unwrap(), 1);
    }
}
