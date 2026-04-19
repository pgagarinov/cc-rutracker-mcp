//! `rutracker` — thin CLI entry point. Argument parsing, tracing init, and
//! exit-code propagation; all real logic lives in `rutracker_cli::dispatch`.

use anyhow::Result;
use clap::Parser;
use rutracker_cli::dispatch::{build_cfg, dispatch, is_mirror_sync, load_brave_cookies, Cli};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if !is_mirror_sync(&cli) {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "rutracker=warn".into()),
            )
            .init();
    }
    let cfg = build_cfg(&cli, load_brave_cookies);
    match dispatch(cli, &cfg).await {
        Ok(0) => Ok(()),
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("{err:#}");
            std::process::exit(2);
        }
    }
}
