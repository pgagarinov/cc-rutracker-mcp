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
use std::path::PathBuf;

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

pub mod prelude {
    pub use super::{
        run_browse, run_categories, run_download, run_search, run_topic, BrowseArgs, CliConfig,
        DownloadArgs, OutputFormat, SearchArgs, TopicArgs,
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
