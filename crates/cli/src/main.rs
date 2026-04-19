//! `rutracker` — CLI entry point. Thin clap wrapper over `rutracker-cli::prelude` handlers.

use anyhow::{Context, Result};
use clap::{ArgAction, Parser, Subcommand, ValueEnum};
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
    /// Local mirror commands (v1: watchlist only).
    Mirror {
        #[command(subcommand)]
        cmd: MirrorCmd,
    },
    /// Film ranker — titles / scan-prepare / aggregate / list / show.
    /// Local-only; reads the mirror, no network.
    Rank {
        #[command(subcommand)]
        cmd: RankCmd,
    },
}

#[derive(Debug, Subcommand)]
enum RankCmd {
    /// Parse mirror topic titles into film_index + film_topic (idempotent).
    Match {
        /// Restrict to this forum id. Empty ⇒ all forums on disk + watchlist.
        #[arg(long)]
        forum: Option<String>,
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Emit the `scan-queue.jsonl` manifest for the `/rank-scan-run` skill.
    ScanPrepare {
        #[arg(long)]
        forum: String,
        #[arg(long)]
        max_payload_bytes: Option<usize>,
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Aggregate scan outputs into film_score; rank rips per film.
    Aggregate {
        #[arg(long)]
        forum: Option<String>,
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Query film_score (JSON default, `--format text` for a compact table).
    List {
        #[arg(long)]
        forum: Option<String>,
        #[arg(long)]
        min_score: Option<f32>,
        #[arg(long)]
        top: Option<u32>,
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Show one film's canonical title, score, themes, and ranked rips.
    Show {
        /// film_id or a substring of the Russian/English title.
        query: String,
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Dump `logs/rank-parse-failures.log` contents.
    ParseFailures {
        #[arg(long)]
        root: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum MirrorCmd {
    /// Initialise a mirror root (creates dirs + state.db).
    Init {
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Refresh `structure.json` from the live index.
    Structure {
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Manage the forum watchlist.
    Watch {
        #[command(subcommand)]
        cmd: WatchCmd,
    },
    /// Sync the watchlist (or the forums given via --forum) into the local mirror.
    Sync {
        /// Restrict sync to these forum ids. Repeatable. Empty ⇒ use the watchlist.
        #[arg(long = "forum")]
        forums: Vec<String>,
        #[arg(long, default_value_t = 500)]
        max_topics: usize,
        #[arg(long, default_value_t = 1.0)]
        rate_rps: f32,
        #[arg(long, default_value_t = 24)]
        max_attempts_per_forum: u32,
        #[arg(long, default_value_t = true, action = ArgAction::Set)]
        cooldown_wait: bool,
        #[arg(long)]
        log_file: Option<String>,
        /// Walk the full forum listing, ignoring the 5-row stop streak. Use
        /// when resuming an interrupted initial bulk fetch.
        #[arg(long)]
        force_full: bool,
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Pretty-print a cached topic JSON. ARG is `<forum_id>/<topic_id>`.
    Show {
        target: String,
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Report per-forum topic counts, last-sync outcomes, and active cooldowns.
    Status {
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Rebuild `state.db` from the on-disk JSON layer.
    RebuildIndex {
        #[arg(long)]
        root: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum WatchCmd {
    /// Add a forum id to the watchlist.
    Add {
        forum_id: String,
        /// Override the mirror root (defaults to $RUTRACKER_MIRROR_ROOT or $HOME/.rutracker/mirror).
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Remove a forum id from the watchlist.
    Remove {
        forum_id: String,
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// List watchlisted forums.
    List {
        #[arg(long)]
        root: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let is_sync = matches!(
        &cli.cmd,
        Cmd::Mirror {
            cmd: MirrorCmd::Sync { .. }
        }
    );
    if !is_sync {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "rutracker=warn".into()),
            )
            .init();
    }
    // Ranker commands are local-only (no HTTP, no downloads) — skip the
    // Keychain prompt so `rank …` works even without Brave cookies.
    let needs_cookies = !matches!(cli.cmd, Cmd::Rank { .. });
    let cookies = if needs_cookies {
        match load_cookies(&cli.profile) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("warning: cookie load failed: {e}");
                std::collections::HashMap::new()
            }
        }
    } else {
        std::collections::HashMap::new()
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
        Cmd::Mirror { cmd } => match cmd {
            MirrorCmd::Init { root } => {
                run_mirror_init(&cfg, &MirrorRootArgs { root })
                    .await
                    .context("mirror init failed")?;
            }
            MirrorCmd::Structure { root } => {
                run_mirror_structure(&cfg, &MirrorRootArgs { root })
                    .await
                    .context("mirror structure failed")?;
            }
            MirrorCmd::Watch { cmd } => match cmd {
                WatchCmd::Add { forum_id, root } => {
                    let mirror_root = mirror_root_for(root.as_ref());
                    let forum_id =
                        resolve_forum(&mirror_root, &forum_id).context("resolving forum")?;
                    run_watch_add(&cfg, &WatchArgs { forum_id, root })
                        .await
                        .context("watch add failed")?;
                }
                WatchCmd::Remove { forum_id, root } => {
                    let mirror_root = mirror_root_for(root.as_ref());
                    let forum_id =
                        resolve_forum(&mirror_root, &forum_id).context("resolving forum")?;
                    run_watch_remove(&cfg, &WatchArgs { forum_id, root })
                        .await
                        .context("watch remove failed")?;
                }
                WatchCmd::List { root } => {
                    run_watch_list(&cfg, &WatchListArgs { root })
                        .await
                        .context("watch list failed")?;
                }
            },
            MirrorCmd::Sync {
                forums,
                max_topics,
                rate_rps,
                max_attempts_per_forum,
                cooldown_wait,
                log_file,
                force_full,
                root,
            } => {
                let mirror_root = mirror_root_for(root.as_ref());
                let forums = forums
                    .into_iter()
                    .map(|f| resolve_forum(&mirror_root, &f))
                    .collect::<Result<Vec<_>>>()
                    .context("resolving forum ids")?;
                match run_mirror_sync(
                    &cfg,
                    &SyncCliArgs {
                        root,
                        forums,
                        max_topics,
                        rate_rps,
                        max_attempts_per_forum,
                        cooldown_wait,
                        log_file,
                        force_full,
                    },
                )
                .await
                {
                    Ok(result) => {
                        if result.exit_code != 0 {
                            std::process::exit(result.exit_code);
                        }
                    }
                    Err(err) => {
                        eprintln!("{:#}", err.context("mirror sync failed"));
                        std::process::exit(2);
                    }
                }
            }
            MirrorCmd::Show { target, root } => {
                let (forum_ref, topic_id) = target.split_once('/').ok_or_else(|| {
                    anyhow::anyhow!("expected <forum_id>/<topic_id>, got {target}")
                })?;
                let mirror_root = mirror_root_for(root.as_ref());
                let forum_id = resolve_forum(&mirror_root, forum_ref).context("resolving forum")?;
                run_mirror_show(
                    &cfg,
                    &ShowArgs {
                        root,
                        forum_id,
                        topic_id: topic_id.to_string(),
                    },
                )
                .await
                .context("mirror show failed")?;
            }
            MirrorCmd::Status { root } => {
                run_mirror_status(&cfg, &MirrorRootArgs { root })
                    .await
                    .context("mirror status failed")?;
            }
            MirrorCmd::RebuildIndex { root } => {
                run_mirror_rebuild_index(&cfg, &MirrorRootArgs { root })
                    .await
                    .context("mirror rebuild-index failed")?;
            }
        },
        Cmd::Rank { cmd } => match cmd {
            RankCmd::Match { forum, root } => {
                let forum = if let Some(f) = forum {
                    let mirror_root = mirror_root_for(root.as_ref());
                    Some(resolve_forum(&mirror_root, &f).context("resolving forum")?)
                } else {
                    None
                };
                run_rank_match(&cfg, &RankMatchArgs { forum, root })
                    .await
                    .context("rank match failed")?;
            }
            RankCmd::ScanPrepare {
                forum,
                max_payload_bytes,
                root,
            } => {
                let mirror_root = mirror_root_for(root.as_ref());
                let forum = resolve_forum(&mirror_root, &forum).context("resolving forum")?;
                run_rank_scan_prepare(
                    &cfg,
                    &RankScanPrepareArgs {
                        forum,
                        max_payload_bytes,
                        root,
                    },
                )
                .await
                .context("rank scan-prepare failed")?;
            }
            RankCmd::Aggregate { forum, root } => {
                let forum = if let Some(f) = forum {
                    let mirror_root = mirror_root_for(root.as_ref());
                    Some(resolve_forum(&mirror_root, &f).context("resolving forum")?)
                } else {
                    None
                };
                run_rank_aggregate(&cfg, &RankAggregateArgs { forum, root })
                    .await
                    .context("rank aggregate failed")?;
            }
            RankCmd::List {
                forum,
                min_score,
                top,
                root,
            } => {
                let forum = if let Some(f) = forum {
                    let mirror_root = mirror_root_for(root.as_ref());
                    Some(resolve_forum(&mirror_root, &f).context("resolving forum")?)
                } else {
                    None
                };
                run_rank_list(
                    &cfg,
                    &RankListArgs {
                        forum,
                        min_score,
                        top,
                        root,
                    },
                )
                .await
                .context("rank list failed")?;
            }
            RankCmd::Show { query, root } => {
                run_rank_show(&cfg, &RankShowArgs { query, root })
                    .await
                    .context("rank show failed")?;
            }
            RankCmd::ParseFailures { root } => {
                run_rank_parse_failures(&cfg, &RankParseFailuresArgs { root })
                    .await
                    .context("rank parse-failures failed")?;
            }
        },
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
