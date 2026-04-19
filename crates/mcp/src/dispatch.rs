//! `tools/call` dispatcher — parse tool name + arguments, delegate to `rutracker-cli` handlers.

use anyhow::{anyhow, Result};
use rutracker_cli::prelude::*;
use serde_json::Value;
use std::path::PathBuf;

pub async fn dispatch_tool_call(params: &Value, cfg: &CliConfig) -> Result<String> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("tools/call missing 'name'"))?;
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);

    match name {
        "search" => tool_search(&args, cfg).await,
        "get_topic" => tool_get_topic(&args, cfg).await,
        "browse_forum" => tool_browse_forum(&args, cfg).await,
        "list_categories" => tool_list_categories(cfg).await,
        "download_torrent" => tool_download_torrent(&args, cfg).await,
        other => Err(anyhow!("unknown tool: {other}")),
    }
}

fn arg_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn arg_bool(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(default)
}

fn arg_u64(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(|v| v.as_u64())
}

fn arg_u32_or(args: &Value, key: &str, default: u32) -> u32 {
    args.get(key)
        .and_then(|v| v.as_u64())
        .map(|n| n as u32)
        .unwrap_or(default)
}

async fn tool_search(args: &Value, cfg: &CliConfig) -> Result<String> {
    let query = arg_str(args, "query").ok_or_else(|| anyhow!("search: missing 'query'"))?;
    let search_args = SearchArgs {
        query,
        category: arg_str(args, "category"),
        sort_by: arg_str(args, "sort_by").unwrap_or_else(|| "seeders".into()),
        order: arg_str(args, "order").unwrap_or_else(|| "desc".into()),
        page: arg_u32_or(args, "page", 1),
    };
    // Use a no-side-effects config (no file output, text format).
    let mcp_cfg = CliConfig {
        base_url: cfg.base_url.clone(),
        format: OutputFormat::Text,
        out: None,
        cookies: cfg.cookies.clone(),
        emit_stdout: false,
    };
    let rendered = run_search(&mcp_cfg, &search_args).await?;
    Ok(strip_trailing_newline(&rendered))
}

async fn tool_get_topic(args: &Value, cfg: &CliConfig) -> Result<String> {
    let topic_id =
        arg_u64(args, "topic_id").ok_or_else(|| anyhow!("get_topic: missing 'topic_id'"))?;
    let topic_args = TopicArgs {
        topic_id,
        include_comments: arg_bool(args, "include_comments", false),
        max_comment_pages: arg_u32_or(args, "max_comment_pages", 1),
    };
    let mcp_cfg = CliConfig {
        base_url: cfg.base_url.clone(),
        format: OutputFormat::Text,
        out: None,
        cookies: cfg.cookies.clone(),
        emit_stdout: false,
    };
    let rendered = run_topic(&mcp_cfg, &topic_args).await?;
    Ok(strip_trailing_newline(&rendered))
}

async fn tool_browse_forum(args: &Value, cfg: &CliConfig) -> Result<String> {
    let category_id = arg_str(args, "category_id")
        .ok_or_else(|| anyhow!("browse_forum: missing 'category_id'"))?;
    let browse_args = BrowseArgs {
        category_id,
        page: arg_u32_or(args, "page", 1),
        sort_by: arg_str(args, "sort_by").unwrap_or_else(|| "seeders".into()),
        order: arg_str(args, "order").unwrap_or_else(|| "desc".into()),
    };
    let mcp_cfg = CliConfig {
        base_url: cfg.base_url.clone(),
        format: OutputFormat::Text,
        out: None,
        cookies: cfg.cookies.clone(),
        emit_stdout: false,
    };
    let rendered = run_browse(&mcp_cfg, &browse_args).await?;
    Ok(strip_trailing_newline(&rendered))
}

async fn tool_list_categories(cfg: &CliConfig) -> Result<String> {
    let mcp_cfg = CliConfig {
        base_url: cfg.base_url.clone(),
        format: OutputFormat::Text,
        out: None,
        cookies: cfg.cookies.clone(),
        emit_stdout: false,
    };
    let rendered = run_categories(&mcp_cfg).await?;
    Ok(strip_trailing_newline(&rendered))
}

async fn tool_download_torrent(args: &Value, cfg: &CliConfig) -> Result<String> {
    let topic_id =
        arg_u64(args, "topic_id").ok_or_else(|| anyhow!("download_torrent: missing 'topic_id'"))?;
    let dest_dir =
        arg_str(args, "dest_dir").ok_or_else(|| anyhow!("download_torrent: missing 'dest_dir'"))?;
    let dl_args = DownloadArgs {
        topic_id,
        out_dir: PathBuf::from(dest_dir),
        allow_path: false,
    };
    let path = run_download(cfg, &dl_args).await?;
    Ok(format!("Saved: {}", path.display()))
}

fn strip_trailing_newline(s: &str) -> String {
    s.strip_suffix('\n').unwrap_or(s).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rutracker_cli::prelude::CliConfig;
    use serde_json::json;
    use std::collections::HashMap;
    use wiremock::matchers::{method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const FORUM_FIXTURE: &[u8] = include_bytes!("../../parser/tests/fixtures/forum-sample.html");
    const TOPIC_FIXTURE: &[u8] = include_bytes!("../../parser/tests/fixtures/topic-sample.html");
    const INDEX_FIXTURE: &[u8] = include_bytes!("../../parser/tests/fixtures/index-sample.html");

    fn cfg(server: &MockServer) -> CliConfig {
        CliConfig {
            base_url: format!("{}/forum/", server.uri()),
            format: rutracker_cli::prelude::OutputFormat::Text,
            out: None,
            cookies: HashMap::new(),
            emit_stdout: false,
        }
    }

    #[tokio::test]
    async fn test_dispatch_search_tool() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/forum/tracker.php"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(FORUM_FIXTURE.to_vec()))
            .mount(&server)
            .await;
        let params = json!({
            "name": "search",
            "arguments": {"query": "test", "category": "252"}
        });
        let text = dispatch_tool_call(&params, &cfg(&server)).await.unwrap();
        assert!(text.starts_with("Found "), "unexpected shape: {text}");
        // strip_trailing_newline should have removed the trailing '\n'
        assert!(!text.ends_with('\n'));
    }

    #[tokio::test]
    async fn test_dispatch_get_topic_tool() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/forum/viewtopic.php"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(TOPIC_FIXTURE.to_vec()))
            .mount(&server)
            .await;
        let params = json!({
            "name": "get_topic",
            "arguments": {"topic_id": 6843582, "include_comments": true}
        });
        let text = dispatch_tool_call(&params, &cfg(&server)).await.unwrap();
        assert!(text.contains("Title: "), "missing Title line: {text}");
    }

    #[tokio::test]
    async fn test_dispatch_browse_forum_tool() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/forum/tracker.php"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(FORUM_FIXTURE.to_vec()))
            .mount(&server)
            .await;
        let params = json!({
            "name": "browse_forum",
            "arguments": {"category_id": "252"}
        });
        let text = dispatch_tool_call(&params, &cfg(&server)).await.unwrap();
        assert!(text.starts_with("Found "));
    }

    #[tokio::test]
    async fn test_dispatch_list_categories_tool() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/forum/index.php"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(INDEX_FIXTURE.to_vec()))
            .mount(&server)
            .await;
        let params = json!({
            "name": "list_categories",
            "arguments": {}
        });
        let text = dispatch_tool_call(&params, &cfg(&server)).await.unwrap();
        assert!(
            text.contains("[c-"),
            "categories output missing [c-… prefix: {text}"
        );
    }

    #[tokio::test]
    async fn test_dispatch_download_torrent_tool() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/forum/dl.php"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(b"d8:announceX5:dummye".to_vec())
                    .insert_header("content-type", "application/x-bittorrent"),
            )
            .mount(&server)
            .await;
        // Target under $HOME so the default sandbox accepts it.
        let home = dirs::home_dir().unwrap();
        let dest = home.join(format!(
            "rutracker-mcp-dispatch-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dest);

        let params = json!({
            "name": "download_torrent",
            "arguments": {"topic_id": 12345, "dest_dir": dest.display().to_string()}
        });
        let text = dispatch_tool_call(&params, &cfg(&server)).await.unwrap();
        assert!(
            text.starts_with("Saved: "),
            "download_torrent must report 'Saved: <path>', got: {text}"
        );
        let _ = std::fs::remove_dir_all(&dest);
    }

    #[tokio::test]
    async fn test_dispatch_missing_name_errors() {
        let server = MockServer::start().await;
        let params = json!({"arguments": {}});
        let err = dispatch_tool_call(&params, &cfg(&server))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("missing 'name'"));
    }

    #[tokio::test]
    async fn test_dispatch_unknown_tool_errors() {
        let server = MockServer::start().await;
        let params = json!({"name": "not_a_real_tool", "arguments": {}});
        let err = dispatch_tool_call(&params, &cfg(&server))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown tool"));
    }

    #[tokio::test]
    async fn test_dispatch_search_missing_query_errors() {
        let server = MockServer::start().await;
        let params = json!({"name": "search", "arguments": {}});
        let err = dispatch_tool_call(&params, &cfg(&server))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("missing 'query'"));
    }

    #[tokio::test]
    async fn test_dispatch_get_topic_missing_id_errors() {
        let server = MockServer::start().await;
        let params = json!({"name": "get_topic", "arguments": {}});
        let err = dispatch_tool_call(&params, &cfg(&server))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("missing 'topic_id'"));
    }

    #[tokio::test]
    async fn test_dispatch_browse_forum_missing_category_errors() {
        let server = MockServer::start().await;
        let params = json!({"name": "browse_forum", "arguments": {}});
        let err = dispatch_tool_call(&params, &cfg(&server))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("missing 'category_id'"));
    }

    #[tokio::test]
    async fn test_dispatch_download_torrent_missing_topic_id_errors() {
        let server = MockServer::start().await;
        let params = json!({"name": "download_torrent", "arguments": {"dest_dir": "/tmp/x"}});
        let err = dispatch_tool_call(&params, &cfg(&server))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("missing 'topic_id'"));
    }

    #[tokio::test]
    async fn test_dispatch_download_torrent_missing_dest_errors() {
        let server = MockServer::start().await;
        let params = json!({"name": "download_torrent", "arguments": {"topic_id": 1}});
        let err = dispatch_tool_call(&params, &cfg(&server))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("missing 'dest_dir'"));
    }

    #[test]
    fn test_strip_trailing_newline() {
        assert_eq!(strip_trailing_newline("abc\n"), "abc");
        assert_eq!(strip_trailing_newline("abc"), "abc");
        assert_eq!(strip_trailing_newline(""), "");
    }
}
