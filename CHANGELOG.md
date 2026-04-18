# Changelog

All notable changes to this project are documented in this file. Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versioning: [SemVer](https://semver.org/).

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
