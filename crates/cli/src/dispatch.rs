//! `rutracker` CLI dispatch layer.
//!
//! Owns the clap argument types (`Cli`, `Cmd`, `MirrorCmd`, `RankCmd`,
//! `WatchCmd`, `FormatArg`) and the `dispatch` function that maps a parsed
//! `Cli` onto the concrete handlers in `crate` / `crate::rank`.
//!
//! Extracted from `main.rs` so the binary stays a genuinely thin wrapper
//! (argument parsing, tracing init, and exit-code propagation only) — every
//! piece of real logic in this module is exercised by the unit tests at the
//! bottom of this file.

use crate::prelude::*;
use crate::{mirror_root_for, resolve_forum};
use anyhow::{Context, Result};
use clap::{ArgAction, Parser, Subcommand, ValueEnum};
use std::collections::HashMap;
use std::path::PathBuf;

pub const DEFAULT_BASE_URL: &str = "https://rutracker.org/forum/";

#[derive(Debug, Parser)]
#[command(
    name = "rutracker",
    version,
    about = "RuTracker CLI — search, browse, and download torrents. Rust rewrite of the Python MCP."
)]
pub struct Cli {
    #[arg(long, env = "RUTRACKER_BASE_URL", default_value = DEFAULT_BASE_URL)]
    pub base_url: String,

    #[arg(long, value_enum, default_value_t = FormatArg::Json, global = true)]
    pub format: FormatArg,

    #[arg(long, global = true)]
    pub out: Option<PathBuf>,

    #[arg(
        long,
        env = "RUTRACKER_PROFILE",
        default_value = "Peter",
        global = true
    )]
    pub profile: String,

    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum FormatArg {
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
pub enum Cmd {
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
pub enum RankCmd {
    Match {
        #[arg(long)]
        forum: Option<String>,
        #[arg(long)]
        root: Option<PathBuf>,
    },
    ScanPrepare {
        #[arg(long)]
        forum: String,
        #[arg(long)]
        max_payload_bytes: Option<usize>,
        #[arg(long)]
        root: Option<PathBuf>,
    },
    Aggregate {
        #[arg(long)]
        forum: Option<String>,
        #[arg(long)]
        root: Option<PathBuf>,
    },
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
    Show {
        query: String,
        #[arg(long)]
        root: Option<PathBuf>,
    },
    ParseFailures {
        #[arg(long)]
        root: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
pub enum MirrorCmd {
    Init {
        #[arg(long)]
        root: Option<PathBuf>,
    },
    Structure {
        #[arg(long)]
        root: Option<PathBuf>,
    },
    Watch {
        #[command(subcommand)]
        cmd: WatchCmd,
    },
    Sync {
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
        #[arg(long)]
        force_full: bool,
        #[arg(long)]
        root: Option<PathBuf>,
    },
    Show {
        target: String,
        #[arg(long)]
        root: Option<PathBuf>,
    },
    Status {
        #[arg(long)]
        root: Option<PathBuf>,
    },
    RebuildIndex {
        #[arg(long)]
        root: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
pub enum WatchCmd {
    Add {
        forum_id: String,
        #[arg(long)]
        root: Option<PathBuf>,
    },
    Remove {
        forum_id: String,
        #[arg(long)]
        root: Option<PathBuf>,
    },
    List {
        #[arg(long)]
        root: Option<PathBuf>,
    },
}

/// True iff the command is `mirror sync` — the one command that installs its
/// own tracing subscriber, so the top-level `main` must skip the default one.
pub fn is_mirror_sync(cli: &Cli) -> bool {
    matches!(
        cli.cmd,
        Cmd::Mirror {
            cmd: MirrorCmd::Sync { .. }
        }
    )
}

/// True for commands that talk to the network and therefore need the Brave
/// cookie jar. Ranker commands are local-only (read the mirror) so they
/// must NOT trigger the macOS Keychain prompt.
pub fn needs_cookies(cmd: &Cmd) -> bool {
    !matches!(cmd, Cmd::Rank { .. })
}

/// Build the `CliConfig` for a dispatch, loading cookies only when the
/// command actually needs them. `cookie_loader` is injectable so tests can
/// cover both the cookie-loaded and cookie-skipped branches without hitting
/// the macOS Keychain.
pub fn build_cfg<F>(cli: &Cli, cookie_loader: F) -> CliConfig
where
    F: FnOnce(&str) -> Result<HashMap<String, String>>,
{
    let cookies = if needs_cookies(&cli.cmd) {
        match cookie_loader(&cli.profile) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("warning: cookie load failed: {e}");
                HashMap::new()
            }
        }
    } else {
        HashMap::new()
    };
    CliConfig {
        base_url: cli.base_url.clone(),
        format: cli.format.into(),
        out: cli.out.clone(),
        cookies,
        emit_stdout: true,
    }
}

/// Default production cookie loader — pulls from Brave Keychain on macOS,
/// empty map elsewhere.
pub fn load_brave_cookies(profile: &str) -> Result<HashMap<String, String>> {
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

/// Resolve a required forum reference (name or id) against `structure.json`
/// under `root`. Wraps the resolver error with a uniform `"resolving forum"`
/// context so every dispatch arm surfaces the same message shape.
fn resolve_required_forum(root: Option<&PathBuf>, input: &str) -> Result<String> {
    let mirror_root = mirror_root_for(root);
    resolve_forum(&mirror_root, input).context("resolving forum")
}

/// Resolve an optional forum reference: `None` passes through, `Some(name)`
/// is resolved via [`resolve_required_forum`]. Used by `rank` commands where
/// `--forum` is optional.
fn resolve_optional_forum(root: Option<&PathBuf>, input: Option<String>) -> Result<Option<String>> {
    match input {
        Some(f) => resolve_required_forum(root, &f).map(Some),
        None => Ok(None),
    }
}

/// Top-level dispatch. Returns the process exit code (0 on success, non-zero
/// when `mirror sync` reports a partial failure). Errors propagate as
/// `Err(anyhow)` — the caller (main or a test) decides how to surface them.
pub async fn dispatch(cli: Cli, cfg: &CliConfig) -> Result<i32> {
    match cli.cmd {
        Cmd::Search {
            query,
            category,
            sort_by,
            order,
            page,
        } => {
            run_search(
                cfg,
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
                cfg,
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
                cfg,
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
            run_categories(cfg).await.context("categories failed")?;
        }
        Cmd::Download {
            topic_id,
            out_dir,
            allow_path,
        } => {
            let path = run_download(
                cfg,
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
        Cmd::Mirror { cmd } => return dispatch_mirror(cmd, cfg).await,
        Cmd::Rank { cmd } => dispatch_rank(cmd, cfg).await?,
    }
    Ok(0)
}

async fn dispatch_mirror(cmd: MirrorCmd, cfg: &CliConfig) -> Result<i32> {
    match cmd {
        MirrorCmd::Init { root } => {
            run_mirror_init(cfg, &MirrorRootArgs { root })
                .await
                .context("mirror init failed")?;
        }
        MirrorCmd::Structure { root } => {
            run_mirror_structure(cfg, &MirrorRootArgs { root })
                .await
                .context("mirror structure failed")?;
        }
        MirrorCmd::Watch { cmd } => match cmd {
            WatchCmd::Add { forum_id, root } => {
                let forum_id = resolve_required_forum(root.as_ref(), &forum_id)?;
                run_watch_add(cfg, &WatchArgs { forum_id, root })
                    .await
                    .context("watch add failed")?;
            }
            WatchCmd::Remove { forum_id, root } => {
                let forum_id = resolve_required_forum(root.as_ref(), &forum_id)?;
                run_watch_remove(cfg, &WatchArgs { forum_id, root })
                    .await
                    .context("watch remove failed")?;
            }
            WatchCmd::List { root } => {
                run_watch_list(cfg, &WatchListArgs { root })
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
            let result = run_mirror_sync(
                cfg,
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
            .context("mirror sync failed")?;
            return Ok(result.exit_code);
        }
        MirrorCmd::Show { target, root } => {
            let (forum_ref, topic_id) = target
                .split_once('/')
                .ok_or_else(|| anyhow::anyhow!("expected <forum_id>/<topic_id>, got {target}"))?;
            let forum_id = resolve_required_forum(root.as_ref(), forum_ref)?;
            run_mirror_show(
                cfg,
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
            run_mirror_status(cfg, &MirrorRootArgs { root })
                .await
                .context("mirror status failed")?;
        }
        MirrorCmd::RebuildIndex { root } => {
            run_mirror_rebuild_index(cfg, &MirrorRootArgs { root })
                .await
                .context("mirror rebuild-index failed")?;
        }
    }
    Ok(0)
}

async fn dispatch_rank(cmd: RankCmd, cfg: &CliConfig) -> Result<()> {
    match cmd {
        RankCmd::Match { forum, root } => {
            let forum = resolve_optional_forum(root.as_ref(), forum)?;
            run_rank_match(cfg, &RankMatchArgs { forum, root })
                .await
                .context("rank match failed")?;
        }
        RankCmd::ScanPrepare {
            forum,
            max_payload_bytes,
            root,
        } => {
            let forum = resolve_required_forum(root.as_ref(), &forum)?;
            run_rank_scan_prepare(
                cfg,
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
            let forum = resolve_optional_forum(root.as_ref(), forum)?;
            run_rank_aggregate(cfg, &RankAggregateArgs { forum, root })
                .await
                .context("rank aggregate failed")?;
        }
        RankCmd::List {
            forum,
            min_score,
            top,
            root,
        } => {
            let forum = resolve_optional_forum(root.as_ref(), forum)?;
            run_rank_list(
                cfg,
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
            run_rank_show(cfg, &RankShowArgs { query, root })
                .await
                .context("rank show failed")?;
        }
        RankCmd::ParseFailures { root } => {
            run_rank_parse_failures(cfg, &RankParseFailuresArgs { root })
                .await
                .context("rank parse-failures failed")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cli_with_cmd(cmd: Cmd) -> Cli {
        Cli {
            base_url: DEFAULT_BASE_URL.to_string(),
            format: FormatArg::Json,
            out: None,
            profile: "Peter".to_string(),
            cmd,
        }
    }

    #[test]
    fn test_is_mirror_sync_true_only_for_mirror_sync() {
        let sync_cli = cli_with_cmd(Cmd::Mirror {
            cmd: MirrorCmd::Sync {
                forums: vec![],
                max_topics: 500,
                rate_rps: 1.0,
                max_attempts_per_forum: 24,
                cooldown_wait: true,
                log_file: None,
                force_full: false,
                root: None,
            },
        });
        assert!(is_mirror_sync(&sync_cli));

        let status_cli = cli_with_cmd(Cmd::Mirror {
            cmd: MirrorCmd::Status { root: None },
        });
        assert!(!is_mirror_sync(&status_cli));

        let search_cli = cli_with_cmd(Cmd::Search {
            query: "x".into(),
            category: None,
            sort_by: "seeders".into(),
            order: "desc".into(),
            page: 1,
        });
        assert!(!is_mirror_sync(&search_cli));
    }

    #[test]
    fn test_needs_cookies_false_only_for_rank_commands() {
        let rank_cli = cli_with_cmd(Cmd::Rank {
            cmd: RankCmd::List {
                forum: None,
                min_score: None,
                top: None,
                root: None,
            },
        });
        assert!(!needs_cookies(&rank_cli.cmd));

        let search_cli = cli_with_cmd(Cmd::Search {
            query: "x".into(),
            category: None,
            sort_by: "seeders".into(),
            order: "desc".into(),
            page: 1,
        });
        assert!(needs_cookies(&search_cli.cmd));

        let mirror_sync_cli = cli_with_cmd(Cmd::Mirror {
            cmd: MirrorCmd::Sync {
                forums: vec![],
                max_topics: 500,
                rate_rps: 1.0,
                max_attempts_per_forum: 24,
                cooldown_wait: true,
                log_file: None,
                force_full: false,
                root: None,
            },
        });
        assert!(needs_cookies(&mirror_sync_cli.cmd));
    }

    #[test]
    fn test_build_cfg_rank_command_skips_cookie_loader() {
        let cli = cli_with_cmd(Cmd::Rank {
            cmd: RankCmd::ParseFailures { root: None },
        });
        let cfg = build_cfg(&cli, |_profile| -> Result<HashMap<String, String>> {
            panic!("cookie loader must not run for rank commands")
        });
        assert!(
            cfg.cookies.is_empty(),
            "rank commands must always see an empty cookie map"
        );
        assert_eq!(cfg.base_url, DEFAULT_BASE_URL);
        assert!(cfg.emit_stdout, "CLI binary emit_stdout must default true");
    }

    #[test]
    fn test_build_cfg_search_invokes_loader_and_uses_profile() {
        let cli = Cli {
            base_url: DEFAULT_BASE_URL.to_string(),
            format: FormatArg::Text,
            out: Some(PathBuf::from("/tmp/out.json")),
            profile: "Work".to_string(),
            cmd: Cmd::Search {
                query: "x".into(),
                category: None,
                sort_by: "seeders".into(),
                order: "desc".into(),
                page: 1,
            },
        };
        let cfg = build_cfg(&cli, |profile| {
            assert_eq!(profile, "Work", "loader must receive Cli.profile");
            let mut m = HashMap::new();
            m.insert("k".into(), "v".into());
            Ok(m)
        });
        assert_eq!(cfg.cookies.get("k"), Some(&"v".into()));
        assert!(matches!(cfg.format, OutputFormat::Text));
        assert_eq!(
            cfg.out.as_deref(),
            Some(PathBuf::from("/tmp/out.json").as_path())
        );
    }

    #[test]
    fn test_build_cfg_swallows_cookie_loader_error() {
        let cli = cli_with_cmd(Cmd::Categories);
        let cfg = build_cfg(&cli, |_profile| Err(anyhow::anyhow!("keychain locked")));
        assert!(
            cfg.cookies.is_empty(),
            "cookie errors fall back to empty jar"
        );
    }

    #[test]
    fn test_load_brave_cookies_non_panic_on_non_macos_path() {
        // On macOS this may hit the Keychain (gated by the actual profile
        // existing); on non-macOS it must be a trivial empty-map return.
        // Either way the function signature must compile and not panic.
        #[cfg(not(target_os = "macos"))]
        {
            let got = load_brave_cookies("Peter").unwrap();
            assert!(got.is_empty());
        }
        #[cfg(target_os = "macos")]
        {
            // Don't actually run the Keychain path in unit tests — just
            // check the symbol resolves. The live path is covered by the
            // #[ignore]'d integration test.
            let _: fn(&str) -> Result<HashMap<String, String>> = load_brave_cookies;
        }
    }

    #[tokio::test]
    async fn test_dispatch_mirror_show_rejects_malformed_target() {
        let cfg = CliConfig {
            base_url: DEFAULT_BASE_URL.to_string(),
            format: OutputFormat::Json,
            out: None,
            cookies: HashMap::new(),
            emit_stdout: false,
        };
        let err = dispatch_mirror(
            MirrorCmd::Show {
                target: "no-slash".to_string(),
                root: None,
            },
            &cfg,
        )
        .await
        .expect_err("target without `/` must surface the input-validation error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("expected <forum_id>/<topic_id>"),
            "error must cite the expected shape, got: {msg}"
        );
    }

    // -----------------------------------------------------------------------
    // End-to-end dispatch tests for every LOCAL-ONLY Cmd variant.
    //
    // These drive the top-level `dispatch(cli, &cfg)` function so the match
    // arms in `dispatch_mirror` / `dispatch_rank` are exercised, not bypassed.
    // Network-bound arms (Search, Topic, Browse, Categories, Download,
    // Structure, Sync) are deliberately NOT exercised here — their handlers
    // are wiremock-tested in `lib.rs`; what we're asserting here is that the
    // dispatch glue itself works for the commands that need no network.
    //
    // All tests pass a numeric forum id ("252") so `resolve_forum` takes the
    // passthrough branch and no `structure.json` fixture is required.
    // -----------------------------------------------------------------------

    fn dispatch_cfg() -> CliConfig {
        CliConfig {
            base_url: "https://example.test/forum/".to_string(),
            format: OutputFormat::Json,
            out: None,
            cookies: HashMap::new(),
            emit_stdout: false,
        }
    }

    fn dispatch_tempdir() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }

    fn init_mirror_root(root: &std::path::Path) {
        rutracker_mirror::Mirror::init(root).unwrap();
    }

    /// Write a minimal `structure.json` containing a single forum id so
    /// `resolve_forum` / `watch add` calls referencing that id succeed.
    fn seed_structure_with_forum(root: &std::path::Path, forum_id: &str) {
        let json = serde_json::json!({
            "schema_version": 1,
            "groups": [{
                "group_id": "c-1",
                "title": "Test group",
                "forums": [{
                    "forum_id": forum_id,
                    "name": format!("Forum {forum_id}"),
                    "parent_id": null,
                }],
            }],
            "fetched_at": "2026-04-18T12:00:00+00:00",
        });
        std::fs::write(
            root.join("structure.json"),
            serde_json::to_vec_pretty(&json).unwrap(),
        )
        .unwrap();
    }

    fn write_minimal_topic(root: &std::path::Path, forum: &str, topic: &str) {
        use rutracker_mirror::topic_io::{Post, TopicFile};
        let dir = root.join("forums").join(forum).join("topics");
        std::fs::create_dir_all(&dir).unwrap();
        let tf = TopicFile {
            schema_version: 1,
            topic_id: topic.into(),
            forum_id: forum.into(),
            title: format!("Title / Eng ({}) [2025, US, drama, WEB-DLRip] Dub", topic),
            fetched_at: "2026-04-18T12:00:00+00:00".into(),
            last_post_id: 100,
            last_post_at: "2026-04-18T12:00:00+00:00".into(),
            opening_post: Post::default(),
            comments: Vec::new(),
            metadata: serde_json::Value::Null,
            size_bytes: Some(2_000_000_000),
            seeds: Some(10),
            leeches: Some(1),
            downloads: Some(100),
        };
        rutracker_mirror::topic_io::write_json_atomic(&dir.join(format!("{topic}.json")), &tf)
            .unwrap();
    }

    fn cli_with(cmd: Cmd) -> Cli {
        Cli {
            base_url: "https://example.test/forum/".to_string(),
            format: FormatArg::Json,
            out: None,
            profile: "Peter".to_string(),
            cmd,
        }
    }

    #[tokio::test]
    async fn test_dispatch_mirror_init_creates_state_db() {
        let td = dispatch_tempdir();
        let root = td.path().to_path_buf();
        let cli = cli_with(Cmd::Mirror {
            cmd: MirrorCmd::Init {
                root: Some(root.clone()),
            },
        });
        let code = dispatch(cli, &dispatch_cfg()).await.unwrap();
        assert_eq!(code, 0);
        assert!(root.join("state.db").exists(), "state.db must be created");
    }

    #[tokio::test]
    async fn test_dispatch_mirror_watch_add_list_remove_round_trip() {
        let td = dispatch_tempdir();
        let root = td.path().to_path_buf();
        init_mirror_root(&root);
        // `watch add` validates the id against `structure.json`, so seed it
        // with forum 252 to exercise the happy path end-to-end.
        seed_structure_with_forum(&root, "252");

        let add = cli_with(Cmd::Mirror {
            cmd: MirrorCmd::Watch {
                cmd: WatchCmd::Add {
                    forum_id: "252".to_string(),
                    root: Some(root.clone()),
                },
            },
        });
        dispatch(add, &dispatch_cfg()).await.unwrap();

        let list = cli_with(Cmd::Mirror {
            cmd: MirrorCmd::Watch {
                cmd: WatchCmd::List {
                    root: Some(root.clone()),
                },
            },
        });
        dispatch(list, &dispatch_cfg()).await.unwrap();

        let remove = cli_with(Cmd::Mirror {
            cmd: MirrorCmd::Watch {
                cmd: WatchCmd::Remove {
                    forum_id: "252".to_string(),
                    root: Some(root.clone()),
                },
            },
        });
        dispatch(remove, &dispatch_cfg()).await.unwrap();
    }

    #[tokio::test]
    async fn test_dispatch_mirror_status_runs_on_fresh_root() {
        let td = dispatch_tempdir();
        let root = td.path().to_path_buf();
        init_mirror_root(&root);
        let cli = cli_with(Cmd::Mirror {
            cmd: MirrorCmd::Status {
                root: Some(root.clone()),
            },
        });
        dispatch(cli, &dispatch_cfg()).await.unwrap();
    }

    #[tokio::test]
    async fn test_dispatch_mirror_show_reads_cached_topic() {
        let td = dispatch_tempdir();
        let root = td.path().to_path_buf();
        init_mirror_root(&root);
        write_minimal_topic(&root, "252", "1001");
        let cli = cli_with(Cmd::Mirror {
            cmd: MirrorCmd::Show {
                target: "252/1001".to_string(),
                root: Some(root.clone()),
            },
        });
        dispatch(cli, &dispatch_cfg()).await.unwrap();
    }

    #[tokio::test]
    async fn test_dispatch_mirror_rebuild_index_runs_on_fresh_root() {
        let td = dispatch_tempdir();
        let root = td.path().to_path_buf();
        init_mirror_root(&root);
        let cli = cli_with(Cmd::Mirror {
            cmd: MirrorCmd::RebuildIndex {
                root: Some(root.clone()),
            },
        });
        dispatch(cli, &dispatch_cfg()).await.unwrap();
    }

    #[tokio::test]
    async fn test_dispatch_mirror_sync_empty_forum_list_exits_zero() {
        let td = dispatch_tempdir();
        let root = td.path().to_path_buf();
        init_mirror_root(&root);
        let cli = cli_with(Cmd::Mirror {
            cmd: MirrorCmd::Sync {
                forums: vec![],
                max_topics: 10,
                rate_rps: 1.0,
                max_attempts_per_forum: 1,
                cooldown_wait: false,
                log_file: Some(String::new()),
                force_full: false,
                root: Some(root.clone()),
            },
        });
        // Empty forums + empty watchlist = no-op: dispatch_mirror must
        // propagate exit_code from run_mirror_sync (which should be 0).
        let code = dispatch(cli, &dispatch_cfg()).await.unwrap();
        assert_eq!(code, 0);
    }

    #[tokio::test]
    async fn test_dispatch_rank_match_no_forum_runs() {
        let td = dispatch_tempdir();
        let root = td.path().to_path_buf();
        init_mirror_root(&root);
        write_minimal_topic(&root, "252", "1001");
        // Without an explicit --forum, `run_rank_match` unions on-disk forum
        // dirs with the watchlist; since the topic above creates
        // forums/252/topics/, that forum is discovered without needing a
        // watchlist entry.
        let cli = cli_with(Cmd::Rank {
            cmd: RankCmd::Match {
                forum: None,
                root: Some(root.clone()),
            },
        });
        dispatch(cli, &dispatch_cfg()).await.unwrap();
    }

    #[tokio::test]
    async fn test_dispatch_rank_match_with_forum_runs() {
        let td = dispatch_tempdir();
        let root = td.path().to_path_buf();
        init_mirror_root(&root);
        write_minimal_topic(&root, "252", "1001");
        let cli = cli_with(Cmd::Rank {
            cmd: RankCmd::Match {
                forum: Some("252".to_string()),
                root: Some(root.clone()),
            },
        });
        dispatch(cli, &dispatch_cfg()).await.unwrap();
    }

    #[tokio::test]
    async fn test_dispatch_rank_scan_prepare_runs() {
        let td = dispatch_tempdir();
        let root = td.path().to_path_buf();
        init_mirror_root(&root);
        write_minimal_topic(&root, "252", "1001");
        let cli = cli_with(Cmd::Rank {
            cmd: RankCmd::ScanPrepare {
                forum: "252".to_string(),
                max_payload_bytes: None,
                root: Some(root.clone()),
            },
        });
        dispatch(cli, &dispatch_cfg()).await.unwrap();
    }

    #[tokio::test]
    async fn test_dispatch_rank_aggregate_runs() {
        let td = dispatch_tempdir();
        let root = td.path().to_path_buf();
        init_mirror_root(&root);
        let cli = cli_with(Cmd::Rank {
            cmd: RankCmd::Aggregate {
                forum: Some("252".to_string()),
                root: Some(root.clone()),
            },
        });
        dispatch(cli, &dispatch_cfg()).await.unwrap();
    }

    #[tokio::test]
    async fn test_dispatch_rank_aggregate_no_forum_runs() {
        let td = dispatch_tempdir();
        let root = td.path().to_path_buf();
        init_mirror_root(&root);
        let cli = cli_with(Cmd::Rank {
            cmd: RankCmd::Aggregate {
                forum: None,
                root: Some(root.clone()),
            },
        });
        dispatch(cli, &dispatch_cfg()).await.unwrap();
    }

    #[tokio::test]
    async fn test_dispatch_rank_list_runs() {
        let td = dispatch_tempdir();
        let root = td.path().to_path_buf();
        init_mirror_root(&root);
        let cli = cli_with(Cmd::Rank {
            cmd: RankCmd::List {
                forum: Some("252".to_string()),
                min_score: Some(0.0),
                top: Some(10),
                root: Some(root.clone()),
            },
        });
        dispatch(cli, &dispatch_cfg()).await.unwrap();
    }

    #[tokio::test]
    async fn test_dispatch_rank_list_no_forum_runs() {
        let td = dispatch_tempdir();
        let root = td.path().to_path_buf();
        init_mirror_root(&root);
        let cli = cli_with(Cmd::Rank {
            cmd: RankCmd::List {
                forum: None,
                min_score: None,
                top: None,
                root: Some(root.clone()),
            },
        });
        dispatch(cli, &dispatch_cfg()).await.unwrap();
    }

    #[tokio::test]
    async fn test_dispatch_rank_show_missing_film_surfaces_error() {
        let td = dispatch_tempdir();
        let root = td.path().to_path_buf();
        init_mirror_root(&root);
        let cli = cli_with(Cmd::Rank {
            cmd: RankCmd::Show {
                query: "no-such-film".to_string(),
                root: Some(root.clone()),
            },
        });
        // `rank show` with no matching film in film_index returns an error —
        // the match arm propagates it with the "rank show failed" context.
        let err = dispatch(cli, &dispatch_cfg())
            .await
            .expect_err("unknown film query must surface an error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("rank show failed") && msg.contains("no film matches"),
            "error must cite both dispatch context and the downstream message: {msg}"
        );
    }

    #[tokio::test]
    async fn test_dispatch_rank_parse_failures_on_empty_root_runs() {
        let td = dispatch_tempdir();
        let root = td.path().to_path_buf();
        init_mirror_root(&root);
        let cli = cli_with(Cmd::Rank {
            cmd: RankCmd::ParseFailures {
                root: Some(root.clone()),
            },
        });
        dispatch(cli, &dispatch_cfg()).await.unwrap();
    }
}
