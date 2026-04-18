//! `rutracker` — CLI entry point. Thin clap wrapper over `rutracker-cli::prelude` handlers.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use rutracker_cli::prelude::*;
use std::collections::HashMap;
use std::path::PathBuf;

const DEFAULT_BASE_URL: &str = "https://rutracker.org/forum/";

#[derive(Debug, Parser)]
#[command(
    name = "rutracker",
    version,
    about = "RuTracker CLI — search, browse, and download torrents. Rust rewrite of the Python MCP."
)]
struct Cli {
    /// Override the base URL (useful for tests).
    #[arg(long, env = "RUTRACKER_BASE_URL", default_value = DEFAULT_BASE_URL)]
    base_url: String,

    /// Output format.
    #[arg(long, value_enum, default_value_t = FormatArg::Json, global = true)]
    format: FormatArg,

    /// Write output to FILE instead of stdout.
    #[arg(long, global = true)]
    out: Option<PathBuf>,

    /// Brave profile name (macOS). Defaults to "Peter".
    #[arg(
        long,
        env = "RUTRACKER_PROFILE",
        default_value = "Peter",
        global = true
    )]
    profile: String,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum FormatArg {
    Json,
    Text,
}

impl From<FormatArg> for OutputFormat {
    fn from(f: FormatArg) -> Self {
        match f {
            FormatArg::Json => OutputFormat::Json,
            FormatArg::Text => OutputFormat::Text,
        }
    }
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Search for torrents.
    Search {
        query: String,
        #[arg(long)]
        category: Option<String>,
        #[arg(long, default_value = "seeders")]
        sort_by: String,
        #[arg(long, default_value = "desc")]
        order: String,
        #[arg(long, default_value_t = 1)]
        page: u32,
    },
    /// Show full topic details.
    Topic {
        topic_id: u64,
        #[arg(long)]
        comments: bool,
        #[arg(long, default_value_t = 1)]
        max_comment_pages: u32,
    },
    /// List torrents in a forum/category without a query.
    Browse {
        category_id: String,
        #[arg(long, default_value_t = 1)]
        page: u32,
        #[arg(long, default_value = "seeders")]
        sort_by: String,
        #[arg(long, default_value = "desc")]
        order: String,
    },
    /// List all forum categories and subforums.
    Categories,
    /// Download a `.torrent` file.
    Download {
        topic_id: u64,
        #[arg(long)]
        out_dir: PathBuf,
        /// Allow writing outside the default $HOME/CWD sandbox.
        #[arg(long)]
        allow_path: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rutracker=warn".into()),
        )
        .init();

    let cli = Cli::parse();
    let cookies = match load_cookies(&cli.profile) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("warning: cookie load failed: {e}");
            std::collections::HashMap::new()
        }
    };
    let cfg = CliConfig {
        base_url: cli.base_url.clone(),
        format: cli.format.into(),
        out: cli.out.clone(),
        cookies,
        emit_stdout: true, // the CLI binary is the one case where stdout IS the channel
    };

    match cli.cmd {
        Cmd::Search {
            query,
            category,
            sort_by,
            order,
            page,
        } => {
            run_search(
                &cfg,
                &SearchArgs {
                    query,
                    category,
                    sort_by,
                    order,
                    page,
                },
            )
            .await
            .context("search failed")?;
        }
        Cmd::Topic {
            topic_id,
            comments,
            max_comment_pages,
        } => {
            run_topic(
                &cfg,
                &TopicArgs {
                    topic_id,
                    include_comments: comments,
                    max_comment_pages,
                },
            )
            .await
            .context("topic fetch failed")?;
        }
        Cmd::Browse {
            category_id,
            page,
            sort_by,
            order,
        } => {
            run_browse(
                &cfg,
                &BrowseArgs {
                    category_id,
                    page,
                    sort_by,
                    order,
                },
            )
            .await
            .context("browse failed")?;
        }
        Cmd::Categories => {
            run_categories(&cfg).await.context("categories failed")?;
        }
        Cmd::Download {
            topic_id,
            out_dir,
            allow_path,
        } => {
            let path = run_download(
                &cfg,
                &DownloadArgs {
                    topic_id,
                    out_dir,
                    allow_path,
                },
            )
            .await
            .context("download failed")?;
            println!("{}", path.display());
        }
    }
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
