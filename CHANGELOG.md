# Changelog

All notable changes to this project are documented in this file. Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versioning: [SemVer](https://semver.org/).

## [1.2.0] — 2026-04-18

### Added — Sync automation + humanization

- `rutracker mirror sync` now auto-resumes through stored cooldowns with a
  per-forum attempt ceiling, structured progress events, and NDJSON logging.
- New sync flags: `--max-attempts-per-forum`, `--cooldown-wait`, and
  `--log-file`.
- Mirror sync requests now look less bot-like via jittered pacing, referer
  threading, reading pauses, and a realistic browser header / user-agent pool.

## [1.1.0] — 2026-04-18

### Added — Local mirror

New `rutracker-mirror` crate and `rutracker mirror` CLI subcommand tree. The
mirror keeps an incremental on-disk copy (`$HOME/.rutracker/mirror/`) of any
forums you watch: one JSON per topic under `forums/<id>/topics/`, plus a
derived SQLite (`state.db`) index that is rebuildable from the JSON layer.

- `rutracker mirror init` — create mirror root + SQLite schema (v1).
- `rutracker mirror structure` — persist `structure.json` from the live index.
- `rutracker mirror watch add|remove|list` — edit the watchlist.
- `rutracker mirror sync [--forum id]… [--max-topics N]` — delta-aware sync:
  5-consecutive-older-and-known stop streak; multi-page comment merge with
  "commit only after all pages"; atomic JSON writes (temp + sync + rename +
  parent-dir fsync); advisory `.lock` file; 1 h cooldown on HTTP 429/503.
- `rutracker mirror show <forum>/<topic>` — pretty-print a cached topic JSON.
- `rutracker mirror status` — per-forum topic counts, last-sync outcomes,
  cooldowns remaining.
- `rutracker mirror rebuild-index` — reconstruct `state.db` from the on-disk
  JSON layer (forward-only schema policy: older binaries refuse newer DBs).
- `scripts/soak-mirror.sh` — release-gate 2-pass contract (initial writes
  ≥ 6 topic files; second pass writes 0).

`.mcp.json` is unchanged. MCP parity for `mirror_sync` / `mirror_status`
is explicitly out of scope for 1.1 and deferred to a follow-up.

## [1.0.0] — 2026-04-18

### Added — Rust rewrite (clean break; no Python, no rollback)

Complete rewrite from the prior Python MCP. The Rust workspace has 5 crates, 2 binaries, 46 tests.

**Crates:**
- `rutracker-parser` — pure HTML parsing (scraper + encoding_rs). 18 fixture-backed tests.
- `rutracker-http` — async reqwest client, cp1251 decoding, login-redirect recovery. 4 wiremock tests.
- `rutracker-cookies-macos` — native Brave cookie AES-128-CBC decrypt (PBKDF2-SHA1) + Keychain via `security-framework`. 8 tests.
- `rutracker-cli` — `rutracker` binary with `clap` subcommands. JSON default, `--format text`, `--out FILE`, path-sandboxed download. 11 tests.
- `rutracker-mcp` — `rutracker-mcp` binary. Hand-rolled JSON-RPC over stdio (no external MCP SDK). 5 tools registered. 5 tests.

**New features over the prior implementation:**
- `browse_forum` — list a category without a search query.
- `list_categories` — 26 forum groups + ~280 subforums, hierarchical.
- `get_topic` extracts 29 user comments + structured metadata (IMDb/Kinopoisk URLs, year, countries, genres, director, cast, duration, release type, video, audio tracks).
- CLI binary — composable with shell, scripts, CI, `jq`.
- `.torrent` download with `bb_dl_key` cookie assertion and path sandbox (`--allow-path` to override).

### Fixed
- Author selector for tracker.php rows. The prior `a.u-link` selector no longer matched rutracker's HTML; Rust uses `td.u-name-col a` with a regression test asserting a specific known-good value.

### Removed
- All Python code (`src/rutracker_mcp/`), `pyproject.toml`, `pixi.lock`, `.pixi/`, `.mcp.json.python-fallback`, `.cookies.json` (Python-era cache).

### HTML selectors of record
- `table#tor-tbl tbody tr` — search rows (50/page).
- `td.u-name-col a` — author.
- `td.f-name-col a, a.gen` — category name.
- `div.category` + `h3.cat_title` — forum group.
- `a[href*="viewforum.php?f="]` — subforum link.
- `tbody[id^="post_"]` — post container.
- `p.nick` — comment author.
- `a.p-link.small` — comment date.
- `div.post_body` — post body.
- `span.post-b` — opening-post label/value pairs.
