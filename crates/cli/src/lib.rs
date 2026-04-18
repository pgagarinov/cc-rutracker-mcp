//! rutracker-cli — library of command handlers.
//!
//! The `rutracker` binary in `main.rs` parses CLI args and dispatches into this library.
//! Exposing the handlers as ordinary async functions lets us fixture-test them with
//! `wiremock` without spawning a subprocess.

use anyhow::{anyhow, Context, Result};
use rutracker_http::urls;
use rutracker_http::Client;
use rutracker_parser::{
    forum_index::parse_forum_index, search::parse_search_page, text_format,
    topic::parse_topic_page, CategoryGroup, SearchPage, TopicDetails,
};
use serde::Serialize;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tracing::field::{Field, Visit};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::layer::{Context as LayerContext, SubscriberExt};
use tracing_subscriber::{filter, fmt, Layer, Registry};

pub mod paths;

#[derive(Debug, Clone, Copy)]
pub enum OutputFormat {
    Json,
    Text,
}

/// Runtime configuration — built from CLI args or tests.
#[derive(Debug, Clone)]
pub struct CliConfig {
    pub base_url: String,
    pub format: OutputFormat,
    pub out: Option<PathBuf>,
    pub cookies: std::collections::HashMap<String, String>,
    /// When true, `emit()` writes to stdout if no `out` file is set. MCP callers set
    /// this to `false` to prevent the CLI-rendered text from contaminating the
    /// JSON-RPC stream on stdout.
    pub emit_stdout: bool,
}

impl CliConfig {
    pub fn client(&self) -> Result<Client> {
        let mut c = Client::new(&self.base_url)?;
        for (k, v) in &self.cookies {
            c.set_cookie(k.clone(), v.clone());
        }
        Ok(c)
    }
}

/// Write `output` to `cfg.out` on disk if set. Writes to stdout only when
/// `cfg.emit_stdout` is true. Returns the destination for tests.
pub async fn emit(cfg: &CliConfig, output: &str) -> Result<Option<PathBuf>> {
    if let Some(path) = &cfg.out {
        tokio::fs::write(path, output)
            .await
            .with_context(|| format!("writing {}", path.display()))?;
        return Ok(Some(path.clone()));
    }
    if cfg.emit_stdout {
        print!("{}", output);
    }
    Ok(None)
}

fn render<T: Serialize>(cfg: &CliConfig, value: &T, text: &str) -> Result<String> {
    match cfg.format {
        OutputFormat::Json => Ok(serde_json::to_string_pretty(value)? + "\n"),
        OutputFormat::Text => Ok(text.to_string() + "\n"),
    }
}

// -------- search --------

#[derive(Debug, Clone, Default)]
pub struct SearchArgs {
    pub query: String,
    pub category: Option<String>,
    pub sort_by: String, // seeders | size | downloads | registered
    pub order: String,   // desc | asc
    pub page: u32,
}

impl SearchArgs {
    pub fn to_query_pairs(&self) -> Vec<(String, String)> {
        let sort_map = [
            ("seeders", "10"),
            ("size", "7"),
            ("downloads", "4"),
            ("registered", "1"),
        ];
        let o = sort_map
            .iter()
            .find(|(k, _)| *k == self.sort_by)
            .map(|(_, v)| *v)
            .unwrap_or("10");
        let s = if self.order == "asc" { "1" } else { "2" };
        let mut pairs = vec![
            ("nm".to_string(), self.query.clone()),
            ("o".to_string(), o.to_string()),
            ("s".to_string(), s.to_string()),
        ];
        if let Some(cat) = &self.category {
            pairs.push(("f".to_string(), cat.clone()));
        }
        if self.page > 1 {
            let start = (self.page - 1) * 50;
            pairs.push(("start".to_string(), start.to_string()));
        }
        pairs
    }
}

pub async fn run_search(cfg: &CliConfig, args: &SearchArgs) -> Result<String> {
    let client = cfg.client()?;
    let pairs: Vec<(String, String)> = args.to_query_pairs();
    let borrowed: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let html = client.get_text(urls::TRACKER_PHP, &borrowed).await?;
    let page: SearchPage = parse_search_page(&html)?;

    let text = text_format::format_search_legacy(&page.results);
    let out = render(cfg, &page, &text)?;
    emit(cfg, &out).await?;
    Ok(out)
}

// -------- topic --------

#[derive(Debug, Clone, Default)]
pub struct TopicArgs {
    pub topic_id: u64,
    pub include_comments: bool,
    pub max_comment_pages: u32,
}

pub async fn run_topic(cfg: &CliConfig, args: &TopicArgs) -> Result<String> {
    let client = cfg.client()?;
    let tid = args.topic_id.to_string();
    let html = client
        .get_text(urls::VIEWTOPIC_PHP, &[("t", tid.as_str())])
        .await?;
    let td: TopicDetails = parse_topic_page(&html)?;
    // Phase 2 already populates comments from page 1; `max_comment_pages > 1` is Phase 4.5.
    let text = text_format::format_topic_legacy(&td);
    let out = render(cfg, &td, &text)?;
    emit(cfg, &out).await?;
    Ok(out)
}

// -------- browse --------

#[derive(Debug, Clone, Default)]
pub struct BrowseArgs {
    pub category_id: String,
    pub page: u32,
    pub sort_by: String,
    pub order: String,
}

pub async fn run_browse(cfg: &CliConfig, args: &BrowseArgs) -> Result<String> {
    let client = cfg.client()?;
    let sort_args = SearchArgs {
        query: String::new(),
        category: Some(args.category_id.clone()),
        sort_by: args.sort_by.clone(),
        order: args.order.clone(),
        page: args.page,
    };
    let pairs: Vec<(String, String)> = sort_args.to_query_pairs();
    let borrowed: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let html = client.get_text(urls::TRACKER_PHP, &borrowed).await?;
    let page: SearchPage = parse_search_page(&html)?;
    let text = text_format::format_search_legacy(&page.results);
    let out = render(cfg, &page, &text)?;
    emit(cfg, &out).await?;
    Ok(out)
}

// -------- categories --------

pub async fn run_categories(cfg: &CliConfig) -> Result<String> {
    let client = cfg.client()?;
    let html = client.get_text(urls::INDEX_PHP, &[]).await?;
    let groups: Vec<CategoryGroup> = parse_forum_index(&html)?;
    let text = format_categories_text(&groups);
    let out = render(cfg, &groups, &text)?;
    emit(cfg, &out).await?;
    Ok(out)
}

fn format_categories_text(groups: &[CategoryGroup]) -> String {
    let mut s = String::new();
    for g in groups {
        s.push_str(&format!("[{}] {}\n", g.group_id, g.title));
        for f in &g.forums {
            s.push_str(&format!("  [{}] {}\n", f.forum_id, f.name));
        }
    }
    s
}

// -------- download --------

#[derive(Debug, Clone)]
pub struct DownloadArgs {
    pub topic_id: u64,
    pub out_dir: PathBuf,
    pub allow_path: bool,
}

pub async fn run_download(cfg: &CliConfig, args: &DownloadArgs) -> Result<PathBuf> {
    paths::validate_out_dir(&args.out_dir, args.allow_path)?;
    let client = cfg.client()?;
    let tid = args.topic_id.to_string();
    let bytes = client
        .get_bytes(urls::DL_PHP, &[("t", tid.as_str())])
        .await
        .map_err(|e| anyhow!("download failed: {e}"))?;

    let filename = format!("topic-{}.torrent", args.topic_id);
    let path = args.out_dir.join(filename);
    tokio::fs::create_dir_all(&args.out_dir).await?;
    tokio::fs::write(&path, &bytes).await?;
    Ok(path)
}

// -------- mirror watchlist --------

#[derive(Debug, Clone)]
pub struct WatchArgs {
    pub forum_id: String,
    /// Override the mirror root. `None` means `rutracker_mirror::default_root()`.
    pub root: Option<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct WatchListArgs {
    pub root: Option<PathBuf>,
}

fn mirror_root(root: Option<&PathBuf>) -> PathBuf {
    root.cloned().unwrap_or_else(rutracker_mirror::default_root)
}

pub async fn run_watch_add(cfg: &CliConfig, args: &WatchArgs) -> Result<String> {
    let root = mirror_root(args.root.as_ref());
    let structure_path = root.join("structure.json");
    let structure_bytes = std::fs::read(&structure_path).with_context(|| {
        format!(
            "reading {} — run `rutracker mirror init` and `mirror structure` first",
            structure_path.display()
        )
    })?;
    let structure: rutracker_mirror::structure::Structure =
        serde_json::from_slice(&structure_bytes)?;

    let mut wl = rutracker_mirror::watchlist::load(&root)?;
    rutracker_mirror::watchlist::add(&mut wl, &structure, &args.forum_id)?;
    rutracker_mirror::watchlist::save(&root, &wl)?;

    let text = format_watchlist_text(&wl);
    let out = render(cfg, &wl, &text)?;
    emit(cfg, &out).await?;
    Ok(out)
}

pub async fn run_watch_remove(cfg: &CliConfig, args: &WatchArgs) -> Result<String> {
    let root = mirror_root(args.root.as_ref());
    let mut wl = rutracker_mirror::watchlist::load(&root)?;
    rutracker_mirror::watchlist::remove(&mut wl, &args.forum_id);
    rutracker_mirror::watchlist::save(&root, &wl)?;

    let text = format_watchlist_text(&wl);
    let out = render(cfg, &wl, &text)?;
    emit(cfg, &out).await?;
    Ok(out)
}

pub async fn run_watch_list(cfg: &CliConfig, args: &WatchListArgs) -> Result<String> {
    let root = mirror_root(args.root.as_ref());
    let wl = rutracker_mirror::watchlist::load(&root)?;

    let text = format_watchlist_text(&wl);
    let out = render(cfg, &wl, &text)?;
    emit(cfg, &out).await?;
    Ok(out)
}

fn format_watchlist_text(wl: &rutracker_mirror::config::Watchlist) -> String {
    if wl.forums.is_empty() {
        return "(watchlist empty)".to_string();
    }
    let mut s = String::new();
    for e in &wl.forums {
        s.push_str(&format!(
            "[{}] {} (added {})\n",
            e.forum_id, e.name, e.added_at
        ));
    }
    s
}

// -------- mirror init / structure / sync / show / status / rebuild-index --------

#[derive(Debug, Clone, Default)]
pub struct MirrorRootArgs {
    pub root: Option<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct SyncCliArgs {
    pub root: Option<PathBuf>,
    /// Forum ids to sync. Empty ⇒ use the watchlist.
    pub forums: Vec<String>,
    pub max_topics: usize,
    pub rate_rps: f32,
    pub max_attempts_per_forum: u32,
    pub cooldown_wait: bool,
    pub log_file: Option<String>,
    /// Walk the full forum listing, ignoring stop-streak. For resuming partial bulk fetches.
    pub force_full: bool,
}

#[derive(Debug, Clone)]
pub struct MirrorSyncResult {
    pub output: String,
    pub exit_code: i32,
}

#[derive(Clone)]
struct SharedWriter {
    inner: Arc<Mutex<Box<dyn Write + Send>>>,
}

struct JsonVisitor {
    fields: serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone, Copy, Default)]
struct Rfc3339Timer;

struct NdjsonLayer {
    writer: SharedWriter,
}

impl SharedWriter {
    fn new(writer: Box<dyn Write + Send>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(writer)),
        }
    }

    fn write_line(&self, line: &str) {
        if let Ok(mut writer) = self.inner.lock() {
            let _ = writer.write_all(line.as_bytes());
            let _ = writer.write_all(b"\n");
            let _ = writer.flush();
        }
    }
}

impl Default for JsonVisitor {
    fn default() -> Self {
        Self {
            fields: serde_json::Map::new(),
        }
    }
}

impl Visit for JsonVisitor {
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields
            .insert(field.name().to_string(), serde_json::Value::Bool(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields.insert(field.name().to_string(), value.into());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields.insert(field.name().to_string(), value.into());
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.fields.insert(
            field.name().to_string(),
            serde_json::Number::from_f64(value)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
        );
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields.insert(
            field.name().to_string(),
            serde_json::Value::String(value.to_string()),
        );
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields.insert(
            field.name().to_string(),
            serde_json::Value::String(format!("{value:?}").trim_matches('"').to_string()),
        );
    }
}

impl FormatTime for Rfc3339Timer {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        write!(w, "{ts}")
    }
}

impl NdjsonLayer {
    fn new(writer: SharedWriter) -> Self {
        Self { writer }
    }
}

impl<S> Layer<S> for NdjsonLayer
where
    S: tracing::Subscriber,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: LayerContext<'_, S>) {
        let metadata = event.metadata();
        let mut visitor = JsonVisitor::default();
        event.record(&mut visitor);
        visitor.fields.insert(
            "ts".to_string(),
            serde_json::Value::String(
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            ),
        );
        visitor.fields.insert(
            "level".to_string(),
            serde_json::Value::String(metadata.level().to_string().to_lowercase()),
        );
        visitor.fields.insert(
            "target".to_string(),
            serde_json::Value::String(metadata.target().to_string()),
        );
        let line = serde_json::to_string(&visitor.fields).unwrap_or_else(|_| {
            r#"{"ts":"","level":"error","event":"json_encode_failed"}"#.to_string()
        });
        self.writer.write_line(&line);
    }
}

fn is_sync_target(target: &str) -> bool {
    matches!(
        target,
        "rutracker_mirror::sync" | "rutracker_mirror::driver"
    )
}

fn sync_filter() -> filter::FilterFn<impl Fn(&tracing::Metadata<'_>) -> bool + Clone> {
    filter::filter_fn(|metadata| is_sync_target(metadata.target()))
}

fn default_sync_log_path(root: &std::path::Path) -> PathBuf {
    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    root.join("logs").join(format!("sync-{ts}.log"))
}

fn resolve_log_writer(
    root: &std::path::Path,
    log_file: Option<&str>,
) -> Result<Option<SharedWriter>> {
    match log_file {
        Some("") => Ok(None),
        Some("-") => Ok(Some(SharedWriter::new(Box::new(io::stderr())))),
        Some(path) => {
            let path = PathBuf::from(path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let file = std::fs::File::create(path)?;
            Ok(Some(SharedWriter::new(Box::new(file))))
        }
        None => {
            let path = default_sync_log_path(root);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let file = std::fs::File::create(path)?;
            Ok(Some(SharedWriter::new(Box::new(file))))
        }
    }
}

fn build_sync_dispatch(
    root: &std::path::Path,
    log_file: Option<&str>,
) -> Result<tracing::Dispatch> {
    let human_layer = fmt::layer()
        .compact()
        .with_ansi(false)
        .with_timer(Rfc3339Timer)
        .with_writer(io::stdout)
        .with_filter(sync_filter());
    let file_layer = resolve_log_writer(root, log_file)?
        .map(NdjsonLayer::new)
        .map(|layer| layer.with_filter(sync_filter()));
    let subscriber = Registry::default().with(human_layer).with(file_layer);
    Ok(tracing::Dispatch::new(subscriber))
}

#[derive(Debug, Clone)]
pub struct ShowArgs {
    pub root: Option<PathBuf>,
    pub forum_id: String,
    pub topic_id: String,
}

pub async fn run_mirror_init(cfg: &CliConfig, args: &MirrorRootArgs) -> Result<String> {
    let root = mirror_root(args.root.as_ref());
    rutracker_mirror::Mirror::init(&root)?;
    let payload = serde_json::json!({
        "initialized": true,
        "root": root.display().to_string(),
    });
    let text = format!("initialized mirror at {}\n", root.display());
    let out = render(cfg, &payload, &text)?;
    emit(cfg, &out).await?;
    Ok(out)
}

pub async fn run_mirror_structure(cfg: &CliConfig, args: &MirrorRootArgs) -> Result<String> {
    let root = mirror_root(args.root.as_ref());
    let client = cfg.client()?;
    let structure = rutracker_mirror::structure::refresh_structure(&root, &client).await?;
    let text = format!(
        "structure.json written ({} groups)\n",
        structure.groups.len()
    );
    let out = render(cfg, &structure, &text)?;
    emit(cfg, &out).await?;
    Ok(out)
}

pub async fn run_mirror_sync(cfg: &CliConfig, args: &SyncCliArgs) -> Result<MirrorSyncResult> {
    let root = mirror_root(args.root.as_ref());
    let dispatch = build_sync_dispatch(&root, args.log_file.as_deref())?;
    let _guard = tracing::dispatcher::set_default(&dispatch);
    let client = cfg.client()?;
    let mut mirror = rutracker_mirror::Mirror::open(&root, Some(client.clone()))?;

    let forum_ids: Vec<String> = if args.forums.is_empty() {
        let wl = rutracker_mirror::watchlist::load(&root)?;
        wl.forums.iter().map(|e| e.forum_id.clone()).collect()
    } else {
        args.forums.clone()
    };

    let opts = rutracker_mirror::engine::SyncOpts {
        max_topics: args.max_topics,
        max_pages: 100,
        rate_rps: args.rate_rps,
        max_attempts_per_forum: args.max_attempts_per_forum,
        cooldown_wait: args.cooldown_wait,
        force_full: args.force_full,
        ..Default::default()
    };

    let mut driver = rutracker_mirror::SyncDriver::new(&mut mirror, client);
    let summary = driver.run_until_done_all(&forum_ids, opts).await?;
    let exit_code = if summary.forums_failed.is_empty() {
        0
    } else {
        1
    };

    let payload = serde_json::json!({
        "forums_ok": summary.forums_ok.iter().map(|forum| serde_json::json!({
            "forum_id": forum.forum_id,
            "topics_count": forum.topics_count,
            "attempts": forum.attempts,
            "gave_up": forum.gave_up,
        })).collect::<Vec<_>>(),
        "forums_failed": summary.forums_failed.iter().map(|forum| serde_json::json!({
            "forum_id": forum.forum_id,
            "topics_count": forum.topics_count,
            "attempts": forum.attempts,
            "gave_up": forum.gave_up,
        })).collect::<Vec<_>>(),
    });
    let mut text = String::new();
    for forum in &summary.forums_ok {
        text.push_str(&format!(
            "{}: topics={} attempts={} status=ok\n",
            forum.forum_id, forum.topics_count, forum.attempts
        ));
    }
    for forum in &summary.forums_failed {
        text.push_str(&format!(
            "{}: topics={} attempts={} status=gave_up\n",
            forum.forum_id, forum.topics_count, forum.attempts
        ));
    }
    let out = render(cfg, &payload, &text)?;
    emit(cfg, &out).await?;
    Ok(MirrorSyncResult {
        output: out,
        exit_code,
    })
}

pub async fn run_mirror_show(cfg: &CliConfig, args: &ShowArgs) -> Result<String> {
    let root = mirror_root(args.root.as_ref());
    let path = root
        .join("forums")
        .join(&args.forum_id)
        .join("topics")
        .join(format!("{}.json", args.topic_id));
    let bytes =
        std::fs::read(&path).with_context(|| format!("reading topic file {}", path.display()))?;
    let file: rutracker_mirror::topic_io::TopicFile = serde_json::from_slice(&bytes)?;

    let text = format!(
        "{} (forum {}, topic {})\nlast_post_id={} last_post_at={}\ncomments={}\n",
        file.title,
        file.forum_id,
        file.topic_id,
        file.last_post_id,
        file.last_post_at,
        file.comments.len(),
    );
    let out = render(cfg, &file, &text)?;
    emit(cfg, &out).await?;
    Ok(out)
}

pub async fn run_mirror_status(cfg: &CliConfig, args: &MirrorRootArgs) -> Result<String> {
    let root = mirror_root(args.root.as_ref());
    let mirror = rutracker_mirror::Mirror::open(&root, None)?;
    let now = chrono::Utc::now();

    #[derive(Serialize)]
    struct ForumStatus {
        forum_id: String,
        topics_count: i64,
        last_sync_started_at: Option<String>,
        last_sync_completed_at: Option<String>,
        last_sync_outcome: Option<String>,
        cooldown_until: Option<String>,
        cooldown_seconds_remaining: i64,
    }

    let mut forums: Vec<ForumStatus> = Vec::new();
    let conn = mirror.state().conn();
    let mut stmt = conn.prepare(
        "SELECT forum_id, last_sync_started_at, last_sync_completed_at, last_sync_outcome, \
                cooldown_until \
         FROM forum_state ORDER BY forum_id",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    for (forum_id, started, completed, outcome, cooldown_until) in rows {
        let topics_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM topic_index WHERE forum_id = ?1",
            [&forum_id],
            |r| r.get(0),
        )?;
        let cooldown_seconds_remaining = cooldown_until
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| (dt.with_timezone(&chrono::Utc) - now).num_seconds().max(0))
            .unwrap_or(0);
        forums.push(ForumStatus {
            forum_id,
            topics_count,
            last_sync_started_at: started,
            last_sync_completed_at: completed,
            last_sync_outcome: outcome,
            cooldown_until,
            cooldown_seconds_remaining,
        });
    }

    let payload = serde_json::json!({ "forums": forums });
    let mut text = String::new();
    for f in &forums {
        text.push_str(&format!(
            "{}: topics={} outcome={} cooldown_remaining={}s\n",
            f.forum_id,
            f.topics_count,
            f.last_sync_outcome.as_deref().unwrap_or("-"),
            f.cooldown_seconds_remaining,
        ));
    }
    let out = render(cfg, &payload, &text)?;
    emit(cfg, &out).await?;
    Ok(out)
}

pub async fn run_mirror_rebuild_index(cfg: &CliConfig, args: &MirrorRootArgs) -> Result<String> {
    let root = mirror_root(args.root.as_ref());
    let mut mirror = rutracker_mirror::Mirror::open(&root, None)?;
    let inserted = rutracker_mirror::engine::rebuild_index(&mut mirror)?;
    let payload = serde_json::json!({ "inserted": inserted });
    let text = format!("rebuilt topic_index: {} rows\n", inserted);
    let out = render(cfg, &payload, &text)?;
    emit(cfg, &out).await?;
    Ok(out)
}

pub mod prelude {
    pub use super::{
        run_browse, run_categories, run_download, run_mirror_init, run_mirror_rebuild_index,
        run_mirror_show, run_mirror_status, run_mirror_structure, run_mirror_sync, run_search,
        run_topic, run_watch_add, run_watch_list, run_watch_remove, BrowseArgs, CliConfig,
        DownloadArgs, MirrorRootArgs, MirrorSyncResult, OutputFormat, SearchArgs, ShowArgs,
        SyncCliArgs, TopicArgs, WatchArgs, WatchListArgs,
    };
}

// ---------- tests ----------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const FORUM_FIXTURE: &[u8] = include_bytes!("../../parser/tests/fixtures/forum-sample.html");
    const TOPIC_FIXTURE: &[u8] = include_bytes!("../../parser/tests/fixtures/topic-sample.html");
    const INDEX_FIXTURE: &[u8] = include_bytes!("../../parser/tests/fixtures/index-sample.html");

    fn cp1251_body(bytes: &[u8]) -> Vec<u8> {
        // Fixtures are already cp1251 bytes on disk.
        bytes.to_vec()
    }

    fn config_for(server: &MockServer, format: OutputFormat, out: Option<PathBuf>) -> CliConfig {
        CliConfig {
            base_url: format!("{}/forum/", server.uri()),
            format,
            out,
            cookies: HashMap::new(),
            emit_stdout: false, // tests never want stdout noise
        }
    }

    #[tokio::test]
    async fn test_emit_respects_emit_stdout_false() {
        // Regression test for architect finding M1: MCP mode must NOT write to stdout.
        let cfg = CliConfig {
            base_url: "https://example.test/forum/".into(),
            format: OutputFormat::Text,
            out: None,
            cookies: HashMap::new(),
            emit_stdout: false,
        };
        let result = emit(&cfg, "should-not-appear").await.unwrap();
        assert!(result.is_none()); // no file, and no stdout write either
    }

    #[tokio::test]
    async fn test_search_json_parseable() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/forum/tracker.php"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(cp1251_body(FORUM_FIXTURE)))
            .mount(&server)
            .await;
        let cfg = config_for(&server, OutputFormat::Json, None);
        let output = run_search(
            &cfg,
            &SearchArgs {
                query: "2026".into(),
                category: Some("252".into()),
                sort_by: "seeders".into(),
                order: "desc".into(),
                page: 1,
            },
        )
        .await
        .unwrap();
        let json: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(json["results"].as_array().unwrap().len(), 50);
        assert!(json["search_id"].is_string());
    }

    #[tokio::test]
    async fn test_topic_json_has_29_comments() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/forum/viewtopic.php"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(cp1251_body(TOPIC_FIXTURE)))
            .mount(&server)
            .await;
        let tmp = tempdir_unique("topic-json");
        let out = tmp.join("topic.json");
        let cfg = config_for(&server, OutputFormat::Json, Some(out.clone()));
        run_topic(
            &cfg,
            &TopicArgs {
                topic_id: 6843582,
                include_comments: true,
                max_comment_pages: 1,
            },
        )
        .await
        .unwrap();
        let contents = tokio::fs::read_to_string(&out).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(v["comments"].as_array().unwrap().len(), 29);
        assert!(v["metadata"]["year"].as_u64().is_some());
    }

    #[tokio::test]
    async fn test_browse_text_mode() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/forum/tracker.php"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(cp1251_body(FORUM_FIXTURE)))
            .mount(&server)
            .await;
        let cfg = config_for(&server, OutputFormat::Text, None);
        let out = run_browse(
            &cfg,
            &BrowseArgs {
                category_id: "252".into(),
                page: 1,
                sort_by: "seeders".into(),
                order: "desc".into(),
            },
        )
        .await
        .unwrap();
        assert!(out.starts_with("Found 50 results:\n\n"));
    }

    #[tokio::test]
    async fn test_categories_text_mode() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/forum/index.php"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(cp1251_body(INDEX_FIXTURE)))
            .mount(&server)
            .await;
        let cfg = config_for(&server, OutputFormat::Text, None);
        let out = run_categories(&cfg).await.unwrap();
        assert!(
            out.contains("[c-"),
            "categories text should contain [c-… group ids"
        );
    }

    #[tokio::test]
    async fn test_download_writes_file() {
        let server = MockServer::start().await;
        let torrent_bytes = b"d8:announce...fake .torrent body...e";
        Mock::given(method("GET"))
            .and(path("/forum/dl.php"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(torrent_bytes.to_vec())
                    .insert_header("content-type", "application/x-bittorrent"),
            )
            .mount(&server)
            .await;
        let tmp = tempdir_unique("dl-under-home");
        let cfg = config_for(&server, OutputFormat::Json, None);
        let path = run_download(
            &cfg,
            &DownloadArgs {
                topic_id: 12345,
                out_dir: tmp.clone(),
                allow_path: true, // sandbox dir likely outside $HOME in CI
            },
        )
        .await
        .unwrap();
        let content = tokio::fs::read(&path).await.unwrap();
        assert_eq!(content, torrent_bytes);
    }

    #[tokio::test]
    async fn test_path_policy_rejects_etc() {
        let server = MockServer::start().await;
        let cfg = config_for(&server, OutputFormat::Json, None);
        let err = run_download(
            &cfg,
            &DownloadArgs {
                topic_id: 1,
                out_dir: PathBuf::from("/etc/rutracker-should-never-land-here"),
                allow_path: false,
            },
        )
        .await
        .unwrap_err();
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("path") || msg.contains("allowed"),
            "error should mention path policy: {msg}"
        );
    }

    #[tokio::test]
    async fn test_path_policy_allow_override_ok() {
        // With --allow-path we accept paths outside the sandbox. Still under /tmp so CI is fine.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/forum/dl.php"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"x".to_vec()))
            .mount(&server)
            .await;
        let cfg = config_for(&server, OutputFormat::Json, None);
        let outside =
            std::env::temp_dir().join(format!("rutracker-cli-allow-{}", std::process::id()));
        let path = run_download(
            &cfg,
            &DownloadArgs {
                topic_id: 1,
                out_dir: outside.clone(),
                allow_path: true,
            },
        )
        .await
        .unwrap();
        assert!(path.starts_with(&outside));
    }

    #[tokio::test]
    async fn test_watch_list_json_is_valid_array() {
        let server = MockServer::start().await;
        let tmp = tempdir_unique("watch-list");

        // Seed a minimal structure.json so `add` can resolve names.
        let structure = serde_json::json!({
            "schema_version": 1,
            "groups": [{
                "group_id": "1",
                "title": "Кино",
                "forums": [{
                    "forum_id": "252",
                    "name": "Зарубежные фильмы",
                    "parent_id": null
                }]
            }],
            "fetched_at": null
        });
        std::fs::write(
            tmp.join("structure.json"),
            serde_json::to_vec_pretty(&structure).unwrap(),
        )
        .unwrap();

        let cfg = config_for(&server, OutputFormat::Json, None);
        run_watch_add(
            &cfg,
            &WatchArgs {
                forum_id: "252".into(),
                root: Some(tmp.clone()),
            },
        )
        .await
        .unwrap();

        let out = run_watch_list(
            &cfg,
            &WatchListArgs {
                root: Some(tmp.clone()),
            },
        )
        .await
        .unwrap();

        let json: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        let forums = json["forums"]
            .as_array()
            .expect("forums must be a JSON array");
        assert_eq!(forums.len(), 1);
        assert_eq!(forums[0]["forum_id"].as_str().unwrap(), "252");
    }

    #[tokio::test]
    async fn test_show_prints_topic_title() {
        let server = MockServer::start().await;
        let tmp = tempdir_unique("show-title");
        rutracker_mirror::Mirror::init(&tmp).unwrap();

        // Seed a single topic under forum 252.
        let topics_dir = tmp.join("forums").join("252").join("topics");
        std::fs::create_dir_all(&topics_dir).unwrap();
        let expected_title = "Inception (2010) — seeded";
        let tf = rutracker_mirror::topic_io::TopicFile {
            schema_version: 1,
            topic_id: "6843582".into(),
            forum_id: "252".into(),
            title: expected_title.into(),
            fetched_at: "2026-04-18T12:00:00+00:00".into(),
            last_post_id: 4242,
            last_post_at: "".into(),
            opening_post: rutracker_mirror::topic_io::Post::default(),
            comments: Vec::new(),
            metadata: serde_json::Value::Null,
        };
        rutracker_mirror::topic_io::write_json_atomic(&topics_dir.join("6843582.json"), &tf)
            .unwrap();

        let cfg = config_for(&server, OutputFormat::Json, None);
        let out = run_mirror_show(
            &cfg,
            &ShowArgs {
                root: Some(tmp.clone()),
                forum_id: "252".into(),
                topic_id: "6843582".into(),
            },
        )
        .await
        .unwrap();

        let json: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(json["title"].as_str().unwrap(), expected_title);
        assert_eq!(json["topic_id"].as_str().unwrap(), "6843582");
    }

    #[tokio::test]
    async fn test_status_reports_counts_and_cooldown() {
        let server = MockServer::start().await;
        let tmp = tempdir_unique("status-counts");
        let mut m = rutracker_mirror::Mirror::init(&tmp).unwrap();

        // Seed 5 topic_index rows + a forum_state row with cooldown_until in the future.
        let cooldown_until = (chrono::Utc::now() + chrono::Duration::seconds(59 * 60)).to_rfc3339();
        let conn = m.state_mut().conn_mut();
        conn.execute(
            "INSERT INTO forum_state (forum_id, last_sync_outcome, cooldown_until) \
             VALUES ('252', 'rate_limited', ?1)",
            [&cooldown_until],
        )
        .unwrap();
        for i in 1..=5 {
            conn.execute(
                "INSERT INTO topic_index (forum_id, topic_id, title, last_post_id, last_post_at, fetched_at) \
                 VALUES ('252', ?1, 'seed', ?2, '', '')",
                [&i.to_string(), &(1000 + i).to_string()],
            )
            .unwrap();
        }
        drop(m);

        let cfg = config_for(&server, OutputFormat::Json, None);
        let out = run_mirror_status(
            &cfg,
            &MirrorRootArgs {
                root: Some(tmp.clone()),
            },
        )
        .await
        .unwrap();

        let json: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        let forums = json["forums"].as_array().expect("forums array");
        assert_eq!(forums.len(), 1);
        assert_eq!(forums[0]["topics_count"].as_i64().unwrap(), 5);
        let remaining = forums[0]["cooldown_seconds_remaining"].as_i64().unwrap();
        assert!(
            remaining > 0,
            "cooldown_seconds_remaining must be > 0, got {remaining}"
        );
    }

    fn tempdir_unique(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rutracker-cli-test-{}-{}",
            std::process::id(),
            suffix
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
