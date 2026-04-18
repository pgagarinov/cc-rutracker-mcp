//! `rutracker-mcp` — MCP stdio server binary.
//!
//! Protocol: line-delimited JSON-RPC 2.0 on stdin/stdout. Claude Code's MCP stdio transport
//! sends one request per line and expects one response per line. Notifications (no `id`) get
//! no response.

use anyhow::Result;
use rutracker_mcp::{cli_config_for_mcp, handle_request, Request};
use std::collections::HashMap;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const DEFAULT_BASE_URL: &str = "https://rutracker.org/forum/";
const DEFAULT_PROFILE: &str = "Peter";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rutracker=warn".into()),
        )
        .init();

    let base_url =
        std::env::var("RUTRACKER_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
    let profile =
        std::env::var("RUTRACKER_PROFILE").unwrap_or_else(|_| DEFAULT_PROFILE.to_string());
    let cookies = load_cookies(&profile).unwrap_or_default();

    let cfg = cli_config_for_mcp(base_url, cookies);

    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin).lines();

    while let Some(line) = reader.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let request: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "invalid JSON-RPC request");
                let err = rutracker_mcp::Response::err(None, -32700, format!("parse error: {e}"));
                write_response(&mut stdout, &err).await?;
                continue;
            }
        };

        // Notifications (id=None, method starts with "notifications/") get no response.
        let is_notification = request.id.is_none();
        let response = handle_request(request, &cfg).await;
        if !is_notification {
            write_response(&mut stdout, &response).await?;
        }
    }
    Ok(())
}

async fn write_response(out: &mut tokio::io::Stdout, resp: &rutracker_mcp::Response) -> Result<()> {
    let mut line = serde_json::to_string(resp)?;
    line.push('\n');
    out.write_all(line.as_bytes()).await?;
    out.flush().await?;
    Ok(())
}

fn load_cookies(profile: &str) -> Result<HashMap<String, String>> {
    #[cfg(target_os = "macos")]
    {
        Ok(rutracker_cookies_macos::load_brave_cookies(profile)?)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = profile;
        Ok(HashMap::new())
    }
}
