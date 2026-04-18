//! `SyncEngine` — per-forum sync loop.
//!
//! **M4**: initial bulk fetch, `.lock` guard, forum-pass transaction boundary,
//! 429/503 → 1 h cooldown, parser-sanity abort, `RUTRACKER_MIRROR_FAIL_AFTER_N_TOPICS`
//! test-only injection.
//!
//! **M5**: delta detection via `topic_index`, 5-consecutive-older-and-known stop-streak
//! (§5.2 / architect R1), multi-page comment merge with "commit only after all pages"
//! semantics (§5.3), and crash-resumability by running `backfill_missing_index_rows`
//! before every sync (§4.2).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::Duration;

use chrono::Utc;
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

#[derive(Debug, Clone)]
pub struct SyncOpts {
    pub max_topics: usize,
    pub max_pages: usize,
    pub rate_rps: f32,
}

impl Default for SyncOpts {
    fn default() -> Self {
        Self {
            max_topics: 500,
            max_pages: 100,
            rate_rps: 1.0,
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
        let _lock = MirrorLock::acquire(self.mirror.root())?;

        let topics_dir = self.mirror.forum_topics_dir(forum_id);
        std::fs::create_dir_all(&topics_dir)?;

        // §4.2 recovery: if a prior run crashed between a JSON write and the SQLite
        // commit, the topic_index is missing rows but the JSONs are on disk. Re-insert
        // any absent rows now so delta detection below sees the full known set.
        self.mirror.backfill_missing_index_rows(forum_id)?;

        let client = self.client.clone();
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

        let html = match client
            .get_text(
                urls::VIEWFORUM_PHP,
                &[
                    ("f", forum_id),
                    ("sort", "registered"),
                    ("order", "desc"),
                    ("start", "0"),
                ],
            )
            .await
        {
            Ok(h) => h,
            Err(rutracker_http::Error::Status(s)) if is_rate_limit(s.as_u16()) => {
                tx.execute(
                    "UPDATE forum_state SET last_sync_outcome='rate_limited', cooldown_until=?1 \
                     WHERE forum_id=?2",
                    params![cooldown_iso(), forum_id],
                )?;
                tx.commit()?;
                return Ok(SyncReport {
                    forums_rate_limited: 1,
                    ..Default::default()
                });
            }
            Err(e) => return Err(Error::Http(e)),
        };

        let listing = parse_forum_page(&html)?;

        let mut written = 0usize;
        let mut rows_parsed = 0usize;
        let mut rows_unchanged = 0usize;
        let mut high = hwm;
        let mut streak: u32 = 0;

        for (i, row) in listing.topics.iter().take(opts.max_topics).enumerate() {
            rows_parsed += 1;
            high = high.max(row.topic_id);

            let known = idx_map.get(&row.topic_id).copied();
            let older = row.topic_id < hwm;

            if older && matches!(known, Some(k) if k == row.last_post_id) {
                rows_unchanged += 1;
                streak += 1;
                if streak >= STOP_STREAK {
                    break;
                }
                continue;
            }
            streak = 0;

            // Known but not-older-than-hwm and last_post_id unchanged: nothing to do.
            if matches!(known, Some(k) if k >= row.last_post_id) {
                rows_unchanged += 1;
                continue;
            }

            if i > 0 && opts.rate_rps > 0.0 {
                tokio::time::sleep(Duration::from_secs_f32(1.0 / opts.rate_rps)).await;
            }

            let td = match fetch_topic_all_pages(&client, row.topic_id, opts.rate_rps).await {
                Ok(td) => td,
                Err(FetchErr::RateLimited) => {
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
            idx_map.insert(row.topic_id, file.last_post_id);
            maybe_inject_panic(written);
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
    topic_id: u64,
    rate_rps: f32,
) -> std::result::Result<rutracker_parser::TopicDetails, FetchErr> {
    let tid = topic_id.to_string();
    let html = client_get(client, &[("t", tid.as_str())]).await?;
    let mut td = parse_topic_page(&html).map_err(|e| FetchErr::Other(Error::Parser(e)))?;

    let total = td.comment_pages_total;
    if total > 1 {
        for page in 1..total {
            if rate_rps > 0.0 {
                tokio::time::sleep(Duration::from_secs_f32(1.0 / rate_rps)).await;
            }
            let start = (page * COMMENTS_PER_PAGE).to_string();
            let html =
                client_get(client, &[("t", tid.as_str()), ("start", start.as_str())]).await?;
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
) -> std::result::Result<String, FetchErr> {
    match client.get_text(urls::VIEWTOPIC_PHP, params).await {
        Ok(h) => Ok(h),
        Err(rutracker_http::Error::Status(s)) if is_rate_limit(s.as_u16()) => {
            Err(FetchErr::RateLimited)
        }
        Err(e) => Err(FetchErr::Other(Error::Http(e))),
    }
}

fn is_rate_limit(code: u16) -> bool {
    matches!(code, 429 | 503)
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
    }
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
    use std::fmt::Write as _;
    use std::sync::Mutex;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // Serialises every test that runs `sync_forum` (or the panic-injection variants).
    // `RUTRACKER_MIRROR_FAIL_AFTER_N_TOPICS` is process-global; letting tests run in
    // parallel would leak the injected panic threshold across them.
    static SYNC_SERIAL: Mutex<()> = Mutex::new(());

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
            rate_rps: 1000.0,
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
}
