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
