# RuTracker — Rust CLI + MCP Implementation Plan

**Date:** 2026-04-18
**Source PRD:** `.omc/prd.json`

---

## 0.5 Shipped state (supersedes the Phase 6a/6b split below)

This section documents what actually shipped on 2026-04-18. The per-phase bodies in §10 are preserved as the design record, but the final cutover differed in one respect: Python was deleted in the same commit as the Rust ship rather than after a 7-day soak. The plan's Phase 6a (ship + tag `v0.9.0-final-python`) and Phase 6b (delete after soak) collapsed into a single "1.0.0 clean break" release.

Consequences:
- No `v0.9.0-final-python` git tag.
- No rollback path to Python; `pixi`, `pyproject.toml`, `src/rutracker_mcp/`, `.pixi/`, `.cookies.json`, and `.mcp.json.python-fallback` are all removed.
- The live-smoke soak (`scripts/soak.sh`) is still the recommended pre-release gate for future HTML-drift incidents, but is no longer a Phase 6b prerequisite.

Everything else in §§1-16 landed as specified. Final artifact inventory is in `CHANGELOG.md` 1.0.0.

---

## 0. Architecture Decision Record

| Field | Value |
|---|---|
| **Decision** | Rewrite the project as a Rust workspace (3 library crates + 2 binary crates) that exposes a CLI-first interface; MCP is a thin binary shim over the same core. Python implementation deleted after a 7-day soak. |
| **Drivers** | (1) Bulk-comment-ingest use case is a context-bloat regime where CLI-plus-files beats MCP. (2) Distribution & composability — single-OS single-binary `cargo install` runs in shell, scripts, CI, cron, other agents. (3) Explicit user mandate: "Rust all-in, no Python." |
| **Alternatives considered** | A: chosen. B: Python full-scope parity (3–5 days, zero platform risk) — rejected only because of user mandate. C: Rust CLI + Python MCP shim — rejected (dual-runtime contradicts mandate). D: Rust CLI no MCP — rejected (loses zero-config Claude Code; shim is 50–150 LOC/tool, additive). |
| **Why chosen** | Satisfies all three drivers; the bulk-ingest win is the user's actual pain; single-binary distribution composes outside Claude; shared `text_format` in the parser crate keeps both binaries thin (no presentation-layer duplication). |
| **Consequences** | (+) Composable outside Claude. (+) Clean break, no dual-runtime maintenance. (+) Fixture-tested parsers. (−) macOS-only at v1.0 (Keychain/Chromium crypto dep). (−) Net-new platform risk: hand-rolled AES-CBC decrypt, pre-1.0 `rmcp` SDK. (−) Breaking change — `.mcp.json` consumers must re-point. |
| **Follow-ups** | Linux/Windows cookie extraction (§17). Prebuilt binary release pipeline via `cargo-dist` (§17). Structured JSON mode for MCP if a non-Claude consumer appears (§17). |

## 1. Background

Previous plan targeted a Python MCP server. This revision replaces it wholesale with a **Rust CLI-first architecture**, with an MCP binary as a thin shim on top of the same core library. The Python implementation in `src/rutracker_mcp/` is deprecated and will be removed when the Rust version reaches feature parity.

Rationale for the change:
- **CLI-first** — the real use case that triggered this project (bulk comment ingest for audience-reaction analysis across many topics) is a context-bloat regime where MCP loses. CLI writes to files, agent reads what it needs. CLI also runs outside Claude (shell, CI, cron, other agents).
- **Rust** — single static binary distribution, fast cold start, no runtime dep. The core workload is I/O-bound so perf is not the driver; distribution and composability are.
- **Three library crates + two binary crates.** Libraries: `rutracker-parser` (pure, no I/O), `rutracker-http` (reqwest async), `rutracker-cookies-macos` (cfg-gated). Binaries: `rutracker` (CLI) and `rutracker-mcp` (MCP) depend on all three. The shared text-format module lives in the parser crate; zero presentation-logic duplication.

## 2. Goals & Non-Goals

**Goals**
1. Ship a Rust workspace with five crates: `rutracker-parser` (lib, pure), `rutracker-http` (lib, reqwest), `rutracker-cookies-macos` (lib, cfg-gated), `rutracker` (CLI bin), `rutracker-mcp` (MCP bin).
2. Feature coverage: search, topic details, comments (paginated), structured metadata (IMDb/KP ratings, year, genre, video, audio, etc.), browse forum, list categories (hierarchical), `.torrent` download.
3. JSON output by default from CLI (composable with `jq`); text output for MCP tools.
4. Brave cookie extraction on macOS (AES-128-CBC decrypt using Keychain-resident key).
5. Skill document (`SKILL.md`) teaching Claude Code when to use CLI vs MCP tools.
6. Fixture-based tests, `cargo clippy` + `cargo fmt --check` gates, `cargo-nextest` runner.

**Non-goals**
- Python code. The existing `src/rutracker_mcp/` is reference material during porting, deleted after parity.
- Write operations on rutracker (post/vote/reply).
- Cookie extraction on Linux / Windows (future work).
- Cookie extraction from Chrome / Firefox / Safari (future work).

## 3. Target Architecture

### 3.1 Workspace layout

```
rutracker-rs/
├── Cargo.toml                        # workspace root
├── README.md
├── CHANGELOG.md
├── SKILL.md                          # Claude Code skill doc (CLI vs MCP guidance)
├── crates/
│   ├── parser/                       # rutracker-parser (pure, no I/O, no async)
│   │   ├── Cargo.toml                # deps: scraper, encoding_rs, regex, serde
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── search.rs             # tracker.php rows
│   │   │   ├── topic.rs              # viewtopic.php: first post + comments
│   │   │   ├── metadata.rs           # label-based field extraction
│   │   │   ├── forum_index.rs        # index.php groups + forums
│   │   │   ├── filters.rs            # client-side post-filtering (pure)
│   │   │   ├── text_format.rs        # Display impls reused by both binaries (MCP text output + CLI --format text)
│   │   │   ├── models.rs             # structs w/ serde derive
│   │   │   └── error.rs
│   │   └── tests/
│   │       ├── fixtures/             # copied from .omc/research/
│   │       │   ├── topic-sample.html
│   │       │   ├── forum-sample.html
│   │       │   ├── index-sample.html
│   │       │   └── legacy-snapshots/ # captured from Python MCP in Phase 1 day 1
│   │       │       ├── legacy-search.txt
│   │       │       └── legacy-get-topic.txt
│   │       └── parser_tests.rs
│   ├── http/                         # rutracker-http (reqwest async, cp1251)
│   │   ├── Cargo.toml                # deps: rutracker-parser, reqwest, tokio, wiremock (dev)
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── client.rs             # RuTrackerClient with retry/refresh
│   │       └── urls.rs               # URL builders
│   ├── cookies-macos/                # rutracker-cookies-macos (cfg(target_os="macos"))
│   │   ├── Cargo.toml                # deps: rusqlite, aes, cbc, pbkdf2, hmac, sha1, security-framework, dirs
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── keychain.rs
│   │       ├── decrypt.rs            # AES-CBC + vector-tested
│   │       └── profile.rs            # Brave profile resolution
│   ├── cli/                          # rutracker (binary)
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── main.rs
│   │       └── commands/
│   │           ├── search.rs
│   │           ├── topic.rs
│   │           ├── browse.rs
│   │           ├── categories.rs
│   │           └── download.rs
│   └── mcp/                          # rutracker-mcp (binary)
│       ├── Cargo.toml
│       └── src/
│           ├── main.rs               # rmcp server init
│           └── tools.rs              # tool handlers (thin calls into core)
├── .omc/                             # unchanged (research, state, plans, prd)
├── .mcp.json                         # points to `rutracker-mcp` binary
└── .gitignore                        # target/, Cargo.lock stays tracked
```

### 3.2 Binary responsibilities

| Binary | Purpose | Output | Dep graph |
|---|---|---|---|
| `rutracker` | Human + script use. Subcommands for each feature. Default JSON, `--format text` for pretty, `--out FILE` to write to disk. | JSON to stdout/file | parser + http + cookies-macos |
| `rutracker-mcp` | Claude Code MCP stdio transport. Wraps core calls. Returns text via `parser::text_format`. | MCP tool responses | parser + http + cookies-macos |

No shared state, no IPC between them. Both binaries use the **same** `parser::text_format::Display` impls — text formatting is not duplicated.

**Why three crates, not one `core`:** `rutracker-parser` compiles with zero I/O deps. Fixture tests run fast and are Linux/Windows-portable (even though binaries are macOS-only). `cookies-macos` is behind `cfg(target_os = "macos")` so adding future Linux/Windows cookie crates is additive, not a refactor.

### 3.3 Core library public API (Rust)

```rust
// crates/http/src/lib.rs (re-exports from parser for convenience)
pub use client::RuTrackerClient;
pub use models::{
    Comment, TopicMetadata, TopicDetails, SearchResult, SearchPage,
    ForumCategory, CategoryGroup, SortBy, SortOrder,
};
pub use error::{Error, Result};

// Main entrypoints
impl RuTrackerClient {
    pub async fn search(&self, params: SearchParams) -> Result<SearchPage>;
    pub async fn browse_forum(&self, params: BrowseParams) -> Result<SearchPage>;
    pub async fn get_topic(&self, id: u64, opts: TopicOpts) -> Result<TopicDetails>;
    pub async fn list_categories(&self, refresh: bool) -> Result<Vec<CategoryGroup>>;
    pub async fn download_torrent(&self, id: u64) -> Result<TorrentFile>;
}
```

## 4. Verified Facts From Sample HTML

Carried forward from the previous plan — selectors are phpBB/TorrentPier-canonical and independent of implementation language.

- **Topic page** (`viewtopic.php?t=6843582`, 117 KB, 30 `tbody[id^="post_"]`): opening post + 29 replies. `div.post_body` contains body text; `p.nick` author; date string inside the post header. Labelled metadata fields in the opening post (`Год выпуска:`, `Жанр:`, `Видео:`, `Аудио N:`, `Тип релиза:`, `Продолжительность:`). Kinopoisk URL detectable via `a[href*="kinopoisk.ru"]`; IMDb via `a[href*="imdb.com"]`.
- **Forum page** (`tracker.php?f=252`, 247 KB): `table#tor-tbl` with 50 rows. Pagination via `search_id` + `start=N` query params. Author selector in current HTML is **not** `u-link` — the title link uses `med tLink tt-text ts-text hl-tags bold`; the uploader is in a different cell (to be located during Phase 3 porting).
- **Index page** (`index.php`, 107 KB): 26 `div.category` groups, each with an `h3.cat_title`; 321 `a[href*="viewforum.php?f="]` anchors nest inside.

Fixtures in `.omc/research/` are copied into `crates/parser/tests/fixtures/` at the start of Phase 1 and are the contract the parser must satisfy.

## 5. Data Types (Rust)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub topic_id: u64,
    pub title: String,
    pub size: String,
    pub seeds: u32,
    pub leeches: u32,
    pub author: String,
    pub category: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchPage {
    pub results: Vec<SearchResult>,
    pub page: u32,
    pub per_page: u32,               // rutracker default 50
    pub total_results: Option<u32>,
    pub search_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comment {
    pub post_id: u64,
    pub author: String,
    pub date: String,                 // raw rutracker string; locale-dependent
    pub text: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TopicMetadata {
    pub imdb_rating: Option<f32>,
    pub kinopoisk_rating: Option<f32>,
    pub imdb_url: Option<String>,
    pub kinopoisk_url: Option<String>,
    pub year: Option<u16>,
    pub countries: Vec<String>,
    pub genres: Vec<String>,
    pub director: String,
    pub cast: Vec<String>,
    pub duration: String,             // "01:37:58"
    pub release_type: String,         // "BDRip 1080p"
    pub video: String,                // "MPEG-4 AVC, 1920x804, 23.976 fps, 15.5 mbps"
    pub audio: Vec<String>,           // one entry per "Аудио N" line
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopicDetails {
    pub topic_id: u64,
    pub title: String,
    pub magnet_link: String,
    pub size: String,
    pub seeds: u32,
    pub leeches: u32,
    pub description: String,
    pub file_list: Vec<String>,
    pub metadata: Option<TopicMetadata>,
    pub comments: Vec<Comment>,
    pub comment_pages_fetched: u32,
    pub comment_pages_total: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategoryGroup {
    pub group_id: String,             // e.g. "c-36"
    pub title: String,
    pub forums: Vec<ForumCategory>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForumCategory {
    pub forum_id: String,
    pub name: String,
    pub parent_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TorrentFile {
    pub bytes: Vec<u8>,
    pub filename: String,             // resolved per §7
}
```

## 6. CLI Surface (`rutracker` binary)

`clap` derive-based, subcommands mirror core capabilities.

```
rutracker <command> [options]

Commands:
  search <query>           Search for torrents
    --category <id>          Forum category filter
    --sort <field>           seeders | size | downloads | registered (default: seeders)
    --order <dir>            desc | asc (default: desc)
    --page <n>               1-based pagination (default: 1)
    --min-seeds <n>
    --max-size-gb <n>
    --year-from <yyyy>
    --year-to <yyyy>

  topic <id>               Get topic details
    --comments               Include comment pages (default: off)
    --max-comment-pages <n>  Cap on comment pages fetched (default: 1)

  browse <category-id>     List a forum without a query
    --page <n>
    --sort <field> --order <dir>

  categories               List forum directory
    --refresh                Bypass local cache

  download <id>            Save the .torrent file
    --out-dir <path>         Must resolve under $HOME or CWD

Global options (on all commands):
  --format json|text         Default json
  --out FILE                 Write output to FILE instead of stdout
  --profile NAME             Brave profile name (default: "Peter")
  --cache-dir PATH           Override cookie/categories cache dir
```

### 6.1 CLI filename resolution for `download` (same rules as before)
1. `Content-Disposition: attachment; filename="..."` — parsed via `reqwest` header + stdlib
2. Sanitised topic title (`[^A-Za-z0-9._-]` → `_`, trimmed to 120 chars), suffixed with `.torrent`
3. `topic-{id}.torrent`

### 6.2 CLI path policy for `download`
Default: `--out-dir` must resolve under `dirs::home_dir()` or `std::env::current_dir()`. Otherwise exits with code 2 and prints the rejected path.

**Override:** `--allow-path` flag disables the sandbox (needed for `/Volumes/...`, external drives, etc.). Documented in README and CHANGELOG as explicit opt-in since the current Python implementation has no such restriction.

## 7. MCP Surface (`rutracker-mcp` binary)

Uses `rmcp` (official Rust MCP SDK). Tools mirror CLI commands but return text:

| Tool | Signature | Returns |
|---|---|---|
| `search` | `search(query, category?, sort_by?, order?, page?, min_seeds?, max_size_gb?, year_from?, year_to?)` | text (legacy format preserved for the base arg set) |
| `get_topic` | `get_topic(topic_id, include_comments?, max_comment_pages?)` | text |
| `browse_forum` | `browse_forum(category_id, page?, sort_by?, order?)` | text |
| `list_categories` | `list_categories(refresh?)` | text — `[group_id] Title` with indented `  [forum_id] Name` |
| `download_torrent` | `download_torrent(topic_id, dest_dir)` | text — absolute path of saved file |

**Text-format policy:** MCP tools serialize via core's `Display` impls (or dedicated text formatters in the `mcp` crate). The MCP binary never returns JSON. JSON is the CLI's job.

## 8. Skill Document (`SKILL.md`)

A Claude Code skill that steers the agent between CLI and MCP:

- **Use MCP tools** for interactive discovery: one search, one topic fetch, a quick category listing. Outputs small, reasoning depends on them.
- **Use the `rutracker` CLI via Bash** for bulk: fetching comments for many topics, dumping a whole category, cross-topic analysis. Write to `--out FILE` and read with `Read`. Skill doc includes canonical invocation patterns and a decision tree.

Installed at repo root as `SKILL.md`; loaded by Claude Code's skill mechanism when the user is in this repo.

## 9. Cookie Extraction (macOS / Brave)

Replaces `pycookiecheat`. Flow:

1. Read `~/Library/Application Support/BraveSoftware/Brave-Browser/Local State` (JSON) to resolve the profile directory by display name.
2. Open `<profile>/Cookies` with `rusqlite` (SQLite). Filter `host_key LIKE '%rutracker.org'`.
3. For each encrypted value (`v10`-prefixed or `v11`-prefixed):
   - Strip 3-byte prefix.
   - Derive AES-128 key via PBKDF2(HMAC-SHA1, password=keychain_value, salt=`b"saltysalt"`, iterations=1003, key_len=16).
   - Decrypt AES-128-CBC, IV=16 bytes of space (`b" " * 16`).
   - Strip PKCS#7 padding.
4. Keychain value lookup via `security-framework` crate:
   - `SecKeychain::default()` → `find_generic_password(service="Brave Safe Storage", account="Brave")`.
   - First call prompts the user; subsequent calls within the Keychain session are silent.
5. Cache decrypted cookies to `.cookies.json` (same format as current Python version, gitignored) so subsequent runs skip the keychain entirely until expiry.
6. On login-redirect detection during a fetch, auto-refresh cookies once and retry (same recovery as today).

**`bb_dl_key` requirement:** asserted before any `dl.php` call. Missing → one refresh attempt → clear error.

Crates: `rusqlite` (bundled), `aes`, `cbc`, `pbkdf2`, `hmac`, `sha1`, `security-framework`, `serde_json`, `dirs`.

## 10. Implementation Phases

Seven milestones: five sequential phases (1–5) plus a split Phase 6 (6a ship + tag, 6b delete-after-soak). Each ends with `cargo test --workspace`, `cargo clippy --workspace -- -D warnings`, and `cargo fmt --all -- --check` green before the next starts.

### Phase 1 — Workspace + parser crate + fixture tests + legacy snapshot capture (day-1 deliverable)

**Deliverables:**
- `Cargo.toml` workspace with 5 member crates (`parser`, `http`, `cookies-macos`, `cli`, `mcp`). `rust-toolchain.toml` pins stable.
- `.gitignore` for `target/`.
- Fixtures copied from `.omc/research/` into `crates/parser/tests/fixtures/`.
- **Day-1 legacy-snapshot capture (de-risks Phase 5):** run the existing Python MCP against known-good inputs; save `legacy-search.txt` and `legacy-get-topic.txt` into `crates/parser/tests/fixtures/legacy-snapshots/` and commit. Locks the snapshot target before Python can rot or cookies expire. Also commit `crates/parser/tests/fixtures/topic-sample-expected.json` (first-comment golden values used by Phase 2).
- Parser work: implement `parse_search_page`, `parse_forum_index`, `parse_topic_page` (structure only; metadata + comments in Phase 2) using `scraper`.

**Acceptance (mechanical):**
- `cargo test -p rutracker-parser` exits 0 with ≥ 8 tests passing (stdout contains `test result: ok. N passed; 0 failed`). Tests marked `#[ignore]` (live-network) are expected in the "ignored" count; `0 ignored` is not a gate — use `cargo test -- --ignored` separately for live smoke.
- `cargo clippy --workspace -- -D warnings` exits 0.
- `cargo fmt --all -- --check` exits 0.
- Named test `parser::forum_index::tests::test_group_count_26` prints `ok` and asserts 26 `CategoryGroup`s from `index-sample.html`.
- Named test `parser::forum_index::tests::test_forum_count_gte_280` prints `ok` and asserts ≥ 280 `ForumCategory`s. (Phase 1 empirical correction: the 321 raw `viewforum.php?f=` anchor count on `index-sample.html` includes ~41 top-nav shortcuts and cross-references; the true hierarchical subforum count under `div.category` groups is 280. The plan's earlier "≥ 300" estimate was based on the raw link count.)
- Named test `parser::search::tests::test_row_count_50_and_search_id_present` prints `ok`.
- Named test `parser::search::tests::test_row0_author_matches_known_value` prints `ok` (asserts `row[0].author == "<value-recorded-in-test-source-citing-fixture-line>"` — not just non-empty).
- `ls crates/parser/tests/fixtures/legacy-snapshots/{legacy-search.txt,legacy-get-topic.txt}` exits 0; both files are non-empty (`wc -c` > 0) and valid UTF-8 (`iconv -f utf-8 -t utf-8 FILE > /dev/null` exits 0).
- `ls crates/parser/tests/fixtures/topic-sample-expected.json` exits 0.

### Phase 2 — Parser: comments + metadata + text_format

**Deliverables:** `parse_comments` (returns 29 entries — opening post excluded), `parse_topic_metadata` covering IMDb/KP ratings + URLs, year, countries, genres, director, cast, duration, release_type, video, audio[]. `text_format` module with `Display` impls reused by both binaries.

**Acceptance (mechanical):**
- Named test `parser::topic::tests::test_parse_comments_count_29` prints `ok` (29 `Comment`s, opening post excluded).
- Named test `parser::topic::tests::test_comment0_matches_expected_json` prints `ok` (author + date + 80-char text snippet match `topic-sample-expected.json`).
- Named test `parser::metadata::tests::test_extracts_year_kp_video_audio` prints `ok` (asserts `year == Some(2026)`, `kinopoisk_url` is `Some(…)`, `video` non-empty, `audio.len() >= 1`).
- Named test `parser::text_format::tests::test_legacy_get_topic_byte_equal` prints `ok` (asserts byte-equal to `legacy-get-topic.txt`).
- Named test `parser::text_format::tests::test_legacy_search_byte_equal` prints `ok` (asserts byte-equal to `legacy-search.txt`).

### Phase 3 — HTTP client + Brave cookies

Reordered after parsers per architectural review: parsers are pure and fixture-only; cookies are the riskiest module. Swap lets parser work finish even if Keychain debugging takes a week.

**Deliverables:** `RuTrackerClient` on `reqwest`, cp1251 decoding via `encoding_rs`, cookie store, login-redirect detection + auto-refresh. `cookies-macos` crate per §9.

**Acceptance (mechanical):**
- Named test `http::tests::test_cp1251_decode_and_parse` prints `ok` (wiremock-backed; stubbed cp1251 body).
- Named test `http::tests::test_302_login_triggers_one_refresh` prints `ok` (wiremock 302 → login.php).
- Named test `http::tests::test_dl_returns_bytes` prints `ok` (binary response preserved).
- Named test `cookies_macos::tests::test_decrypt_vector` prints `ok` — encrypts a known plaintext with Chromium parameters (IV=16 spaces, PBKDF2-SHA1, 1003 rounds, key=`"peanuts"` test vector), decrypts with `cookies_macos::decrypt`, asserts byte-equal.
- Named test `cookies_macos::tests::test_decrypt_python_captured_cookie` prints `ok` — decrypts a committed `v10`-prefixed cookie blob captured from the existing Python `pycookiecheat` output and asserts it matches the plaintext Python produced.
- Named test `cookies_macos::tests::test_assert_dl_key_missing_returns_error` prints `ok` (synthetic cookie map without `bb_dl_key` triggers a distinctive `Error::MissingDlKey`).
- Integration test `cookies_macos::tests::test_load_brave_cookies_contains_bb_session` is `#[ignore]`-marked (requires live Keychain); manual smoke in Phase 6a release checklist — NOT a gating acceptance.
- Manual smoke in README: `rutracker search 2026 --category 252` returns live results — recorded as README snippet, NOT a phase-gating acceptance.

### Phase 4 — CLI binary

**Deliverables:** `rutracker` binary with all subcommands from §6, `clap` derive config, JSON output default via `serde_json`, `--format text` via `text_format` (shared, not duplicated), `--out FILE` writer, `--allow-path` override for download.

**Acceptance (mechanical — `cargo test -p rutracker --test cli_integration`):**
- `test_search_json_parsable_by_jq`: runs `rutracker search 2026 --category 252 --format json` under a wiremock-stubbed HTTP base-URL, pipes stdout through `jq '.results | length'`, asserts `>= 1`.
- `test_topic_comments_count_via_jq`: runs `rutracker topic 6843582 --comments --max-comment-pages 1 --out $TMPDIR/topic.json`, then `jq '.comments | length' $TMPDIR/topic.json` prints `29`.
- `test_path_policy_rejects_etc`: runs `rutracker download 6843582 --out-dir /etc` (wiremock-stubbed), asserts exit code 2 and stderr contains the rejected path.
- `test_path_policy_allow_override`: same but with `--allow-path`, asserts exit 0 and the file exists under `/etc/` (cleaned up in teardown; test runs under a sandbox dir via `$XDG_RUNTIME_DIR`-style mock).
- `test_download_default_path_succeeds`: `rutracker download 6843582 --out-dir $HOME/tmp-rutracker`, asserts file created and `file $HOME/tmp-rutracker/*.torrent` output contains `data`.
- `test_missing_bb_dl_key_refreshes_once`: synthetic cookie jar missing `bb_dl_key`, wiremock asserts exactly 1 refresh GET then 1 `dl.php` GET; exit 0.
- `test_browse_and_categories_text_mode`: both `rutracker browse 252 --format text` and `rutracker categories --format text` exit 0 and stdout contains an expected header string (proves CLI text mode wired).

### Phase 5 — MCP binary (time-boxed spike + full build)

**Deliverables:**
- **Hour 0-4: `rmcp` spike.** Register **two tools** (`search` and `get_topic`), serve over stdio, validate a Claude Code stdio handshake. If not working within 4 hours, abandon `rmcp`, switch to hand-rolled stdio JSON-RPC server (target: 200 LOC in `crates/mcp/src/jsonrpc.rs`). Spike outcome logged in `crates/mcp/NOTES.md`.
- After spike: `rutracker-mcp` with all 5 tools, stdio transport, text output via shared `text_format`.

**Acceptance (mechanical):**
- Named test `mcp::tests::test_get_topic_snapshot_byte_equal` prints `ok` (asserts byte-equal to `legacy-get-topic.txt`).
- Named test `mcp::tests::test_search_snapshot_byte_equal` prints `ok` (asserts byte-equal to `legacy-search.txt`).
- Named test `mcp::tests::test_all_five_tools_registered` prints `ok` (asserts tool name list contains all 5 names exactly).
- Named test `mcp::tests::test_stdio_handshake_initialize` prints `ok` (asserts MCP `initialize` request returns protocol version).
- `diff <(grep '"command"' .mcp.json) <(echo '  "command": "rutracker-mcp"')` exits 0 (proves `.mcp.json` edited to Rust binary).

### Phase 6a — Ship Rust, tag final Python release (rollback target)

**Deliverables:**
- Commit 1: add Rust binaries, update `.mcp.json`, README section "Rust installation", CHANGELOG `1.0.0-rc1`. Python still on disk.
- Tag `v0.9.0-final-python` on the last commit that contains working Python. This is the bisect / rollback target if Rust regresses post-merge.

**Acceptance (mechanical):**
- `git tag -l v0.9.0-final-python` prints `v0.9.0-final-python` (proves rollback tag exists).
- At the tagged commit, `ls src/rutracker_mcp/server.py` exits 0 AND `ls crates/mcp/src/main.rs` exits 0 (both implementations present for manual rollback; `.mcp.json` chooses which is live).
- Release checklist executed: `bash scripts/soak.sh` (runs 20 random topic fetches against live rutracker, logs to `soak-<date>.log`) exits 0 before this phase is signed off.
- `diff <(grep '"command"' .mcp.json) <(echo '  "command": "rutracker-mcp"')` exits 0 (Rust binary is live in `.mcp.json`).

### Phase 6b — Delete Python (after soak period)

**Deliverables** (after a ≥ 7-day soak using the Rust binaries for real work):
- Separate commit deletes `src/rutracker_mcp/`, removes `[tool.pixi.*]` sections from `pyproject.toml` and deletes the whole file, plus `pixi.lock` and `.pixi/`. (No standalone `pixi.toml` exists — pixi config currently lives under `[tool.pixi.*]` inside `pyproject.toml`.)
- `SKILL.md` with the CLI-vs-MCP decision tree and canonical invocations.
- `CHANGELOG.md` `1.0.0` entry finalised.
- README mentions no Python / pixi.

**Acceptance (mechanical):**
- `rg --type py -l . | wc -l` prints `0`.
- `ls pyproject.toml pixi.lock .pixi` all exit non-zero (files gone).
- `cargo install --path crates/cli --locked` and `cargo install --path crates/mcp --locked` both exit 0 on a clean macOS machine (CI job `install-smoke`).
- Named test `mcp::tests::test_skill_md_present` asserts `SKILL.md` exists at repo root and contains literal strings `"interactive via MCP"` and `"bulk via CLI"` and at least one `rutracker` and one MCP-tool invocation code block.
- `grep -c "^##* 1.0.0" CHANGELOG.md` prints `>= 1`.
- `git log --oneline v0.9.0-final-python..HEAD -- src/rutracker_mcp/` is empty (no Python edits between tag and HEAD — deletion is a single atomic commit).

## 11. Backward Compatibility

Breaking change. The Python MCP is removed in Phase 6. Mitigations:
- Phase 5 captures a byte-identical snapshot of the legacy `get_topic` / `search` MCP output before Python removal, and the Rust MCP tests against it.
- `.mcp.json` change is a one-line edit.
- Cookie cache file (`.cookies.json`) keeps the same shape so the rewrite can read Python-era caches without re-prompting the Keychain.

## 12. Risks & Mitigations

| Risk | Mitigation | Owner | Proof of closure |
|---|---|---|---|
| Brave cookie decrypt has platform-specific edge cases (v10 vs v11 prefixes, padding) | Phase 3 isolates the decrypt path behind a trait; vector-tested against known-good cookies captured from Python's `pycookiecheat` output | Phase 3 implementer | `cookies_macos::tests::test_decrypt_vector` + `test_decrypt_python_captured_cookie` pass |
| `rmcp` pre-1.0 API churn | Phase 5 starts with 4-hour 2-tool spike; fallback = hand-rolled stdio JSON-RPC (~200 LOC) in `crates/mcp/src/jsonrpc.rs` | Phase 5 implementer | `test_stdio_handshake_initialize` passes on `rmcp = "0.16"`, OR `NOTES.md` records fallback + `test_jsonrpc_handshake` passes |
| `scraper` / `html5ever` less forgiving of phpBB's messy HTML | Fixture tests centralised in `crates/parser`; `scripts/soak.sh` release-gate run against 20 live topic IDs | Phase 6a release driver | `cargo test -p rutracker-parser` passes; `soak-<date>.log` committed in Phase 6a |
| cp1251 decoding corner cases (`dl.php` returns binary) | `encoding_rs::WINDOWS_1251` for text; `_get(decode=false)` path for bytes | Phase 3 implementer | `http::tests::test_cp1251_decode_and_parse` + `test_dl_returns_bytes` pass |
| `bb_dl_key` cookie absent in current cache | `assert_dl_key` precheck + one `refresh_cookies` retry; clear error otherwise | Phase 3 implementer | `test_assert_dl_key_missing_returns_error` + `test_missing_bb_dl_key_refreshes_once` pass |
| MCP payload bloat with comments | Per-comment truncation 1500 chars; `max_comment_pages` default = 1 | Phase 2 implementer | Snapshot asserts `get_topic(t, include_comments=true)` stdout length < 70 000 bytes |
| Forum-categories cache stale (new subforums) | TTL 7 days in `.categories.json`; `list_categories(refresh=true)` bypasses | Phase 3 implementer | Integration test round-trips TTL expiry and re-fetches |
| Pagination cost (1 HTTP request per page) | Opt-in `max_comment_pages` / `page` params; low defaults | Phase 4 implementer | `test_search_pagination_request_count` asserts expected request count |
| Path traversal via `download --out-dir` | Default sandbox under `$HOME`/CWD; explicit `--allow-path` opt-out | Phase 4 implementer | `test_path_policy_rejects_etc` + `test_path_policy_allow_override` pass |
| Login redirect during long loop | Centralised `_get` cookie-refresh path; short-circuit + clear error | Phase 3 implementer | `test_302_login_triggers_one_refresh` passes |
| Pre-existing `u-link` author selector defect | Phase 1 regression test asserts specific known-good author value | Phase 1 implementer | `test_row0_author_matches_known_value` passes |
| Phase 6b irreversibility (no Python bisect target) | Phase 6a tag `v0.9.0-final-python` before deletion; 7-day soak required | Phase 6a release driver | `git tag -l v0.9.0-final-python` non-empty; Phase 6b commit message references tag + soak-end date |
| Keychain prompt fatigue for developers | Cookie cache persists across runs; default `cargo test` uses fixtures, not live cookies | All | Developer can run `cargo test --workspace` without Keychain prompts; documented in README |
| Rust compile time slows iteration | `cargo watch -x check` in dev; `cargo-nextest` optional for parallel runs | Ongoing | `CONTRIBUTING.md` documents both tools |
| Cross-compilation to Linux/Windows out of scope | README documents "macOS-only today"; cookies extraction is cfg-gated for future platforms | Docs | README section "Platform support" lists macOS-only and links to issue tracker |

## 13. Dependencies (workspace `Cargo.toml`)

```toml
[workspace]
members = ["crates/parser", "crates/http", "crates/cookies-macos", "crates/cli", "crates/mcp"]
resolver = "2"

[workspace.package]
edition = "2021"
rust-version = "1.75"
license = "MIT"

[workspace.dependencies]
# HTTP + parsing
tokio = { version = "1", features = ["rt-multi-thread", "macros", "fs", "time"] }
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "cookies", "gzip", "stream"] }
scraper = "0.20"
encoding_rs = "0.8"
regex = "1"

# Serialization
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# CLI
clap = { version = "4", features = ["derive", "env"] }

# MCP
rmcp = "0.16"                               # official SDK; pin to exact current minor at Phase 1 start

# Cookie extraction (macOS)
rusqlite = { version = "0.32", features = ["bundled"] }
aes = "0.8"
cbc = "0.1"
pbkdf2 = { version = "0.12", features = ["simple"] }
hmac = "0.12"
sha1 = "0.10"
security-framework = "3"                    # macOS keychain
dirs = "5"

# Errors + utilities
thiserror = "1"
anyhow = "1"
chrono = { version = "0.4", features = ["serde"] }

# Observability
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt"] }

# Testing
wiremock = "0.6"
pretty_assertions = "1"
proptest = "1"               # property-based tests for regex-heavy parsers

# Dev tooling (not a Cargo dep — installed via `cargo install`)
# cargo-nextest                 for fast parallel test runs; documented in CONTRIBUTING, not required for `cargo test`
```

## 14. Prior Art (reference only, not deps)

| Source | Consult for | Phase |
|---|---|---|
| [`GvozdevAD/py_rutracker`](https://github.com/GvozdevAD/py_rutracker) | `dl.php` flow, pagination URL reconstruction, search-form field enumeration | 2, 4 |
| [`Dascienz/phpBB-forum-scraper`](https://github.com/Dascienz/phpBB-forum-scraper) | phpBB-canonical selectors cross-check | 3 |
| [`robatipoor/chromium-cookie-decrypt-rs`](https://crates.io/) category / existing Chromium cookie-decrypt Rust crates | Cookie AES-CBC decrypt patterns | 3 |
| RuTracker runs **TorrentPier** (phpBB 2.x fork); URL + cookie fingerprint confirmed | Confirms selector stability | all |

## 15. Definition of Done

Every DoD item maps to a specific phase acceptance test.

| DoD item | Proven by |
|---|---|
| All five crates build release | `cargo build --workspace --release` exits 0 (Phase 6a `install-smoke` CI job) |
| All tests pass | `cargo test --workspace` exits 0, stdout shows `0 failed` for default profile (Phase 6b gate). Live-marked tests carry `#[ignore]` and are opt-in via `cargo test -- --ignored`; the "ignored" count in default output is expected and not a gate |
| Lint + format clean | `cargo clippy --workspace -- -D warnings` and `cargo fmt --all -- --check` exit 0 (Phase 1 acceptance, re-checked each phase) |
| `rutracker` CLI covers all 5 commands with JSON and text outputs | Phase 4 acceptance tests `test_search_json_parsable_by_jq`, `test_topic_comments_count_via_jq`, `test_browse_and_categories_text_mode`, `test_download_default_path_succeeds` |
| `rutracker-mcp` exposes 5 tools, passes legacy snapshot | Phase 5 `test_all_five_tools_registered`, `test_get_topic_snapshot_byte_equal`, `test_search_snapshot_byte_equal` |
| Brave cookie extraction works + `bb_dl_key` validates + auto-refreshes | Phase 3 `test_decrypt_vector`, `test_decrypt_python_captured_cookie`, `test_assert_dl_key_missing_returns_error`, Phase 4 `test_missing_bb_dl_key_refreshes_once` |
| `SKILL.md` present and referenced | Phase 6b `test_skill_md_present` + `grep -q "SKILL.md" README.md` |
| No Python in repo | Phase 6b acceptance: `rg --type py -l . \| wc -l` prints `0`, `pyproject.toml`/`pixi.lock`/`.pixi` absent |
| `.mcp.json` points at Rust binary | Phase 5 acceptance: `diff` check on `.mcp.json` `"command"` field |
| `CHANGELOG.md` `1.0.0` entry present | Phase 6b `grep -c "^##* 1.0.0" CHANGELOG.md` ≥ 1 |
| Release soak passes before cutover | Phase 6a `bash scripts/soak.sh` exits 0, `soak-<date>.log` committed |

## 16. RALPLAN-DR Summary (deliberate mode)

### 16.1 Principles
1. **CLI-first.** `rutracker` CLI is the primary surface; composable with shell, files, jq, CI, non-Claude agents. MCP is a thin shim, never the primary contract.
2. **Shared core, binaries as thin adapters.** Both binaries depend on the same three library crates (`rutracker-parser`, `rutracker-http`, `rutracker-cookies-macos`). Text formatting lives in `rutracker-parser::text_format` and is reused by CLI `--format text` and MCP tool output — no duplicated presentation logic.
3. **Rust-only distribution; no external runtime on user machines.** `cargo install` produces self-contained executables (two of them — CLI + MCP — not a single binary). No Python/node/pixi/conda required to run.
4. **Fixture-tested parsers.** Every CSS selector is locked against a real HTML fixture; default test run never touches the live network.
5. **Clean break, no dual maintenance.** Existing Python is reference during the port, deleted at end of Phase 6.

### 16.2 Decision Drivers
1. The triggering use case is **bulk comment ingest** (audience analysis across many topics) — exactly the context-bloat regime where MCP loses to a file-backed CLI.
2. **Distribution & reusability**: a single binary runs in shell, scripts, CI, cron, other agents. The Python+pixi+MCP stack is Claude-only.
3. **Explicit user mandate**: "Rust all-in, ALL-IN, no Python."

### 16.3 Viable Options
| Option | Description | Status | Reason |
|---|---|---|---|
| **A** | Rust workspace, CLI + MCP, Python deleted | **CHOSEN** | Matches all three drivers |
| B | Keep Python; extend to full-scope parity (5 tools incl. comments/metadata/categories/download/pagination), repair existing `u-link`-style parser defects, add Python CLI (`click`) on the shared core | Rejected | Violates user mandate ("no Python"). Honest full-scope estimate: 3–5 days (parity with Rust plan), not 1 day — the current Python baseline only ships 2 of the 5 tools and has the preexisting parser defects documented in §4. Independent merit: no new platform risk (no Chromium crypto port, no `rmcp` dep). Genuine cost advantage over Option A: preserved here for audit honesty even though rejected |
| C | Rust CLI + Python MCP shim invoking CLI via subprocess | Rejected | Dual runtime contradicts "no Python"; two-language ops complexity; adds fork/exec overhead per tool call |
| D | Pure Rust CLI, no MCP at all | Rejected | Loses zero-config Claude Code integration. MCP shim cost estimate: ~50–150 LOC per tool once `text_format` is shared (handler + argument deserialization + error mapping). Cheap enough to be additive, not a standalone effort |

### 16.4 Pre-Mortem (3 scenarios)
1. **`rmcp` 0.x API churn breaks Phase 5.** Probability: medium (pre-1.0 SDK, currently 0.16). Impact: MCP binary blocked. Mitigation: Phase 5 starts with a 2-tool spike (search + get_topic) to validate stability before the full port; fallback is hand-rolled stdio JSON-RPC (~200 LOC). The CLI binary is fully usable without the MCP binary, so this risk does not block end users. **Owner:** Phase 5 implementer. **Proof:** either `cargo test -p rutracker-mcp test_stdio_handshake_initialize` passes on `rmcp = "0.16"`, or `crates/mcp/NOTES.md` records the fallback switch and `test_jsonrpc_handshake` passes.
2. **Brave cookie AES-CBC decrypt produces garbage on the developer's machine.** Probability: medium. Chromium uses two cookie encryption versions (`v10` ASCII-prefix and `v11` host-bound); macOS Keychain may have multiple "Safe Storage" entries depending on Brave install age. Impact: zero usable cookies → no auth → no scraping. Mitigation: **Phase 3** starts with a vector test against a known-good cookie captured by the existing `pycookiecheat` path; if decrypt diverges from Python's output, debug before any client code lands. Fallback: read the existing `.cookies.json` from the Python era as a bootstrap path. **Owner:** Phase 3 implementer. **Proof:** `test_decrypt_vector` + `test_decrypt_python_captured_cookie` both pass; if either fails, the bootstrap-cache path is exercised by `test_load_cookies_falls_back_to_cache`.
3. **`scraper` / `html5ever` rejects malformed phpBB HTML that `lxml` previously tolerated.** Probability: low-medium. Impact: parser tests pass on the 3 static fixtures but fail in the wild. Mitigation: a **manual release-gate soak script** (`scripts/soak.sh`, invoked by the developer, not by `cargo test`) fetches 20 random topic IDs and asserts no panics + non-empty parsed results. **Owner:** Phase 6a release driver. **Proof:** `soak-<date>.log` committed in the Phase 6a release commit, stdout ends with `All 20 topics parsed successfully`. Kept out of the default test run to preserve principle 16.1.4.

### 16.5 Expanded Test Plan
| Layer | Tooling | Default in `cargo test`? | Notes |
|---|---|---|---|
| Unit | `cargo test`, `pretty_assertions` | yes | Every `parse_*` function: positive + negative assertion against `tests/fixtures/*.html`. Pure functions: HTML in, struct out, no I/O. |
| Integration | `wiremock` | yes | `RuTrackerClient` end-to-end with stubbed cp1251 responses; exercises 302→login refresh path. |
| E2E (live) | `--features live -- --ignored` | no (opt-in) | `rutracker search` + `rutracker topic` against rutracker.org. Smoke-only, manual run, documented in README. |
| Snapshot | Custom test asserting byte-equal strings | yes | **Phase 1 day-1** captures `legacy-search.txt` and `legacy-get-topic.txt` from the Python MCP (while still known-good) into `crates/parser/tests/fixtures/legacy-snapshots/`. Phase 2 asserts `text_format` produces byte-equal output on the legacy arg set; Phase 5 asserts the MCP binary preserves it end-to-end. Early capture decouples the test from Python's continued health. |
| Observability | `tracing` + `tracing-subscriber` (declared in §13) | n/a (instrumentation) | INFO on tool calls + URL fetches, DEBUG on parsing branches. `RUST_LOG=rutracker=debug` documented in README. |
| Property-based | `proptest` (declared in §13) | yes (cheap) | Year regex `\[(\d{4}),` against random titles — catches pathologies. |

Dev tooling for fast iteration (not a Cargo dep, installed separately, documented in CONTRIBUTING):
- `cargo-nextest` — parallel test runner; both `cargo test` and `cargo nextest run` are supported, CI uses `cargo test`.

## 17. Out-of-Scope (revisit later)

- Linux / Windows cookie extraction.
- Chrome / Firefox / Safari cookie extraction.
- Writing/posting on rutracker.
- A prebuilt binary release pipeline (GitHub Releases + `cargo-dist`) — manual `cargo install` suffices for v1.0.
- A structured-JSON output mode for MCP tools (single text-format policy retained).
- A higher-level "discover films above an IMDb threshold" skill — belongs in a separate repo consuming this CLI.
