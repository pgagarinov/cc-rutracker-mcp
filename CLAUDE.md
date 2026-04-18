# CLAUDE.md

Guidance for Claude Code when working in this repository.

## Project Overview

Rust workspace with 5 crates and 2 binaries. `rutracker` is a composable CLI; `rutracker-mcp` is an MCP stdio server for Claude Code. Both wrap the same parser/HTTP/cookies core.

## Development

```bash
# Build + test
cargo build --release
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check

# Install both binaries
cargo install --path crates/cli --locked
cargo install --path crates/mcp --locked
```

## Architecture

```
crates/
  parser/          pure HTML parsing (scraper + encoding_rs) — no I/O
    src/search.rs       tracker.php rows
    src/topic.rs        viewtopic.php (opening post + comments)
    src/metadata.rs     label/value extraction from post_body
    src/forum_index.rs  index.php categories + subforums
    src/text_format.rs  shared Display impls for CLI text mode + MCP tool output
  http/            reqwest async client, cp1251 decoding, login-redirect recovery
  cookies-macos/   Brave AES-128-CBC decrypt + Keychain lookup (security-framework)
  cli/             `rutracker` binary — clap subcommands, JSON default, path sandbox
  mcp/             `rutracker-mcp` binary — hand-rolled JSON-RPC stdio server, 5 tools
```

Library-level tests live in each crate and use `wiremock` + HTML fixtures. Live Keychain / live network tests are `#[ignore]`-marked; run them with `cargo test -- --ignored`.

## Cookies

First run prompts macOS Keychain for "Brave Safe Storage" access. Decrypted cookies cached to `.cookies.json` (gitignored). `bb_dl_key` required for `.torrent` downloads.

## MCP Setup

`.mcp.json` points at the installed `rutracker-mcp` binary. No extra configuration.
