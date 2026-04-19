//! `SyncEngine` — per-forum sync loop.
//!
//! **M4**: initial bulk fetch, `.lock` guard, forum-pass transaction boundary,
//! 429/503/520-526 → 1 h cooldown, parser-sanity abort, `RUTRACKER_MIRROR_FAIL_AFTER_N_TOPICS`
//! test-only injection.
//!
//! **M5**: delta detection via `topic_index`, 5-consecutive-older-and-known stop-streak
//! (§5.2 / architect R1), multi-page comment merge with "commit only after all pages"
//! semantics (§5.3), and crash-resumability by running `backfill_missing_index_rows`
//! before every sync (§4.2).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::Utc;
use rand::{Rng, SeedableRng};
use rusqlite::params;
use rutracker_http::{urls, Client};
use rutracker_parser::{forum_page::parse_forum_page, topic::parse_topic_page, TopicRow};
use serde_json::json;

use crate::lock::MirrorLock;
use crate::topic_io::{self, Post, TopicFile};
use crate::{Error, Mirror, Result};

const COOLDOWN_SECONDS: i64 = 60 * 60;
const STOP_STREAK: u32 = 5;
const COMMENTS_PER_PAGE: u32 = 30;
const TOPICS_PER_LISTING_PAGE: u32 = 50;

#[derive(Debug, Clone)]
pub struct SyncOpts {
    pub max_topics: usize,
    pub max_pages: usize,
    pub rate_rps: f32,
    pub max_attempts_per_forum: u32,
    pub cooldown_wait: bool,
    pub cooldown_multiplier: f32,
    pub min_delay_ms: u64,
    pub max_delay_ms: u64,
    pub pause_every_n: u32,
    pub pause_min_secs: u64,
    pub pause_max_secs: u64,
    pub rng_seed: Option<u64>,
    /// Disable the 5-consecutive-older-and-known stop streak. Useful when
    /// resuming a partially-completed initial bulk fetch — without this, the
    /// engine would halt at the known prefix and never reach new rows.
    pub force_full: bool,
    /// Delay (ms) before retrying a Cloudflare transient 5xx (520-526) once
    /// in-request. 429/503 still immediately trip cooldown.
    pub transient_retry_delay_ms: u64,
}

impl Default for SyncOpts {
    fn default() -> Self {
        Self {
            max_topics: 500,
            max_pages: 100,
            rate_rps: 1.0,
            max_attempts_per_forum: 24,
            cooldown_wait: true,
            cooldown_multiplier: 1.0,
            min_delay_ms: 500,
            max_delay_ms: 2_500,
            pause_every_n: 20,
            pause_min_secs: 30,
            pause_max_secs: 60,
            rng_seed: None,
            force_full: false,
            transient_retry_delay_ms: 30_000,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct SyncReport {
    pub files_written: usize,
    pub rows_upserted: usize,
    pub rows_parsed: usize,
    pub rows_unchanged: usize,
    pub forums_skipped_cooldown: usize,
    pub forums_rate_limited: usize,
}

pub struct SyncEngine<'a> {
    mirror: &'a mut Mirror,
    client: Client,
}

enum FetchErr {
    RateLimited,
    Other(Error),
}

impl<'a> SyncEngine<'a> {
    pub fn new(mirror: &'a mut Mirror, client: Client) -> Self {
        Self { mirror, client }
    }

    /// Sync a single forum. See module docs for phase semantics.
    pub async fn sync_forum(&mut self, forum_id: &str, opts: SyncOpts) -> Result<SyncReport> {
        tracing::info!(target: "rutracker_mirror::sync", event = "forum_start", forum_id);

        let _lock = MirrorLock::acquire(self.mirror.root())?;

        let topics_dir = self.mirror.forum_topics_dir(forum_id);
        std::fs::create_dir_all(&topics_dir)?;

        // §4.2 recovery: if a prior run crashed between a JSON write and the SQLite
        // commit, the topic_index is missing rows but the JSONs are on disk. Re-insert
        // any absent rows now so delta detection below sees the full known set.
        self.mirror.backfill_missing_index_rows(forum_id)?;

        let client = self.client.clone();
        let mut rng = build_rng(&opts);
        let listing_referer = forum_index_referer(&client);
        let tx = self.mirror.state_mut().conn_mut().transaction()?;

        tx.execute(
            "INSERT INTO forum_state (forum_id, last_sync_started_at, last_sync_outcome) \
             VALUES (?1, ?2, 'running') \
             ON CONFLICT(forum_id) DO UPDATE SET last_sync_started_at = ?2, last_sync_outcome = 'running'",
            params![forum_id, Utc::now().to_rfc3339()],
        )?;

        let hwm: u64 = tx
            .query_row(
                "SELECT topic_high_water_mark FROM forum_state WHERE forum_id = ?1",
                params![forum_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        let mut idx_map: HashMap<u64, u64> = HashMap::new();
        {
            let mut stmt =
                tx.prepare("SELECT topic_id, last_post_id FROM topic_index WHERE forum_id = ?1")?;
            let rows = stmt.query_map(params![forum_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (tid_s, lpid_s) = row?;
                if let (Ok(tid), Ok(lpid)) = (tid_s.parse::<u64>(), lpid_s.parse::<u64>()) {
                    idx_map.insert(tid, lpid);
                }
            }
        }

        let mut written = 0usize;
        let mut rows_parsed = 0usize;
        let mut rows_unchanged = 0usize;
        let mut high = hwm;
        let mut streak: u32 = 0;
        let mut topic_writes: u32 = 0;
        let mut pages_walked: usize = 0;

        'pages: for page_idx in 0..opts.max_pages {
            let start = (page_idx as u32) * TOPICS_PER_LISTING_PAGE;
            let start_s = start.to_string();

            if page_idx > 0 {
                sleep_jittered(&opts, &mut rng).await;
            }

            let html = match get_with_retry(
                &client,
                urls::VIEWFORUM_PHP,
                &[
                    ("f", forum_id),
                    ("sort", "registered"),
                    ("order", "desc"),
                    ("start", start_s.as_str()),
                ],
                Some(listing_referer.as_str()),
                opts.transient_retry_delay_ms,
            )
            .await
            {
                Ok(h) => h,
                Err(FetchErr::RateLimited) => {
                    tracing::info!(
                        target: "rutracker_mirror::sync",
                        event = "rate_limit_sleep",
                        forum_id
                    );
                    tx.execute(
                        "UPDATE forum_state SET last_sync_outcome='rate_limited', cooldown_until=?1 \
                         WHERE forum_id=?2",
                        params![cooldown_iso(), forum_id],
                    )?;
                    tx.commit()?;
                    return Ok(SyncReport {
                        files_written: written,
                        rows_upserted: written,
                        rows_parsed,
                        rows_unchanged,
                        forums_rate_limited: 1,
                        ..Default::default()
                    });
                }
                Err(FetchErr::Other(e)) => return Err(e),
            };

            let listing = match parse_forum_page(&html) {
                Ok(l) => l,
                Err(rutracker_parser::Error::ParseSanityFailed(_)) if page_idx > 0 => {
                    // Legitimate end-of-listing: forum has exactly N*50 topics and the
                    // (N+1)-th page renders the forum UI with an empty tbody, which the
                    // parser flags as sanity-failed. On page-2+ we treat that as end of
                    // forum; sanity is only hard-fail on page 1 (where a broken parser
                    // is the real concern — see §11 scenario 3).
                    break 'pages;
                }
                Err(e) => return Err(Error::Parser(e)),
            };
            if listing.topics.is_empty() {
                // End of forum listing.
                break 'pages;
            }
            let page_len = listing.topics.len();
            let page_known = listing
                .topics
                .iter()
                .filter(|row| idx_map.contains_key(&row.topic_id))
                .count();
            let page_new = page_len.saturating_sub(page_known);
            pages_walked += 1;
            tracing::info!(
                target: "rutracker_mirror::sync",
                event = "page_parsed",
                forum_id,
                page = page_idx + 1,
                rows = page_len,
                new = page_new,
                known = page_known
            );

            for (i, row) in listing.topics.iter().enumerate() {
                if rows_parsed >= opts.max_topics {
                    break 'pages;
                }
                rows_parsed += 1;
                high = high.max(row.topic_id);

                let known = idx_map.get(&row.topic_id).copied();
                let older = row.topic_id < hwm;

                if older && matches!(known, Some(k) if k == row.last_post_id) {
                    rows_unchanged += 1;
                    streak += 1;
                    if !opts.force_full && streak >= STOP_STREAK {
                        break 'pages;
                    }
                    continue;
                }
                streak = 0;

                // Known but not-older-than-hwm and last_post_id unchanged: nothing to do.
                if matches!(known, Some(k) if k >= row.last_post_id) {
                    rows_unchanged += 1;
                    continue;
                }

                if page_idx > 0 || i > 0 {
                    sleep_jittered(&opts, &mut rng).await;
                }

                let td = match fetch_topic_all_pages(
                    &client,
                    forum_id,
                    row.topic_id,
                    &opts,
                    &mut rng,
                )
                .await
                {
                    Ok(td) => td,
                    Err(FetchErr::RateLimited) => {
                        tracing::info!(
                            target: "rutracker_mirror::sync",
                            event = "rate_limit_sleep",
                            forum_id
                        );
                        tx.execute(
                            "UPDATE forum_state SET last_sync_outcome='rate_limited', cooldown_until=?1 \
                             WHERE forum_id=?2",
                            params![cooldown_iso(), forum_id],
                        )?;
                        tx.commit()?;
                        return Ok(SyncReport {
                            files_written: written,
                            rows_upserted: written,
                            rows_parsed,
                            rows_unchanged,
                            forums_rate_limited: 1,
                            ..Default::default()
                        });
                    }
                    Err(FetchErr::Other(e)) => return Err(e),
                };

                let path = topics_dir.join(format!("{}.json", row.topic_id));
                let existing = read_topic_file(&path).ok();
                let file = build_topic_file(forum_id, row, &td, existing);
                topic_io::write_json_atomic(&path, &file)?;

                tx.execute(
                    "INSERT INTO topic_index \
                     (forum_id, topic_id, title, last_post_id, last_post_at, fetched_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                     ON CONFLICT(forum_id, topic_id) DO UPDATE SET \
                       title = excluded.title, \
                       last_post_id = excluded.last_post_id, \
                       last_post_at = excluded.last_post_at, \
                       fetched_at = excluded.fetched_at",
                    params![
                        forum_id,
                        row.topic_id.to_string(),
                        file.title,
                        file.last_post_id.to_string(),
                        file.last_post_at,
                        file.fetched_at,
                    ],
                )?;

                written += 1;
                topic_writes += 1;
                idx_map.insert(row.topic_id, file.last_post_id);
                tracing::info!(
                    target: "rutracker_mirror::sync",
                    event = "topic_fetched",
                    forum_id,
                    topic_id = row.topic_id,
                    title = row.title.as_str()
                );
                if opts.pause_every_n > 0
                    && topic_writes > 0
                    && topic_writes % opts.pause_every_n == 0
                {
                    let pause_ms = next_pause_ms(&opts, &mut rng);
                    if pause_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(pause_ms)).await;
                    }
                    tracing::info!(
                        target: "rutracker_mirror::sync",
                        event = "reading_pause",
                        slept_secs = pause_ms as f64 / 1000.0
                    );
                }
                maybe_inject_panic(written);
            }

            // Short page = last page.
            if page_len < TOPICS_PER_LISTING_PAGE as usize {
                break 'pages;
            }
        }

        let topics_count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM topic_index WHERE forum_id = ?1",
            params![forum_id],
            |r| r.get(0),
        )?;

        tx.execute(
            "UPDATE forum_state \
             SET last_sync_completed_at = ?1, last_sync_outcome = 'ok', \
                 topic_high_water_mark = ?2, topics_count = ?3 \
             WHERE forum_id = ?4",
            params![
                Utc::now().to_rfc3339(),
                high.to_string(),
                topics_count,
                forum_id,
            ],
        )?;
        tx.commit()?;

        tracing::info!(
            target: "rutracker_mirror::sync",
            event = "forum_complete",
            forum_id,
            topics_count,
            pages_walked
        );

        Ok(SyncReport {
            files_written: written,
            rows_upserted: written,
            rows_parsed,
            rows_unchanged,
            ..Default::default()
        })
    }
}

/// Fetch page 1 of `viewtopic.php?t=<id>`, then `start=30..` pages until the
/// parser-reported total is reached. Parses each page and extends the comment
/// list in place. Returns `FetchErr::RateLimited` on any 429/503 — the caller
/// commits a cooldown and does NOT write a merged file (§5.3 "commit only after
/// all pages").
async fn fetch_topic_all_pages(
    client: &Client,
    forum_id: &str,
    topic_id: u64,
    opts: &SyncOpts,
    rng: &mut impl Rng,
) -> std::result::Result<rutracker_parser::TopicDetails, FetchErr> {
    let tid = topic_id.to_string();
    let forum_referer = forum_referer(client, forum_id);
    let topic_referer = topic_referer(client, topic_id);
    let html = client_get(
        client,
        &[("t", tid.as_str())],
        Some(forum_referer.as_str()),
        opts.transient_retry_delay_ms,
    )
    .await?;
    let mut td = parse_topic_page(&html).map_err(|e| FetchErr::Other(Error::Parser(e)))?;

    let total = td.comment_pages_total;
    if total > 1 {
        for page in 1..total {
            sleep_jittered(opts, rng).await;
            let start = (page * COMMENTS_PER_PAGE).to_string();
            let html = client_get(
                client,
                &[("t", tid.as_str()), ("start", start.as_str())],
                Some(topic_referer.as_str()),
                opts.transient_retry_delay_ms,
            )
            .await?;
            let next = parse_topic_page(&html).map_err(|e| FetchErr::Other(Error::Parser(e)))?;
            td.comments.extend(next.comments);
            td.comment_pages_fetched += 1;
        }
    }
    Ok(td)
}

async fn client_get(
    client: &Client,
    params: &[(&str, &str)],
    referer: Option<&str>,
    retry_delay_ms: u64,
) -> std::result::Result<String, FetchErr> {
    client_get_retry(client, urls::VIEWTOPIC_PHP, params, referer, retry_delay_ms).await
}

/// GET with a single inline retry for Cloudflare transient 5xx (520–526).
/// 429/503 immediately return `RateLimited` (those are explicit rate-limit
/// signals, not edge glitches). After the one retry, any rate-limit status
/// also maps to `RateLimited` (the caller commits cooldown).
async fn client_get_retry(
    client: &Client,
    url: &str,
    params: &[(&str, &str)],
    referer: Option<&str>,
    retry_delay_ms: u64,
) -> std::result::Result<String, FetchErr> {
    match client.get_text_with_referer(url, params, referer).await {
        Ok(h) => Ok(h),
        Err(rutracker_http::Error::Status(s)) if is_cloudflare_transient(s.as_u16()) => {
            tracing::info!(
                target: "rutracker_mirror::sync",
                event = "cloudflare_retry",
                url,
                status = s.as_u16(),
                retry_in_ms = retry_delay_ms
            );
            if retry_delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(retry_delay_ms)).await;
            }
            match client.get_text_with_referer(url, params, referer).await {
                Ok(h) => Ok(h),
                Err(rutracker_http::Error::Status(s)) if is_rate_limit(s.as_u16()) => {
                    Err(FetchErr::RateLimited)
                }
                Err(e) => Err(FetchErr::Other(Error::Http(e))),
            }
        }
        Err(rutracker_http::Error::Status(s)) if is_rate_limit(s.as_u16()) => {
            Err(FetchErr::RateLimited)
        }
        Err(e) => Err(FetchErr::Other(Error::Http(e))),
    }
}

async fn get_with_retry(
    client: &Client,
    url: &str,
    params: &[(&str, &str)],
    referer: Option<&str>,
    retry_delay_ms: u64,
) -> std::result::Result<String, FetchErr> {
    client_get_retry(client, url, params, referer, retry_delay_ms).await
}

fn build_rng(opts: &SyncOpts) -> rand::rngs::StdRng {
    let seed = opts.rng_seed.unwrap_or_else(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64
    });
    rand::rngs::StdRng::seed_from_u64(seed)
}

fn next_delay_ms(opts: &SyncOpts, rng: &mut impl Rng) -> u64 {
    if opts.min_delay_ms == 0 && opts.max_delay_ms == 0 {
        if opts.rate_rps <= 0.0 {
            0
        } else {
            (1000.0 / opts.rate_rps) as u64
        }
    } else {
        rand::Rng::gen_range(
            rng,
            opts.min_delay_ms..=opts.max_delay_ms.max(opts.min_delay_ms),
        )
    }
}

fn next_pause_ms(opts: &SyncOpts, rng: &mut impl Rng) -> u64 {
    let min_ms = opts.pause_min_secs.saturating_mul(1000);
    let max_ms = opts.pause_max_secs.saturating_mul(1000).max(min_ms);
    if min_ms == 0 && max_ms == 0 {
        0
    } else {
        rand::Rng::gen_range(rng, min_ms..=max_ms)
    }
}

async fn sleep_jittered(opts: &SyncOpts, rng: &mut impl Rng) {
    let ms = next_delay_ms(opts, rng);
    if ms > 0 {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }
}

fn forum_root(client: &Client) -> String {
    let trimmed = client.base().trim_end_matches('/');
    trimmed
        .strip_suffix("/forum")
        .unwrap_or(trimmed)
        .to_string()
}

fn forum_index_referer(client: &Client) -> String {
    format!("{}/forum/{}", forum_root(client), urls::INDEX_PHP)
}

fn forum_referer(client: &Client, forum_id: &str) -> String {
    format!(
        "{}/forum/{}?f={forum_id}",
        forum_root(client),
        urls::VIEWFORUM_PHP
    )
}

fn topic_referer(client: &Client, topic_id: u64) -> String {
    format!(
        "{}/forum/{}?t={topic_id}",
        forum_root(client),
        urls::VIEWTOPIC_PHP
    )
}

fn is_rate_limit(code: u16) -> bool {
    // 429 Too Many Requests, 503 Service Unavailable — per plan §5.2/§5.4.
    // Cloudflare origin-reachability 5xx (520–526) also map to cooldown after an
    // inline retry fails: they are transient edge-layer errors observed in live
    // rutracker sync runs.
    matches!(code, 429 | 503 | 520..=526)
}

fn is_cloudflare_transient(code: u16) -> bool {
    matches!(code, 520..=526)
}

fn cooldown_iso() -> String {
    (Utc::now() + chrono::Duration::seconds(COOLDOWN_SECONDS)).to_rfc3339()
}

fn read_topic_file(path: &Path) -> Result<TopicFile> {
    let s = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&s)?)
}

/// Assemble the topic JSON to write. When `existing` is `Some`, its comments
/// are merged with `td.comments` keyed by `post_id` — fresh comments overwrite
/// older revisions (§5.3 acknowledged-data-loss: no revision history kept).
fn build_topic_file(
    forum_id: &str,
    row: &TopicRow,
    td: &rutracker_parser::TopicDetails,
    existing: Option<TopicFile>,
) -> TopicFile {
    let opening_post = Post {
        post_id: 0,
        author: String::new(),
        date: String::new(),
        text: td.description.clone(),
    };

    let mut merged: BTreeMap<u64, Post> = BTreeMap::new();
    if let Some(ex) = &existing {
        for c in &ex.comments {
            merged.insert(c.post_id, c.clone());
        }
    }
    for c in &td.comments {
        merged.insert(c.post_id, Post::from(c.clone()));
    }
    let comments: Vec<Post> = merged.into_values().collect();
    let max_comment_id = comments.iter().map(|p| p.post_id).max().unwrap_or(0);
    let last_post_id = row.last_post_id.max(max_comment_id);

    let metadata = json!({
        "magnet_link": td.magnet_link,
        "size": td.size,
        "seeds": td.seeds,
        "leeches": td.leeches,
        "author": row.author,
        "downloads": row.downloads,
        "reply_count": row.reply_count,
        "file_list": td.file_list,
        "parsed": td.metadata,
        "comment_pages_fetched": td.comment_pages_fetched,
        "comment_pages_total": td.comment_pages_total,
    });

    TopicFile {
        schema_version: 1,
        topic_id: row.topic_id.to_string(),
        forum_id: forum_id.to_string(),
        title: row.title.clone(),
        fetched_at: Utc::now().to_rfc3339(),
        last_post_id,
        last_post_at: row.last_post_at.clone(),
        opening_post,
        comments,
        metadata,
        size_bytes: size_to_bytes(&td.size),
        seeds: Some(td.seeds),
        leeches: Some(td.leeches),
        downloads: Some(row.downloads),
    }
}

/// Parse a rutracker size string like `"2.22 GB"`, `"502 MB"`, `"700 KB"`, `"123 B"`
/// into raw bytes. Accepts `.` or `,` as decimal separator. Returns `None` on
/// empty input or unknown/missing unit. Lives here (not in ranker) so the
/// mirror can populate `TopicFile.size_bytes` at fetch time without a circular
/// dep.
fn size_to_bytes(s: &str) -> Option<u64> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    let cut = trimmed
        .char_indices()
        .find(|(_, c)| c.is_alphabetic())
        .map(|(i, _)| i)?;
    let (num_part, unit_part) = trimmed.split_at(cut);
    let cleaned: String = num_part
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| if c == ',' { '.' } else { c })
        .collect();
    let value: f64 = cleaned.parse().ok()?;
    let unit = unit_part.trim().to_lowercase();
    let mult: f64 = match unit.as_str() {
        "b" | "bytes" | "байт" => 1.0,
        "kb" | "kib" | "кб" | "кбайт" => 1024.0,
        "mb" | "mib" | "мб" | "мбайт" => 1024.0 * 1024.0,
        "gb" | "gib" | "гб" | "гбайт" => 1024.0 * 1024.0 * 1024.0,
        "tb" | "tib" | "тб" | "тбайт" => 1024.0_f64.powi(4),
        _ => return None,
    };
    Some((value * mult) as u64)
}

/// Rebuild `topic_index` from the on-disk JSON layer. Walks `forums/<id>/topics/*.json`
/// for every forum directory under the mirror root; truncates the existing index
/// table first so the result matches whatever is on disk exactly.
///
/// The JSON-per-topic layer is the source of truth; `state.db` is derived (§4.1).
/// Returns the number of rows inserted.
pub fn rebuild_index(mirror: &mut Mirror) -> Result<usize> {
    let forums_dir = mirror.root().join("forums");
    mirror
        .state_mut()
        .conn_mut()
        .execute("DELETE FROM topic_index", [])?;
    if !forums_dir.exists() {
        return Ok(0);
    }
    let mut forum_ids: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&forums_dir)? {
        let entry = entry?;
        if entry.path().is_dir() {
            if let Some(name) = entry.file_name().to_str() {
                forum_ids.push(name.to_string());
            }
        }
    }
    let mut total = 0usize;
    for forum_id in forum_ids {
        total += mirror.backfill_missing_index_rows(&forum_id)?;
    }
    Ok(total)
}

#[cfg(any(test, feature = "fail-injection"))]
fn maybe_inject_panic(written: usize) {
    if let Ok(raw) = std::env::var("RUTRACKER_MIRROR_FAIL_AFTER_N_TOPICS") {
        if let Ok(n) = raw.parse::<usize>() {
            if written >= n {
                panic!("RUTRACKER_MIRROR_FAIL_AFTER_N_TOPICS panic at {written}");
            }
        }
    }
}

#[cfg(not(any(test, feature = "fail-injection")))]
fn maybe_inject_panic(_written: usize) {}

#[cfg(test)]
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::*;
    use crate::lock::MirrorLock;
    use crate::topic_io::TopicFile;
    use chrono::DateTime;
    use rand::SeedableRng;
    use std::fmt::Write as _;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;
    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::{Context, SubscriberExt};
    use tracing_subscriber::{Layer, Registry};
    use wiremock::matchers::{header, method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // Serialises every test that runs `sync_forum` (or the panic-injection variants).
    // `RUTRACKER_MIRROR_FAIL_AFTER_N_TOPICS` is process-global; letting tests run in
    // parallel would leak the injected panic threshold across them.
    static SYNC_SERIAL: Mutex<()> = Mutex::new(());

    #[derive(Clone, Default)]
    struct EventCaptureLayer {
        events: Arc<Mutex<Vec<String>>>,
    }

    #[derive(Default)]
    struct EventFieldVisitor {
        event: Option<String>,
    }

    impl Visit for EventFieldVisitor {
        fn record_str(&mut self, field: &Field, value: &str) {
            if field.name() == "event" {
                self.event = Some(value.to_string());
            }
        }

        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if field.name() == "event" {
                self.event = Some(format!("{value:?}").trim_matches('"').to_string());
            }
        }
    }

    impl<S> Layer<S> for EventCaptureLayer
    where
        S: tracing::Subscriber,
    {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            let mut visitor = EventFieldVisitor::default();
            event.record(&mut visitor);
            if let Some(name) = visitor.event {
                self.events.lock().unwrap().push(name);
            }
        }
    }

    fn build_forum_listing_html(forum_id: &str, rows: &[(u64, u64)]) -> String {
        // rows: Vec<(topic_id, last_post_id)>
        let mut acc = String::new();
        for (idx, (tid, lpi)) in rows.iter().enumerate() {
            let _ = write!(
                acc,
                concat!(
                    "<tr class=\"hl-tr\" data-topic_id=\"{tid}\">\n",
                    "  <td class=\"vf-col-t-title\"><a class=\"tt-text\" href=\"viewtopic.php?t={tid}\">Topic {tid}</a></td>\n",
                    "  <td class=\"u-name-col\"><a>author{idx}</a></td>\n",
                    "  <td class=\"tor-size\"><u>100</u> MB</td>\n",
                    "  <td><b class=\"seedmed\">5</b></td>\n",
                    "  <td class=\"leechmed\">2</td>\n",
                    "  <td class=\"vf-col-last-post\"><p>18-Apr-26 12:00</p>",
                    "<p><a href=\"viewtopic.php?p={lpi}#{lpi}\">link</a></p></td>\n",
                    "</tr>\n",
                ),
                tid = tid,
                idx = idx,
                lpi = lpi,
            );
        }
        let padding = "x".repeat(2048);
        format!(
            r#"<!DOCTYPE html><html><head>
<link rel="canonical" href="https://rutracker.org/forum/viewforum.php?f={forum_id}">
<title>Forum {forum_id} {padding}</title></head><body>
<table class="vf-tor"><tbody>
{acc}
</tbody></table></body></html>"#
        )
    }

    fn simple_listing(forum_id: &str, topic_ids: &[u64]) -> String {
        let rows: Vec<(u64, u64)> = topic_ids
            .iter()
            .map(|tid| (*tid, 1_000_000 + tid))
            .collect();
        build_forum_listing_html(forum_id, &rows)
    }

    fn build_empty_forum_listing_html(forum_id: &str) -> String {
        let padding = "z".repeat(2048);
        format!(
            r#"<!DOCTYPE html><html><head>
<link rel="canonical" href="https://rutracker.org/forum/viewforum.php?f={forum_id}">
<title>Empty {forum_id} {padding}</title></head><body>
<table class="vf-tor"><tbody></tbody></table></body></html>"#
        )
    }

    /// Single-page topic HTML: opening post only, no comments.
    fn build_topic_html(topic_id: u64) -> String {
        let op_id = 9_000_000 + topic_id;
        format!(
            r##"<!DOCTYPE html><html><head>
<link rel="canonical" href="https://rutracker.org/forum/viewtopic.php?t={topic_id}">
<title>Topic {topic_id}</title></head><body>
<h1 id="topic-title">Test topic {topic_id}</h1>
<a class="magnet-link" href="magnet:?xt=urn:btih:dummy{topic_id}">Magnet</a>
<span id="tor-size-humn">100 MB</span>
<span class="seed"><b>5</b></span>
<span class="leech"><b>2</b></span>
<table>
<tbody id="post_{op_id}">
<tr><td><p class="nick">author</p><a class="p-link small" href="#">18-Apr-26 12:00</a><div class="post_body">Description for {topic_id}</div></td></tr>
</tbody></table></body></html>"##
        )
    }

    /// Multi-page topic HTML. `page_index` is 0-based. `post_ids` are the
    /// comment post ids on this page. `total_pages` controls the `a.pg` hint so
    /// the parser reports the correct page count.
    fn build_multi_page_topic_html(
        topic_id: u64,
        page_index: u32,
        total_pages: u32,
        post_ids: &[u64],
    ) -> String {
        let op_id = 9_000_000 + topic_id;
        let mut comments = String::new();
        for pid in post_ids {
            let _ = write!(
                comments,
                concat!(
                    "<tbody id=\"post_{pid}\">\n",
                    "<tr><td><p class=\"nick\">commenter{pid}</p>",
                    "<a class=\"p-link small\" href=\"#\">18-Apr-26 12:00</a>",
                    "<div class=\"post_body\">Comment {pid} text</div></td></tr>\n",
                    "</tbody>\n",
                ),
                pid = pid,
            );
        }
        let mut pagination = String::new();
        for p in 1..total_pages {
            let start = p * COMMENTS_PER_PAGE;
            let _ = write!(
                pagination,
                r#"<a class="pg" href="viewtopic.php?t={topic_id}&amp;start={start}">{p}</a>"#
            );
        }
        let canonical_start = if page_index == 0 {
            String::new()
        } else {
            format!("&amp;start={}", page_index * COMMENTS_PER_PAGE)
        };
        format!(
            r##"<!DOCTYPE html><html><head>
<link rel="canonical" href="https://rutracker.org/forum/viewtopic.php?t={topic_id}{canonical_start}">
<title>Topic {topic_id}</title></head><body>
<h1 id="topic-title">Multi-page topic {topic_id}</h1>
<a class="magnet-link" href="magnet:?xt=urn:btih:dummy{topic_id}">Magnet</a>
<span id="tor-size-humn">100 MB</span>
<span class="seed"><b>5</b></span>
<span class="leech"><b>2</b></span>
{pagination}
<table>
<tbody id="post_{op_id}">
<tr><td><p class="nick">author</p><a class="p-link small" href="#">18-Apr-26 12:00</a><div class="post_body">Description for {topic_id}</div></td></tr>
</tbody>
{comments}
</table></body></html>"##
        )
    }

    async fn stub_listing_body(server: &MockServer, forum_id: &str, body: String) {
        Mock::given(method("GET"))
            .and(path("/forum/viewforum.php"))
            .and(query_param("f", forum_id))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(server)
            .await;
    }

    async fn stub_forum_listing(server: &MockServer, forum_id: &str, topic_ids: &[u64]) {
        stub_listing_body(server, forum_id, simple_listing(forum_id, topic_ids)).await;
    }

    async fn stub_empty_forum_listing(server: &MockServer, forum_id: &str) {
        stub_listing_body(server, forum_id, build_empty_forum_listing_html(forum_id)).await;
    }

    async fn stub_429(server: &MockServer, forum_id: &str) {
        Mock::given(method("GET"))
            .and(path("/forum/viewforum.php"))
            .and(query_param("f", forum_id))
            .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
            .mount(server)
            .await;
    }

    async fn stub_status(server: &MockServer, forum_id: &str, status: u16) {
        Mock::given(method("GET"))
            .and(path("/forum/viewforum.php"))
            .and(query_param("f", forum_id))
            .respond_with(ResponseTemplate::new(status).set_body_string("cloudflare"))
            .mount(server)
            .await;
    }

    async fn stub_all_topics(server: &MockServer, topic_ids: &[u64]) {
        for tid in topic_ids {
            let body = build_topic_html(*tid);
            Mock::given(method("GET"))
                .and(path("/forum/viewtopic.php"))
                .and(query_param("t", tid.to_string().as_str()))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .mount(server)
                .await;
        }
    }

    fn fast_opts(max_topics: usize) -> SyncOpts {
        SyncOpts {
            max_topics,
            max_pages: 100,
            rate_rps: 0.0,
            max_attempts_per_forum: 24,
            cooldown_wait: true,
            cooldown_multiplier: 1.0,
            min_delay_ms: 0,
            max_delay_ms: 0,
            pause_every_n: 0,
            pause_min_secs: 0,
            pause_max_secs: 0,
            rng_seed: Some(7),
            force_full: false,
            transient_retry_delay_ms: 0,
        }
    }

    fn make_client(server: &MockServer) -> Client {
        Client::new(&format!("{}/forum/", server.uri())).unwrap()
    }

    fn topic_index_count(m: &Mirror) -> i64 {
        m.state()
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM topic_index WHERE forum_id = '252'",
                [],
                |r| r.get(0),
            )
            .unwrap()
    }

    // ────────────────────────── M4 tests ──────────────────────────

    #[tokio::test]
    async fn test_sync_forum_writes_expected_files_and_rows() {
        let _serial = SYNC_SERIAL.lock().unwrap();
        let server = MockServer::start().await;
        let topic_ids: Vec<u64> = (6_000_001..=6_000_010).collect();
        stub_forum_listing(&server, "252", &topic_ids).await;
        stub_all_topics(&server, &topic_ids).await;

        let td = TempDir::new().unwrap();
        let mut m = Mirror::init(td.path()).unwrap();
        let client = make_client(&server);
        let mut engine = SyncEngine::new(&mut m, client);
        let report = engine.sync_forum("252", fast_opts(10)).await.unwrap();

        assert_eq!(report.files_written, 10);
        assert_eq!(report.rows_upserted, 10);

        let topics_dir = td.path().join("forums").join("252").join("topics");
        for tid in &topic_ids {
            let p = topics_dir.join(format!("{}.json", tid));
            assert!(p.exists(), "topic file missing: {}", p.display());
        }
        assert_eq!(topic_index_count(&m), 10);
    }

    #[tokio::test]
    async fn test_topic_json_round_trips() {
        let _serial = SYNC_SERIAL.lock().unwrap();
        let server = MockServer::start().await;
        let topic_ids: Vec<u64> = (7_000_001..=7_000_010).collect();
        stub_forum_listing(&server, "252", &topic_ids).await;
        stub_all_topics(&server, &topic_ids).await;

        let td = TempDir::new().unwrap();
        let mut m = Mirror::init(td.path()).unwrap();
        let client = make_client(&server);
        let mut engine = SyncEngine::new(&mut m, client);
        engine.sync_forum("252", fast_opts(10)).await.unwrap();

        let topics_dir = td.path().join("forums").join("252").join("topics");
        for tid in &topic_ids {
            let p = topics_dir.join(format!("{}.json", tid));
            let data = std::fs::read_to_string(&p).unwrap();
            let file: TopicFile = serde_json::from_str(&data).unwrap();
            assert_eq!(file.topic_id, tid.to_string(), "topic_id matches filename");
            let first = serde_json::to_string(&file).unwrap();
            let round: TopicFile = serde_json::from_str(&first).unwrap();
            let second = serde_json::to_string(&round).unwrap();
            assert_eq!(first, second, "round-trip must be stable");
        }
    }

    #[tokio::test]
    async fn test_transaction_boundary_commit_only_on_success() {
        let server = MockServer::start().await;
        let topic_ids: Vec<u64> = (8_000_001..=8_000_010).collect();
        stub_forum_listing(&server, "252", &topic_ids).await;
        stub_all_topics(&server, &topic_ids).await;

        let td = TempDir::new().unwrap();
        {
            let _m = Mirror::init(td.path()).unwrap();
        }

        let _guard = SYNC_SERIAL.lock().unwrap();
        std::env::set_var("RUTRACKER_MIRROR_FAIL_AFTER_N_TOPICS", "3");

        let root = td.path().to_path_buf();
        let mock_uri = server.uri();
        // Run the panicking sync in a fresh thread so it owns its own tokio
        // runtime — `rt.block_on` from within the `#[tokio::test]` reactor would
        // panic immediately ("runtime within a runtime") and defeat the injection.
        let panic_result = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let client = Client::new(&format!("{}/forum/", mock_uri)).unwrap();
                let mut m = Mirror::open(&root, None).unwrap();
                let mut engine = SyncEngine::new(&mut m, client);
                let _ = engine.sync_forum("252", fast_opts(10)).await;
            });
        })
        .join();
        std::env::remove_var("RUTRACKER_MIRROR_FAIL_AFTER_N_TOPICS");
        assert!(panic_result.is_err(), "expected injected panic");

        {
            let m = Mirror::open(td.path(), None).unwrap();
            assert_eq!(
                topic_index_count(&m),
                0,
                "transaction should have rolled back on panic"
            );
        }

        {
            let mut m = Mirror::open(td.path(), None).unwrap();
            let client = make_client(&server);
            let mut engine = SyncEngine::new(&mut m, client);
            engine.sync_forum("252", fast_opts(10)).await.unwrap();
            assert_eq!(topic_index_count(&m), 10);
        }
        drop(_guard);
    }

    #[test]
    fn test_concurrent_sync_sees_lock_and_aborts() {
        let _serial = SYNC_SERIAL.lock().unwrap();
        let td = TempDir::new().unwrap();
        {
            let _m = Mirror::init(td.path()).unwrap();
        }

        // Simulate "first sync is running" by holding the root lock.
        let lock1 = MirrorLock::acquire(td.path()).unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt.block_on(async {
            let client = Client::new("http://127.0.0.1:9/forum/").unwrap();
            let mut m = Mirror::open(td.path(), None).unwrap();
            let mut engine = SyncEngine::new(&mut m, client);
            engine.sync_forum("252", fast_opts(10)).await
        });

        drop(lock1);
        match err {
            Err(Error::Locked { holder_pid }) => {
                assert_eq!(holder_pid, std::process::id());
            }
            other => panic!("expected Err(Locked), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_429_marks_cooldown_and_aborts() {
        let _serial = SYNC_SERIAL.lock().unwrap();
        let server = MockServer::start().await;
        stub_429(&server, "252").await;

        let td = TempDir::new().unwrap();
        let mut m = Mirror::init(td.path()).unwrap();
        let client = make_client(&server);
        let mut engine = SyncEngine::new(&mut m, client);

        let before = Utc::now();
        let report = engine.sync_forum("252", fast_opts(10)).await.unwrap();
        assert_eq!(report.forums_rate_limited, 1);
        assert_eq!(report.files_written, 0);

        let (outcome, cooldown_until): (String, String) = m
            .state()
            .conn()
            .query_row(
                "SELECT last_sync_outcome, cooldown_until FROM forum_state WHERE forum_id='252'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(outcome, "rate_limited");
        let until = DateTime::parse_from_rfc3339(&cooldown_until)
            .unwrap()
            .with_timezone(&Utc);
        let delta = (until - before).num_seconds();
        assert!(
            (59 * 60..=61 * 60).contains(&delta),
            "cooldown must fall in [59m, 61m], got {delta}s"
        );
    }

    async fn stub_cloudflare_then_ok(
        server: &MockServer,
        forum_id: &str,
        topic_ids: &[u64],
        cf_status: u16,
    ) {
        // First call to /forum/viewforum.php → Cloudflare transient; then 200 listing.
        Mock::given(method("GET"))
            .and(path("/forum/viewforum.php"))
            .and(query_param("f", forum_id))
            .respond_with(ResponseTemplate::new(cf_status).set_body_string("cf-transient"))
            .up_to_n_times(1)
            .mount(server)
            .await;
        stub_forum_listing(server, forum_id, topic_ids).await;
    }

    async fn stub_listing_page(server: &MockServer, forum_id: &str, start: u32, topic_ids: &[u64]) {
        let body = simple_listing(forum_id, topic_ids);
        Mock::given(method("GET"))
            .and(path("/forum/viewforum.php"))
            .and(query_param("f", forum_id))
            .and(query_param("start", start.to_string().as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn test_forum_listing_pagination_walks_all_pages() {
        // RED: a forum with 150 topics split across 3 listing pages (0, 50, 100).
        // The engine must walk all 3 pages and download every topic.
        let _serial = SYNC_SERIAL.lock().unwrap();
        let server = MockServer::start().await;
        let page1: Vec<u64> = (9_000_001..=9_000_050).collect();
        let page2: Vec<u64> = (9_000_051..=9_000_100).collect();
        let page3: Vec<u64> = (9_000_101..=9_000_150).collect();

        stub_listing_page(&server, "252", 0, &page1).await;
        stub_listing_page(&server, "252", 50, &page2).await;
        stub_listing_page(&server, "252", 100, &page3).await;
        // Page 4 (start=150): empty → signals end of listing.
        Mock::given(method("GET"))
            .and(path("/forum/viewforum.php"))
            .and(query_param("f", "252"))
            .and(query_param("start", "150"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(build_empty_forum_listing_html("252")),
            )
            .mount(&server)
            .await;

        let all_ids: Vec<u64> = page1
            .iter()
            .chain(page2.iter())
            .chain(page3.iter())
            .copied()
            .collect();
        stub_all_topics(&server, &all_ids).await;

        let td = TempDir::new().unwrap();
        let mut m = Mirror::init(td.path()).unwrap();
        let client = make_client(&server);
        let mut engine = SyncEngine::new(&mut m, client);

        let report = engine.sync_forum("252", fast_opts(500)).await.unwrap();
        assert_eq!(
            report.rows_parsed, 150,
            "engine must walk all 3 listing pages"
        );
        assert_eq!(report.files_written, 150);
    }

    #[tokio::test]
    async fn test_forum_listing_pagination_respects_max_pages_cap() {
        // Safety rail: `max_pages` bounds the walk even if the forum keeps
        // returning full pages. 2-page cap over 4 available pages → 100 rows.
        let _serial = SYNC_SERIAL.lock().unwrap();
        let server = MockServer::start().await;
        let page1: Vec<u64> = (9_100_001..=9_100_050).collect();
        let page2: Vec<u64> = (9_100_051..=9_100_100).collect();
        let page3: Vec<u64> = (9_100_101..=9_100_150).collect();
        let page4: Vec<u64> = (9_100_151..=9_100_200).collect();

        stub_listing_page(&server, "252", 0, &page1).await;
        stub_listing_page(&server, "252", 50, &page2).await;
        stub_listing_page(&server, "252", 100, &page3).await;
        stub_listing_page(&server, "252", 150, &page4).await;

        let mut seen: Vec<u64> = Vec::new();
        seen.extend(&page1);
        seen.extend(&page2);
        seen.extend(&page3);
        seen.extend(&page4);
        stub_all_topics(&server, &seen).await;

        let td = TempDir::new().unwrap();
        let mut m = Mirror::init(td.path()).unwrap();
        let client = make_client(&server);
        let mut engine = SyncEngine::new(&mut m, client);

        let opts = SyncOpts {
            max_pages: 2,
            ..fast_opts(500)
        };
        let report = engine.sync_forum("252", opts).await.unwrap();
        assert_eq!(
            report.rows_parsed, 100,
            "max_pages=2 caps the walk at 2 listing pages (100 rows)"
        );
    }

    #[tokio::test]
    async fn test_cloudflare_520_retry_then_success() {
        // RED: a single 520 on the listing page should NOT hard-cooldown;
        // inline retry (one attempt) should recover and sync normally.
        let _serial = SYNC_SERIAL.lock().unwrap();
        let server = MockServer::start().await;
        let topic_ids: Vec<u64> = (7_000_001..=7_000_005).collect();
        stub_cloudflare_then_ok(&server, "252", &topic_ids, 520).await;
        stub_all_topics(&server, &topic_ids).await;

        let td = TempDir::new().unwrap();
        let mut m = Mirror::init(td.path()).unwrap();
        let client = make_client(&server);
        let mut engine = SyncEngine::new(&mut m, client);
        let opts = SyncOpts {
            transient_retry_delay_ms: 5,
            ..fast_opts(5)
        };

        let report = engine.sync_forum("252", opts).await.unwrap();
        assert_eq!(
            report.forums_rate_limited, 0,
            "inline retry must not flip cooldown"
        );
        assert_eq!(report.files_written, 5);
    }

    #[tokio::test]
    async fn test_force_full_bypasses_stop_streak() {
        // RED: stop-streak (5 known-unchanged rows) normally halts a forum pass early.
        // With `force_full = true`, the engine must walk the full listing regardless.
        let _serial = SYNC_SERIAL.lock().unwrap();
        let server = MockServer::start().await;
        let topic_ids: Vec<u64> = (8_000_001..=8_000_010).collect();
        stub_forum_listing(&server, "252", &topic_ids).await;
        stub_all_topics(&server, &topic_ids).await;

        let td = TempDir::new().unwrap();
        let mut m = Mirror::init(td.path()).unwrap();
        let client = make_client(&server);
        let mut engine = SyncEngine::new(&mut m, client);

        // Seed: first pass to populate topic_index + hwm.
        let first = engine.sync_forum("252", fast_opts(10)).await.unwrap();
        assert_eq!(first.files_written, 10);

        // Without force_full: all rows older-and-known-unchanged → stop after 5, rows_parsed should be ≤ 5.
        let normal = engine.sync_forum("252", fast_opts(10)).await.unwrap();
        assert!(
            normal.rows_parsed <= 5,
            "stop-streak should halt after 5 known rows (got {})",
            normal.rows_parsed
        );

        // With force_full: walks all rows, rows_parsed == 10.
        let forced = engine
            .sync_forum(
                "252",
                SyncOpts {
                    force_full: true,
                    ..fast_opts(10)
                },
            )
            .await
            .unwrap();
        assert_eq!(forced.rows_parsed, 10, "force_full must bypass stop-streak");
    }

    #[tokio::test]
    async fn test_cloudflare_5xx_marks_cooldown_and_aborts() {
        for status in [520u16, 521, 522, 523, 524, 525, 526] {
            let _serial = SYNC_SERIAL.lock().unwrap();
            let server = MockServer::start().await;
            stub_status(&server, "252", status).await;

            let td = TempDir::new().unwrap();
            let mut m = Mirror::init(td.path()).unwrap();
            let client = make_client(&server);
            let mut engine = SyncEngine::new(&mut m, client);

            let report = engine
                .sync_forum("252", fast_opts(10))
                .await
                .unwrap_or_else(|e| panic!("status {status} should cooldown, got error: {e}"));
            assert_eq!(
                report.forums_rate_limited, 1,
                "status {status} must mark as rate-limited"
            );
            assert_eq!(report.files_written, 0);

            let outcome: String = m
                .state()
                .conn()
                .query_row(
                    "SELECT last_sync_outcome FROM forum_state WHERE forum_id='252'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(outcome, "rate_limited", "status {status} → rate_limited");
        }
    }

    #[tokio::test]
    async fn test_parser_returning_zero_rows_aborts_forum() {
        let _serial = SYNC_SERIAL.lock().unwrap();
        let server = MockServer::start().await;
        stub_empty_forum_listing(&server, "252").await;

        let td = TempDir::new().unwrap();
        let mut m = Mirror::init(td.path()).unwrap();
        let client = make_client(&server);
        let mut engine = SyncEngine::new(&mut m, client);

        let err = engine
            .sync_forum("252", fast_opts(10))
            .await
            .expect_err("empty listing should trip parse-sanity");
        assert!(
            matches!(
                err,
                Error::Parser(rutracker_parser::Error::ParseSanityFailed(_))
            ),
            "expected Parser(ParseSanityFailed), got {err:?}"
        );

        // topic_index must be untouched on parse-sanity abort.
        assert_eq!(topic_index_count(&m), 0);
    }

    // ────────────────────────── M5 tests ──────────────────────────

    #[tokio::test]
    async fn test_idempotent_no_upstream_changes() {
        let _serial = SYNC_SERIAL.lock().unwrap();
        let server = MockServer::start().await;
        let topic_ids: Vec<u64> = (5_000_001..=5_000_010).collect();
        stub_forum_listing(&server, "252", &topic_ids).await;
        stub_all_topics(&server, &topic_ids).await;

        let td = TempDir::new().unwrap();
        let mut m = Mirror::init(td.path()).unwrap();
        let client = make_client(&server);

        // First sync populates.
        {
            let mut engine = SyncEngine::new(&mut m, client.clone());
            let r = engine.sync_forum("252", fast_opts(10)).await.unwrap();
            assert_eq!(r.files_written, 10);
        }

        // Second sync against identical fixtures: 0 files written, stop-streak trips.
        let t0 = std::time::Instant::now();
        let report = {
            let mut engine = SyncEngine::new(&mut m, client);
            engine.sync_forum("252", fast_opts(10)).await.unwrap()
        };
        let elapsed = t0.elapsed();

        assert_eq!(report.files_written, 0, "idempotent: no writes");
        assert!(report.rows_unchanged >= STOP_STREAK as usize);
        assert!(elapsed < Duration::from_secs(1), "got {elapsed:?}");
    }

    #[tokio::test]
    async fn test_new_post_on_existing_topic_rewrites_only_that_topic() {
        let _serial = SYNC_SERIAL.lock().unwrap();
        let topic_ids: Vec<u64> = (4_000_001..=4_000_010).collect();

        // First sync with server A.
        let td = TempDir::new().unwrap();
        let mut m = Mirror::init(td.path()).unwrap();
        let server_a = MockServer::start().await;
        stub_forum_listing(&server_a, "252", &topic_ids).await;
        stub_all_topics(&server_a, &topic_ids).await;
        {
            let mut engine = SyncEngine::new(&mut m, make_client(&server_a));
            engine.sync_forum("252", fast_opts(10)).await.unwrap();
        }
        drop(server_a);

        let topics_dir = td.path().join("forums").join("252").join("topics");
        let mtimes_before: HashMap<u64, std::time::SystemTime> = topic_ids
            .iter()
            .map(|tid| {
                let p = topics_dir.join(format!("{}.json", tid));
                (*tid, std::fs::metadata(&p).unwrap().modified().unwrap())
            })
            .collect();

        // Ensure filesystem mtime granularity will register a change if we rewrite.
        std::thread::sleep(Duration::from_millis(1100));

        // Second server mutates last_post_id for one topic.
        let bumped_tid = 4_000_005u64;
        let rows: Vec<(u64, u64)> = topic_ids
            .iter()
            .map(|tid| {
                let lpi = if *tid == bumped_tid {
                    1_000_000 + tid + 999
                } else {
                    1_000_000 + tid
                };
                (*tid, lpi)
            })
            .collect();
        let server_b = MockServer::start().await;
        stub_listing_body(&server_b, "252", build_forum_listing_html("252", &rows)).await;
        stub_all_topics(&server_b, &topic_ids).await;

        let report = {
            let mut engine = SyncEngine::new(&mut m, make_client(&server_b));
            engine.sync_forum("252", fast_opts(10)).await.unwrap()
        };

        assert_eq!(
            report.files_written, 1,
            "exactly the one mutated topic should be rewritten"
        );

        for tid in &topic_ids {
            let p = topics_dir.join(format!("{}.json", tid));
            let after = std::fs::metadata(&p).unwrap().modified().unwrap();
            let before = mtimes_before[tid];
            if *tid == bumped_tid {
                assert!(after > before, "mutated topic mtime should advance");
            } else {
                assert_eq!(after, before, "topic {tid} mtime must be unchanged");
            }
        }
    }

    #[tokio::test]
    async fn test_crash_mid_forum_resumes_cleanly() {
        let topic_ids: Vec<u64> = (3_000_001..=3_000_010).collect();
        let server = MockServer::start().await;
        stub_forum_listing(&server, "252", &topic_ids).await;
        stub_all_topics(&server, &topic_ids).await;

        let td = TempDir::new().unwrap();
        {
            let _m = Mirror::init(td.path()).unwrap();
        }

        let _guard = SYNC_SERIAL.lock().unwrap();
        std::env::set_var("RUTRACKER_MIRROR_FAIL_AFTER_N_TOPICS", "3");

        let root = td.path().to_path_buf();
        let mock_uri = server.uri();
        // Fresh thread so it owns its own tokio runtime — `rt.block_on` from
        // within the outer reactor panics immediately ("runtime within a
        // runtime") and defeats the injection.
        let panic_result = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let client = Client::new(&format!("{}/forum/", mock_uri)).unwrap();
                let mut m = Mirror::open(&root, None).unwrap();
                let mut engine = SyncEngine::new(&mut m, client);
                let _ = engine.sync_forum("252", fast_opts(10)).await;
            });
        })
        .join();
        std::env::remove_var("RUTRACKER_MIRROR_FAIL_AFTER_N_TOPICS");
        assert!(panic_result.is_err(), "expected injected panic");

        // 3 JSONs on disk; topic_index rolled back to 0.
        let topics_dir = td.path().join("forums").join("252").join("topics");
        let partial_files: Vec<_> = std::fs::read_dir(&topics_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
            .collect();
        assert_eq!(partial_files.len(), 3, "3 JSONs should have landed");
        {
            let m = Mirror::open(td.path(), None).unwrap();
            assert_eq!(topic_index_count(&m), 0, "tx rolled back");
        }

        // Resume: backfill + delta detection should skip the 3 existing, fetch the 7 missing.
        let report = {
            let mut m = Mirror::open(td.path(), None).unwrap();
            let client = make_client(&server);
            let mut engine = SyncEngine::new(&mut m, client);
            engine.sync_forum("252", fast_opts(10)).await.unwrap()
        };

        {
            let m = Mirror::open(td.path(), None).unwrap();
            let distinct: i64 = m
                .state()
                .conn()
                .query_row(
                    "SELECT COUNT(DISTINCT topic_id) FROM topic_index WHERE forum_id='252'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(distinct, 10, "10 distinct topics after resume");
        }

        // Only the missing 7 should have been re-fetched after resume.
        assert_eq!(
            report.files_written, 7,
            "resume must not re-write the 3 already-on-disk topics"
        );
        drop(_guard);
    }

    #[tokio::test]
    async fn test_multi_page_comments_merged() {
        let _serial = SYNC_SERIAL.lock().unwrap();
        let topic_id = 2_000_001u64;
        let server = MockServer::start().await;

        // Forum listing with one topic.
        stub_forum_listing(&server, "252", &[topic_id]).await;

        // Topic page 1 (start missing): posts 101,102,103 + pagination hint total=2.
        let page1 = build_multi_page_topic_html(topic_id, 0, 2, &[101, 102, 103]);
        Mock::given(method("GET"))
            .and(path("/forum/viewtopic.php"))
            .and(query_param("t", topic_id.to_string().as_str()))
            .and(query_param_is_missing("start"))
            .respond_with(ResponseTemplate::new(200).set_body_string(page1))
            .mount(&server)
            .await;
        // Topic page 2 (start=30): posts 104,105,106.
        let page2 = build_multi_page_topic_html(topic_id, 1, 2, &[104, 105, 106]);
        Mock::given(method("GET"))
            .and(path("/forum/viewtopic.php"))
            .and(query_param("t", topic_id.to_string().as_str()))
            .and(query_param("start", "30"))
            .respond_with(ResponseTemplate::new(200).set_body_string(page2))
            .mount(&server)
            .await;

        let td = TempDir::new().unwrap();
        let mut m = Mirror::init(td.path()).unwrap();
        let mut engine = SyncEngine::new(&mut m, make_client(&server));
        let report = engine.sync_forum("252", fast_opts(10)).await.unwrap();
        assert_eq!(report.files_written, 1);

        let p = td
            .path()
            .join("forums")
            .join("252")
            .join("topics")
            .join(format!("{}.json", topic_id));
        let file: TopicFile = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        let ids: Vec<u64> = file.comments.iter().map(|c| c.post_id).collect();
        assert_eq!(
            ids,
            vec![101, 102, 103, 104, 105, 106],
            "all posts merged, sorted ascending, deduped"
        );
        assert!(
            file.last_post_id >= 106,
            "last_post_id must reflect max comment id"
        );
    }

    // ────────────────────────── M6 tests ──────────────────────────

    #[test]
    fn test_rebuild_index_from_json_matches_file_count() {
        let _serial = SYNC_SERIAL.lock().unwrap();
        let td = TempDir::new().unwrap();
        {
            let _m = Mirror::init(td.path()).unwrap();
        }

        // Seed 10 JSON files across two forums (5 each). The index starts empty
        // because we never ran sync, so rebuild must insert all 10.
        let forums = [
            ("252", 1_000_001u64..=1_000_005u64),
            ("251", 2_000_001u64..=2_000_005u64),
        ];
        for (forum_id, range) in &forums {
            let topics_dir = td.path().join("forums").join(forum_id).join("topics");
            std::fs::create_dir_all(&topics_dir).unwrap();
            for tid in range.clone() {
                let tf = TopicFile {
                    schema_version: 1,
                    topic_id: tid.to_string(),
                    forum_id: forum_id.to_string(),
                    title: format!("seed {tid}"),
                    fetched_at: String::new(),
                    last_post_id: 9_000_000 + tid,
                    last_post_at: String::new(),
                    opening_post: Post::default(),
                    comments: Vec::new(),
                    metadata: serde_json::Value::Null,
                    size_bytes: None,
                    seeds: None,
                    leeches: None,
                    downloads: None,
                };
                topic_io::write_json_atomic(&topics_dir.join(format!("{}.json", tid)), &tf)
                    .unwrap();
            }
        }

        // Delete state.db and re-init (simulates a cold-start rebuild after loss).
        std::fs::remove_file(td.path().join("state.db")).unwrap();
        let mut m = Mirror::init(td.path()).unwrap();

        let inserted = super::rebuild_index(&mut m).unwrap();
        assert_eq!(inserted, 10, "expected 10 rows inserted, got {inserted}");

        let count: i64 = m
            .state()
            .conn()
            .query_row("SELECT COUNT(*) FROM topic_index", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 10);
    }

    #[tokio::test]
    async fn test_stop_streak_tolerates_moved_topic() {
        let _serial = SYNC_SERIAL.lock().unwrap();
        let forum_id = "252";
        let td = TempDir::new().unwrap();

        // Seed state: hwm=9_999_999; topic 5_000_000 known with last_post_id=1_005_000_000.
        let known_tid: u64 = 5_000_000;
        let known_lpi: u64 = 1_005_000_000;
        {
            let mut m = Mirror::init(td.path()).unwrap();
            let conn = m.state_mut().conn_mut();
            conn.execute(
                "INSERT INTO forum_state (forum_id, topic_high_water_mark) VALUES (?1, ?2)",
                params![forum_id, "9999999"],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO topic_index (forum_id, topic_id, title, last_post_id, last_post_at, fetched_at) \
                 VALUES (?1, ?2, 'seed', ?3, '', '')",
                params![forum_id, known_tid.to_string(), known_lpi.to_string()],
            )
            .unwrap();

            // Also drop a matching JSON on disk so backfill doesn't insert over us.
            let topics_dir = td.path().join("forums").join(forum_id).join("topics");
            std::fs::create_dir_all(&topics_dir).unwrap();
            let tf = TopicFile {
                schema_version: 1,
                topic_id: known_tid.to_string(),
                forum_id: forum_id.to_string(),
                title: "seed".into(),
                fetched_at: String::new(),
                last_post_id: known_lpi,
                last_post_at: String::new(),
                opening_post: Post::default(),
                comments: Vec::new(),
                metadata: serde_json::Value::Null,
                size_bytes: None,
                seeds: None,
                leeches: None,
                downloads: None,
            };
            topic_io::write_json_atomic(&topics_dir.join(format!("{}.json", known_tid)), &tf)
                .unwrap();
        }

        // Listing: row 0 is older-and-known-matching; rows 1-4 are new (not in index).
        let new_tids: [u64; 4] = [12_000_001, 12_000_002, 12_000_003, 12_000_004];
        let mut rows = vec![(known_tid, known_lpi)];
        for t in &new_tids {
            rows.push((*t, 1_000_000 + t));
        }

        let server = MockServer::start().await;
        stub_listing_body(&server, forum_id, build_forum_listing_html(forum_id, &rows)).await;
        stub_all_topics(&server, &new_tids).await;
        // Also stub the known topic in case it got fetched — verifies we DON'T fetch it
        // (the mock is unused; wiremock is permissive about that).
        stub_all_topics(&server, &[known_tid]).await;

        let mut m = Mirror::open(td.path(), None).unwrap();
        let mut engine = SyncEngine::new(&mut m, make_client(&server));
        let report = engine.sync_forum(forum_id, fast_opts(10)).await.unwrap();

        assert_eq!(report.rows_parsed, 5, "all 5 listing rows processed");
        assert_eq!(report.files_written, 4, "4 new topics fetched");
        assert_eq!(
            report.rows_unchanged, 1,
            "1 short-circuit trigger (the older+known row)"
        );
    }

    #[test]
    fn test_jittered_delay_is_nonuniform() {
        let opts = SyncOpts {
            min_delay_ms: 100,
            max_delay_ms: 500,
            rng_seed: Some(42),
            ..fast_opts(10)
        };
        let mut rng = rand::rngs::StdRng::seed_from_u64(opts.rng_seed.unwrap());
        let delays: Vec<u64> = (0..10).map(|_| next_delay_ms(&opts, &mut rng)).collect();

        assert!(delays.iter().all(|ms| (100..=500).contains(ms)));
        let distinct: std::collections::BTreeSet<u64> = delays.iter().copied().collect();
        assert!(
            distinct.len() >= 2,
            "expected at least 2 distinct jitter values, got {delays:?}"
        );
    }

    #[tokio::test]
    async fn test_referer_set_on_topic_fetch() {
        let _serial = SYNC_SERIAL.lock().unwrap();
        let server = MockServer::start().await;
        let topic_id = 6_252_001u64;
        let expected_referer = format!("{}/forum/viewforum.php?f=252", server.uri());

        stub_forum_listing(&server, "252", &[topic_id]).await;
        Mock::given(method("GET"))
            .and(path("/forum/viewtopic.php"))
            .and(query_param("t", topic_id.to_string().as_str()))
            .and(header("referer", expected_referer.as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_string(build_topic_html(topic_id)))
            .mount(&server)
            .await;

        let td = TempDir::new().unwrap();
        let mut m = Mirror::init(td.path()).unwrap();
        let mut engine = SyncEngine::new(&mut m, make_client(&server));
        let report = engine.sync_forum("252", fast_opts(1)).await.unwrap();
        assert_eq!(report.files_written, 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_reading_pause_every_n_topics() {
        let _serial = SYNC_SERIAL.lock().unwrap();
        let server = MockServer::start().await;
        let topic_ids: Vec<u64> = (6_300_001..=6_300_009).collect();
        stub_forum_listing(&server, "252", &topic_ids).await;
        stub_all_topics(&server, &topic_ids).await;

        let td = TempDir::new().unwrap();
        let mut m = Mirror::init(td.path()).unwrap();
        let mut engine = SyncEngine::new(&mut m, make_client(&server));

        let events = Arc::new(Mutex::new(Vec::new()));
        let subscriber = Registry::default().with(EventCaptureLayer {
            events: events.clone(),
        });
        let _guard = tracing::subscriber::set_default(subscriber);

        let report = engine
            .sync_forum(
                "252",
                SyncOpts {
                    pause_every_n: 3,
                    pause_min_secs: 0,
                    pause_max_secs: 0,
                    ..fast_opts(9)
                },
            )
            .await
            .unwrap();

        assert_eq!(report.files_written, 9);
        let reading_pause_count = events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.as_str() == "reading_pause")
            .count();
        assert_eq!(reading_pause_count, 3);
    }

    #[tokio::test]
    async fn test_legacy_rate_rps_still_works() {
        let opts = SyncOpts {
            rate_rps: 10.0,
            min_delay_ms: 0,
            max_delay_ms: 0,
            ..fast_opts(1)
        };
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let start = tokio::time::Instant::now();
        sleep_jittered(&opts, &mut rng).await;
        let elapsed = start.elapsed();

        assert!(
            (80..=120).contains(&(elapsed.as_millis() as u64)),
            "expected ~100ms legacy delay, got {elapsed:?}"
        );
    }
}
