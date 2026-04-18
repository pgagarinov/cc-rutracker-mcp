# RuTracker — Rust CLI + MCP

Rust workspace with two binaries sharing one parser/HTTP/cookies core:

| Binary | Purpose |
|---|---|
| `rutracker` | Composable CLI. JSON-first output, `--format text`, `--out FILE`. Runs anywhere a shell does. |
| `rutracker-mcp` | MCP stdio server for Claude Code. 5 tools. |

## Tools

| Tool | Description |
|---|---|
| `search` | Search for torrents. Category filter, sort, pagination. |
| `get_topic` | Topic details — magnet, size, description, metadata (IMDb/KP/year/video/audio), comments. |
| `browse_forum` | List a forum/category without a search query. |
| `list_categories` | Full forum directory (26 groups, ~280 subforums). |
| `download_torrent` | Save `.torrent` file to disk. Path-sandboxed; `--allow-path` to override. |

## Prerequisites

- **macOS** (osx-arm64). Cookie extraction is macOS-Keychain-bound; Linux/Windows support is future work.
- **Rust stable** (≥ 1.75). Auto-installed by `rustup` via `rust-toolchain.toml`.
- **Brave browser** with an active rutracker.org session in a profile named "Peter" (or set `RUTRACKER_PROFILE=...`).

## Install

```bash
cargo build --release
cargo install --path crates/cli --locked
cargo install --path crates/mcp --locked

rutracker --help
rutracker-mcp --help
```

## CLI usage

```bash
# Search
rutracker search "2026" --category 252

# JSON to file, pipe to jq
rutracker search "2026" --out /tmp/search.json
jq '.results | length' /tmp/search.json

# Topic with comments
rutracker topic 6843582 --comments --format text

# Browse a category without a query
rutracker browse 252 --sort-by seeders

# List all categories
rutracker categories --format text

# Download .torrent (sandboxed to $HOME/CWD by default)
rutracker download 6843582 --out-dir $HOME/tmp-rutracker
rutracker download 6843582 --out-dir /Volumes/ext --allow-path
```

Global flags: `--format {json,text}` (default json), `--out FILE`, `--base-url URL`, `--profile NAME`.

## Claude Code MCP usage

The included `.mcp.json` points at the `rutracker-mcp` binary. After `cargo install` runs, reopen Claude Code in this directory and the 5 tools appear automatically.

## Cookies

First run prompts the macOS Keychain for "Brave Safe Storage" access. Cookies are decrypted via AES-128-CBC (PBKDF2-SHA1 key derivation, matches Chromium defaults) and cached locally (gitignored).

Required cookies:
- `bb_session`, `bb_guid`, `bb_ssl`, `bb_t` — authenticated browsing.
- `bb_dl_key` — `.torrent` download via `dl.php`. Absence triggers an explicit error with a refresh hint.

## Testing

```bash
# Fixture-driven tests (fast, offline — default)
cargo test --workspace

# Lint + format gates
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check

# Live smoke (manual, opt-in — requires Brave session + Keychain prompt)
cargo test --workspace -- --ignored
```

## Local mirror

`rutracker-mirror` keeps an incremental on-disk copy of any forums you watch. The
JSON-per-topic layer under `forums/<id>/topics/<topic_id>.json` is the source of
truth; `state.db` (SQLite, WAL) is a derived index rebuildable from the JSONs.

Layout (default root `$HOME/.rutracker/mirror/`, override via
`RUTRACKER_MIRROR_ROOT` or `--root`):

```
$HOME/.rutracker/mirror/
├── structure.json   — forum tree snapshot
├── watchlist.json   — forums to sync
├── state.db         — derived SQLite index
└── forums/<id>/topics/<topic_id>.json
```

Bootstrap + first sync:

```bash
rutracker mirror init
rutracker mirror structure
rutracker mirror watch add 252
rutracker mirror watch add 251
rutracker mirror sync --max-topics 20
```

Re-running `rutracker mirror sync` is idempotent: steady-state passes stop after
5 consecutive unchanged rows and rewrite only topics with a new `last_post_id`.
On HTTP 429/503 the forum is parked for one hour (`forum_state.cooldown_until`);
`rutracker mirror status` surfaces active cooldowns.

```bash
rutracker mirror show 252/6843582 --format text
rutracker mirror status
rutracker mirror rebuild-index   # reconstruct state.db from the JSON layer
```

Caveats: mirror root is APFS/ext4 only (NFS/SMB not tested). Watchlist soft-cap
is ~100 forums. Only the latest revision of edited posts is retained — prior
revisions are intentionally not kept (plan §5.3). Add `.rutracker/` to a
repo-level `.gitignore` if the root lives inside a checkout.

Increase log verbosity with `RUST_LOG=rutracker_mirror=debug`.

## Manual release gate

```bash
bash scripts/soak.sh          # parser soak: 20 random topics, asserts title+desc non-empty
bash scripts/soak-mirror.sh   # mirror soak: 2-pass (initial >= 6 files; second pass 0 files)
```

Each script logs to `soak-*-<date>.log`; commit the log with the release.

## Architecture

- `crates/parser` — pure HTML parsers + dataclasses + shared `text_format`. No I/O.
- `crates/http` — async reqwest client, cp1251 decoding, login-redirect recovery.
- `crates/cookies-macos` — Brave cookie AES-CBC decrypt + Keychain lookup + SQLite reader.
- `crates/cli` — `rutracker` binary: clap subcommands, JSON/text output, path sandbox.
- `crates/mcp` — `rutracker-mcp` binary: hand-rolled JSON-RPC stdio server.
- `crates/mirror` — local-mirror engine: SQLite index, atomic JSON writes, delta-aware sync.

Full design: [.omc/plans/full-mcp.md](.omc/plans/full-mcp.md).
