# RuTracker Local Mirror — Incremental Sync Plan

**Date:** 2026-04-18
**Scope:** Add a local mirror of rutracker content to the Rust workspace: forum-tree snapshot, user-maintained watchlist, directory-per-forum + JSON-per-topic storage, and a `sync` command that fetches only deltas.
**Mode:** Deliberate (auto-enabled — scraping at scale has real ban risk, filesystem consistency matters, schema evolution is guaranteed).

---

## 0. Architecture Decision Record

| Field | Value |
|---|---|
| **Decision** | New `rutracker-mirror` crate + `rutracker mirror …` CLI subcommands. **Hybrid storage**: one JSON file per topic on disk (human-readable, the user's explicit ask) **plus one SQLite file** (`state.db`, WAL mode) for the sync index and per-forum meta. Topic JSONs remain the source of truth and the archival artefact; SQLite is a rebuilt-from-scratch-if-lost index for fast delta detection. User-edited `watchlist.json` gates which forums sync. Default root `$HOME/.rutracker/mirror/` (override via `--root` or `RUTRACKER_MIRROR_ROOT`). |
| **Drivers** | (1) Directory + JSON matches the user's mental model ("форум = каталог, топик = JSON"). (2) Incremental deltas — never re-download what we already have. (3) Start tiny (2 forums — Фильмы 2025 / Фильмы 2026) and grow the watchlist over time. |
| **Alternatives considered** | B: single SQLite DB with blobs (no on-disk JSON). C: Git-tracked JSON tree. D: mirror raw HTML. E: JSON-only (no SQLite) with rewritten `index.json` — architect rejected due to write amplification at 10k+ topics. |
| **Why chosen** | B rejected: user explicitly asked for file-per-topic. C rejected: commit churn on 10k+ files; git ops slow. D rejected: can't merge new comments into an opaque HTML blob. E rejected: per-topic rewrite of a 1–2 MB index file is O(n) amplification — hybrid with SQLite (`rusqlite` with `bundled` feature, currently a target-specific dep of `cookies-macos`; promoted to a workspace dep in Phase M1) gives O(1) upsert per topic with WAL crash safety while preserving JSON-on-disk archival. |
| **Consequences** | (+) Human-readable, `grep`-able topic JSONs. (+) Resume-safe index via SQLite WAL. (+) Trivial to rebuild the SQLite index from the on-disk JSONs if it's lost. (+) Small watchlists are cheap. (−) Two storage layers to keep in sync (index + topic JSON); mitigated by making the SQLite index a *derived* cache with an explicit `rebuild-index` command. (−) Schema evolution remains a concern (§11 scenario 2). |
| **Follow-ups** | Mirroring attachments / images (out of scope v1). Local full-text search — cheap to add on top of SQLite via FTS5 later, not v1. Torrent-file mirror (use existing `download_torrent`). |

---

## 1. Principles

1. **JSON-per-topic on disk is the archive.** The human-readable topic JSON is the source of truth. SQLite is a derived cache and can be rebuilt from the JSONs at any time via `rutracker mirror rebuild-index`.
2. **Resume-safe at topic granularity.** Sync can be interrupted between topics without loss. Within a single topic fetch that spans multiple comment pages, we commit the merged result only after all pages succeed; a 429 mid-topic leaves the previously stored version untouched.
3. **Polite by default.** Rate-limited single-threaded requests. On 429/503, mark a per-forum `cooldown_until` timestamp and skip that forum on subsequent runs until the cooldown expires (403 is auth failure, not rate-limit — see §5.2/§5.4). Ban avoidance is a pre-mortem-level concern (§11).
4. **Idempotent on no-upstream-change.** `sync` with no upstream changes writes zero topic files and does O(forum_count) network requests (one forum-listing page each). When upstream changed, the cost is O(deltas + forum_count).
5. **Platform-neutral mirror crate.** `rutracker-mirror` takes a preloaded `rutracker_http::Client` via dependency injection — it does **not** depend on `rutracker-cookies-macos`. The CLI wires cookies in; mirror logic runs anywhere Rust runs.

## 2. Decision Drivers

1. **User's stated mental model:** forum = directory, topic = one JSON file, post = JSON element.
2. **Delta-only sync:** explicit goal ("докачал бы дельты"); "не хочу скачивать весь веб-сайт".
3. **Explicit watchlist:** user wants to enumerate what to track; everything else is ignored.

## 3. Viable Options

| Option | Description | Status | Reason |
|---|---|---|---|
| **A** | **Hybrid**: JSON-per-topic on disk + SQLite `state.db` for index/meta | **CHOSEN** | Matches the user's mental model; avoids write-amplification in option E |
| B | SQLite DB for everything incl. topic text blobs | Rejected | User asked for file-per-topic explicitly |
| C | Git repo as the mirror; every sync is a commit | Rejected | Diff noise on bulk adds; git slow on 100k+ files |
| D | Cache raw HTML blobs; parse on read | Rejected | Can't merge new comments into opaque HTML |
| E | JSON-only with single `index.json` rewritten per insert | Rejected (architect R1) | O(n) write amplification at 10k+ topics. Keeps the JSON-only aesthetic but burns SSDs for no benefit over option A |

## 4. Storage layout

```
$HOME/.rutracker/mirror/                           # $RUTRACKER_MIRROR_ROOT override
├── structure.json                                  # full forum tree snapshot (~100 KB, ~280 forums)
├── watchlist.json                                  # user-edited: which forums to sync
├── state.db                                        # SQLite (WAL mode) — sync index + per-forum meta
├── state.db-wal                                    # WAL sidecar (transient)
├── state.db-shm                                    # SHM sidecar (transient)
├── .lock                                           # advisory file lock for concurrent-run guard
└── forums/
    └── 252/                                        # forum_id directory (human-readable archive)
        └── topics/
            ├── 6843582.json                        # one topic = description + metadata + comments
            ├── 6843065.json
            └── …
```

### 4.1 File formats

Every JSON file has a top-level `schema_version: 1` field so future migrations are unambiguous.

**`structure.json`** — `Vec<CategoryGroup>` serialized from `parser::CategoryGroup`, plus `fetched_at` timestamp.

**`watchlist.json`**
```json
{
  "schema_version": 1,
  "forums": [
    {"forum_id": "252", "name": "Зарубежные фильмы 2026", "added_at": "2026-04-18T15:00:00Z"},
    {"forum_id": "251", "name": "Зарубежные фильмы 2025", "added_at": "2026-04-18T15:00:00Z"}
  ]
}
```

**`state.db` — SQLite schema (WAL mode, `synchronous=NORMAL`, `journal_mode=WAL`):**
```sql
CREATE TABLE schema_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
-- Seeded with ('schema_version', '1') on init.

CREATE TABLE forum_state (
    forum_id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    parent_group_id TEXT,
    last_sync_started_at INTEGER,        -- unix millis
    last_sync_completed_at INTEGER,
    last_sync_outcome TEXT,              -- 'ok' | 'rate_limited' | 'login_required' | 'error' | 'running'
    cooldown_until INTEGER,              -- unix millis, skip forum until then (rate-limit backoff)
    topic_high_water_mark INTEGER NOT NULL DEFAULT 0,
    topics_count INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE topic_index (
    forum_id TEXT NOT NULL,
    topic_id INTEGER NOT NULL,
    title TEXT NOT NULL,
    last_post_id INTEGER NOT NULL,
    last_post_at TEXT NOT NULL,          -- rutracker's opaque date string
    fetched_at INTEGER NOT NULL,          -- unix millis
    PRIMARY KEY (forum_id, topic_id)
) WITHOUT ROWID;
CREATE INDEX idx_topic_by_id ON topic_index(topic_id);
```

SQLite is a *derived* index: `rutracker mirror rebuild-index` walks `forums/**/*.json` and reconstructs it from scratch. Losing `state.db` is a performance incident, not a data loss incident.

**`forums/<id>/topics/<topic_id>.json`** — extends `parser::TopicDetails`:
```json
{
  "schema_version": 1,
  "topic_id": 6843582,
  "forum_id": "252",
  "title": "…",
  "magnet_link": "magnet:?…",
  "size": "2.91 GB",
  "seeds": 4728,
  "leeches": 828,
  "description": "…",
  "metadata": { "year": 2026, "kinopoisk_url": "…", … },
  "comments": [
    {"post_id": 89054973, "author": "Dinomaik", "date": "11-Апр-26 07:23", "text": "…"}
  ],
  "fetched_at": "2026-04-18T15:00:05Z",
  "last_post_id": 89054973,
  "last_post_at": "11-Апр-26 07:23"
}
```

Comments are sorted by `post_id` ascending and deduplicated by `post_id`.

### 4.2 Atomicity

**Topic JSON writes**: serialize → write `<path>.tmp` → `File::sync_all` (data + metadata) → `rename <path>.tmp <path>`. On macOS APFS and Linux ext4, rename within the same directory is atomic at the VFS level. True power-loss durability also requires an `fsync` of the parent directory — we call `File::sync_all` on an opened directory handle after the rename. Documented caveat: on network filesystems (NFS, SMB), these guarantees are weaker; the mirror is intentionally macOS-local (§15).

**SQLite writes**: WAL mode with `synchronous=NORMAL`. We group all topic-index upserts for one forum's sync pass in a single transaction, committed only after the last topic JSON in that pass has successfully landed on disk. Order of operations per topic:
1. Fetch + parse + merge.
2. Write `topics/<id>.json` atomically.
3. Insert/update the `topic_index` row in the in-progress transaction.
4. On forum-pass completion, update `forum_state.last_sync_completed_at`, `outcome`, `high_water_mark` and commit.

A crash after (2) but before (4) leaves the new JSON on disk with the old SQLite state. Recovery: at the start of every `sync` we run a **lazy backfill** pass — `SyncEngine::backfill_missing_index_rows(forum_id)` scans `forums/<id>/topics/*.json` and inserts any missing rows into `topic_index`. It is cheap (one `SELECT forum_id, topic_id FROM topic_index WHERE forum_id=?` + a directory listing). For a full reconstruction after `state.db` deletion, `rutracker mirror rebuild-index` runs the same pass across all forums without the SELECT optimisation.

## 5. Sync algorithm

### 5.1 CLI surface

```
rutracker mirror init [--root PATH]                    # create dirs + SQLite schema
rutracker mirror structure                             # refresh structure.json (uses list_categories)
rutracker mirror watch add <forum_id> [--name "…"]
rutracker mirror watch remove <forum_id>
rutracker mirror watch list
rutracker mirror sync [--forum <id>…] [--max-topics N] [--rate-rps F] [--dry-run] [--reparse]
rutracker mirror show <forum_id>/<topic_id>            # pretty-print a cached topic JSON
rutracker mirror status                                # summary: forums, topics, last sync, cooldowns
rutracker mirror rebuild-index                         # rebuild state.db from on-disk JSONs
```

Global flags inherited from the CLI: `--base-url`, `--profile`, `--out`.

### 5.2 Per-forum sync loop

```
for forum in watchlist:
    if cooldown_until(forum) > now(): skip_with_log; continue
    state = SELECT forum_state WHERE forum_id = forum
    page = 1
    consecutive_older_and_known = 0
    STOP_STREAK = 5    # architect R1 fix: don't stop on the first older+known row
    done = false
    while not done:
        rows = fetch viewforum.php?f=<forum>&sort=registered&order=desc&page=<page>
        for row in rows:
            known_idx_row = SELECT topic_index WHERE forum=forum AND topic_id=row.topic_id
            is_older_than_hwm = row.topic_id < state.topic_high_water_mark
            if is_older_than_hwm and known_idx_row is not None and known_idx_row.last_post_id == row.last_post_id:
                consecutive_older_and_known += 1
                if consecutive_older_and_known >= STOP_STREAK:
                    done = true; break
                continue
            consecutive_older_and_known = 0   # reset on any new/changed row
            if known_idx_row is None or row.last_post_id > known_idx_row.last_post_id:
                td = fetch_topic(row.topic_id, all_comment_pages)   # may span pages 1..N
                merged = merge_comments(existing_json(row.topic_id), td)
                write_topic_json_atomic(forum, row.topic_id, merged)
                upsert topic_index (forum, topic_id, title, last_post_id, last_post_at, fetched_at)
        page += 1
        if page > MAX_PAGES: break
        if done: break
    commit transaction
    UPDATE forum_state SET last_sync_completed_at=now, last_sync_outcome='ok', topic_high_water_mark=max(seen)
```

**First-ever sync** of a forum: `topic_high_water_mark = 0`. Walks descending registration order until `--max-topics` (default 500) OR until page cap OR until the forum runs out.

**Steady-state sync:** typically returns after page 1. We stop only after 5 consecutive rows that are both older than hwm AND have an unchanged `last_post_id` matching our index. This tolerates the rutracker moved-topic quirk (a topic can be relocated between forums and re-appear with a lower id in a higher position in the listing) and edited-first-post cases (the listing gets reordered without the topic being "new").

**Rate-limit abort (architect R1 BLOCKING fix):** on 429/503, set `forum_state.last_sync_outcome = 'rate_limited'` and `cooldown_until = now + 1h`. Subsequent `sync` invocations skip the forum until that timestamp. `rutracker mirror status` surfaces cooldowns. (403 is treated as auth failure, not rate-limit: it triggers the login-redirect abort path in §5.4.)

### 5.3 Delta for comments

When a known topic's row shows a fresher `last_post_id`:
- Fetch all comment pages (topic can span multiple, e.g. 30 posts/page on rutracker).
- Merge by `post_id`: union existing stored comments with freshly parsed comments, dedup by `post_id`, sort ascending.
- Edited posts overwrite: if a `post_id` exists in both sets, the freshly parsed one wins.
- Update `last_post_id` / `last_post_at` from the max across merged comments.
- **Write the merged topic JSON only after all comment pages have been fetched successfully.** A 429 on page 3 of 5 leaves the old topic JSON untouched and the forum pass moves on (or aborts per §5.2 cooldown).

**Acknowledged data loss (architect R1 MAJOR #3):** this design captures only the latest text of each post. Prior versions of edited posts are discarded. Users who need audit history must keep external backups. This is intentional — `comment_revisions[]` would double the JSON size on every edit and was rejected as out-of-scope for v1.

### 5.4 Politeness

- Default rate: `1 req/sec`, single-threaded. Configurable via `--rate-rps` or env.
- On 429/503: abort the forum pass immediately; set a fixed `cooldown_until = now + 1h`. We deliberately do NOT honour `Retry-After` — the current `rutracker_http::Client` API returns only body/status, and extending it to expose response headers is out of scope for v1. Fixed cooldown is simpler and safer.
- Custom User-Agent: `rutracker-rs/<version> (+https://github.com/pgagarinov/cc-rutracker-mcp)`.
- On login-redirect: abort the run with actionable error ("cookies expired; run `rutracker` once interactively to refresh").
- Max pages per forum default: 100 (≈ 5000 topics). Hard cap prevents accidental runaway.

## 6. New parsers / HTTP

| File | New / modified | Purpose |
|---|---|---|
| `crates/parser/src/row.rs` | **new** | Shared `parse_topic_row(tr) -> RowCommon` helper. Extracted from `search::parse_search_page` (architect R1 MAJOR #5 fix — two callers, one implementation). |
| `crates/parser/src/search.rs` | modified | Delegates per-row parsing to `row::parse_topic_row`. No behavior change; tests unchanged. |
| `crates/parser/src/forum_page.rs` | **new** | `parse_forum_page(html) -> ForumListing { rows: Vec<ForumRow>, pagination }`. `ForumRow` wraps `RowCommon` and adds `last_post_id` / `last_post_at` from the last-post anchor that `viewforum.php` exposes but `tracker.php` does not. |
| `crates/parser/tests/fixtures/viewforum-sample.html` | **new** | Captured from `viewforum.php?f=252` on implementation day. |
| `crates/parser/tests/fixtures/viewtopic-comments-page2.html` | **new** | Multi-page comment fixture for §5.3 merge tests. |
| `crates/http/src/lib.rs` | unchanged | Already supports `get_text(path, params)`. |
| `crates/cookies-macos` | **not depended on from `mirror`** | Architect R1 MAJOR #4 fix: `mirror` crate receives a preloaded `Client` via DI. Only the `cli` and `mcp` bins link `cookies-macos`. |

## 7. New crate: `rutracker-mirror`

```
crates/mirror/
├── Cargo.toml
├── migrations/             # SQL DDL for state.db schema v1
│   └── 0001_init.sql
├── src/
│   ├── lib.rs              # public API: Mirror::open(root, client), SyncEngine, SyncReport
│   ├── layout.rs           # path helpers: mirror_root, forum_dir, topic_path
│   ├── watchlist.rs        # add/remove/list (watchlist.json)
│   ├── state.rs            # SQLite wrapper: schema init, upsert, queries
│   ├── structure.rs        # refresh structure.json from parser::CategoryGroup
│   ├── atomic.rs           # temp-file + File::sync_all + rename + dir-fsync
│   ├── lock.rs             # advisory filesystem lock (.lock); PID recorded
│   ├── engine.rs           # SyncEngine — the per-forum loop from §5.2
│   ├── migrate.rs          # schema_version check + forward-only migration stub
│   └── error.rs
└── tests/
    ├── fixtures/           # snapshot of a two-forum mirror for golden tests
    └── integration.rs      # wiremock-driven initial + incremental sync
```

Depends on: `rutracker-parser`, `rutracker-http`, `rusqlite` (`bundled` feature, promoted to a workspace dep in M1 — currently a target-specific dep of `cookies-macos` only), `serde`, `serde_json`, `chrono`, `thiserror`, `tracing`, `anyhow`. **Does NOT depend on `rutracker-cookies-macos`** — the mirror receives a preloaded `rutracker_http::Client` via `Mirror::open(root, client)`. This keeps the crate platform-neutral and testable on any OS via `wiremock`.

## 8. CLI wiring

New subcommand tree under `rutracker mirror`, mounted in `crates/cli/src/main.rs`. Handlers delegate to `rutracker_mirror::*`. JSON default output (machine-friendly), `--format text` for status prints.

**v1 scope:** CLI-only. The `.mcp.json` is not touched; no new MCP tools. **v1.1 follow-up:** optional MCP tools `mirror_sync`, `mirror_watch_list`, `mirror_status` as thin dispatcher wrappers over the v1 CLI handlers. Explicitly out of scope for v1 (§15).

## 9. Implementation phases

Each phase ends with `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`.

### Phase M1 — Scaffold + `mirror init` + `mirror structure` + SQLite schema
- `crates/mirror` workspace member.
- `init`: create `$HOME/.rutracker/mirror/`, empty `structure.json`, empty `watchlist.json`, initialised `state.db` (migration 0001_init.sql applied, `schema_meta` seeded with `schema_version=1`).
- `structure`: calls `list_categories`, persists `structure.json` with `fetched_at`.
- `migrate.rs` skeleton: on open, read `schema_meta.schema_version`; if higher than binary's expected version, refuse open with actionable error "rutracker-mirror v<binary> is older than your state.db (v<db>); upgrade the binary". Forward-only, no down-migrations.

**Acceptance (architect R1 MAJOR #6 fix — migration ceremony specified):**
- `rutracker mirror init` creates `state.db` with `schema_meta.schema_version=1`.
- Named test `migrate::tests::test_newer_db_refused_with_actionable_error` passes (synthetic v2 db → binary refuses with a message containing the word "upgrade").
- Named test `migrate::tests::test_rebuild_index_from_json_reconstructs_state` — seed a JSON-only mirror, delete state.db, run `rebuild-index`, assert recovered rows match.
- Named test `state::tests::test_lazy_backfill_populates_missing_rows` — seed a mirror with a topic JSON that is absent from `topic_index`, call `backfill_missing_index_rows("252")`, assert the index now contains that row.
- `rutracker mirror structure` produces a `structure.json` with ≥ 26 groups.
- `rusqlite` promoted from target-specific dep of `cookies-macos` to a workspace dep in the root `Cargo.toml`; both `cookies-macos` and `mirror` reference it via `rusqlite.workspace = true`.

### Phase M2 — Watchlist commands
- `watch add <id>` looks up forum name from `structure.json`, writes to `watchlist.json`.
- `watch remove`, `watch list`.

**Acceptance:**
- Named test `watchlist::tests::test_add_then_list_returns_forum` — add 252, list returns 1 entry, id="252".
- Named test `watchlist::tests::test_remove_removes_entry` — add 252+251, remove 252, list returns only 251.
- Named test `watchlist::tests::test_add_duplicate_is_idempotent` — add 252 twice, list returns 1 entry (no duplicates).
- Named test `watchlist::tests::test_add_unknown_forum_id_errors` — add 999999 (not in structure.json) → error with actionable message.
- `rutracker mirror watch list --format json` stdout parses as valid JSON with a `forums[]` array.

### Phase M3 — `parse_forum_page` + row-parser dedup
- Extract `parser::row::parse_topic_row` (architect R1 MAJOR #5 fix).
- `parser::search::parse_search_page` delegates to it; existing 4 search tests stay green.
- New `parser::forum_page::parse_forum_page(html) -> ForumListing`.
- Commit `viewforum-sample.html` + `viewtopic-comments-page2.html` fixtures.

**Acceptance:**
- `parse_forum_page` on `viewforum-sample.html` returns ≥ 30 rows with non-zero `last_post_id` on each.
- Existing search tests still green (row-parser refactor is refactor-only, no behaviour change).

### Phase M4 — Sync engine: initial bulk fetch
- Implement `SyncEngine::sync_forum(forum_id, max_topics)`.
- For each topic in forum (up to `--max-topics`), fetch + write JSON atomically, upsert `topic_index` row.
- Commit the SQLite transaction only after the forum pass completes.
- Advisory `.lock` file in mirror root (PID recorded); second concurrent `sync` aborts with a clear error.
- **Test-only failure injection** (architect R1 phase-gating fix): env var `RUTRACKER_MIRROR_FAIL_AFTER_N_TOPICS`, gated behind `#[cfg(any(test, feature = "fail-injection"))]`, panics after the Nth topic write.

**Acceptance (all mechanical):**
- Named test `engine::tests::test_sync_forum_writes_expected_files_and_rows` — `sync_forum("252", max_topics=10)` with wiremock stubs for `viewforum.php` + 10 `viewtopic.php` responses; assert 10 JSON files exist AND `SELECT COUNT(*) FROM topic_index WHERE forum_id='252'` returns 10.
- Named test `engine::tests::test_topic_json_round_trips` — for each of the 10 files, `serde_json::from_str::<TopicFile>(…).is_ok()` and its `topic_id` matches the filename.
- Named test `engine::tests::test_transaction_boundary_commit_only_on_success` — inject a `FAIL_AFTER_N_TOPICS=3` panic, restart engine, assert `topic_index` contains 0 rows after the crash (transaction rolled back) and 10 rows after a full restart (forum-level commit semantic). Proves the "commit only after forum pass" guarantee from §4.2.
- Named test `engine::tests::test_concurrent_sync_sees_lock_and_aborts` — spawn two engines on the same root, second one aborts with `MirrorError::Locked` containing the PID of the first.
- Named test `engine::tests::test_429_marks_cooldown_and_aborts` — wiremock returns 429 → `forum_state.last_sync_outcome='rate_limited'`, `cooldown_until - now ∈ [59m, 61m]` (allowance for clock jitter), process exits 0 (graceful abort).

### Phase M5 — Delta detection + multi-page comment merge + resumability
- Stop-condition with 5-consecutive-older-and-known streak (§5.2 / architect R1 BLOCKING #1).
- Comment-merge from §5.3 with "commit only after all pages" semantics.
- Resumability via `RUTRACKER_MIRROR_FAIL_AFTER_N_TOPICS` injection.

**Acceptance (all mechanical, all wiremock-backed unless noted):**
- `test_idempotent_no_upstream_changes`: run sync twice with identical fixture. Second run's `files_written` counter = 0; total wall time < 1 s (asserted).
- `test_new_post_on_existing_topic_rewrites_only_that_topic`: fixture mutation → second sync writes exactly 1 file, all other topic file mtimes unchanged.
- `test_crash_mid_forum_resumes_cleanly`: set `RUTRACKER_MIRROR_FAIL_AFTER_N_TOPICS=3`, first run panics after 3 writes, second run completes to 10 topics total, `SELECT COUNT(DISTINCT topic_id)` = 10.
- `test_multi_page_comments_merged`: 2-page comment fixture → merged topic JSON has the union of posts, dedup by `post_id`.
- `test_stop_streak_tolerates_moved_topic`: fixture where row 0 is older-and-known, rows 1–4 are new → sync processes rows 1–4 and only then stops (NOT on row 0). Asserts 5 rows parsed (0 skipped, 4 fetched, 1 short-circuit trigger).

### Phase M6 — CLI finish + docs + rebuild-index
- `show`, `status`, `rebuild-index` subcommands.
- README section + `scripts/soak-mirror.sh` (live smoke, 2 forums × 5 topics each).
- CHANGELOG `1.1.0`.
- **v1 ships WITHOUT new MCP tools.** The `.mcp.json` stays unchanged. MCP parity (`mirror_sync`, `mirror_status`) is deferred to **v1.1** as a follow-up and explicitly out of scope here.

**Acceptance (all mechanical):**
- Named test `cli::tests::test_show_prints_topic_title` — seed a mirror with one topic, run `rutracker mirror show 252/6843582 --format json`, stdout JSON contains the expected `title`.
- Named test `cli::tests::test_status_reports_counts_and_cooldown` — seed forum with 5 rows + a synthetic `cooldown_until` in the future, run `rutracker mirror status --format json`, assert `forums[0].topics_count == 5` and `cooldown_seconds_remaining > 0`.
- Named test `engine::tests::test_rebuild_index_from_json_matches_file_count` — seed 10 JSON files across 2 forums, delete `state.db`, run rebuild, assert `SELECT COUNT(*) FROM topic_index == 10`.
- `scripts/soak-mirror.sh`: wrapper with fixed contract — pipes exit 0 iff initial sync downloads ≥ 3 topics per forum AND a second sync writes 0 topic files. Not a cargo test; runs as a manual pre-release gate (analogous to `scripts/soak.sh` in the main repo). Evidence: commit the `soak-mirror-<date>.log` with release.
- README section grep: `grep -q "Local mirror" README.md && grep -q "rutracker mirror sync" README.md`.
- CHANGELOG entry grep: `grep -q "1.1.0" CHANGELOG.md && grep -q "mirror" CHANGELOG.md`.

## 10. Backward compatibility

All new. Existing tools (`search`, `get_topic`, `browse_forum`, `list_categories`, `download_torrent`) unchanged. `.mcp.json` unchanged. New CLI subcommand tree under `mirror` doesn't collide with anything.

## 11. Pre-mortem (3 scenarios)

1. **RuTracker bans the account mid-sync.** Probability: medium. Impact: mirror stops; account may be flagged. Mitigation: default 1 rps single-threaded; on 429/503 set `forum_state.cooldown_until = now + 1h`, exit gracefully; next run skips the forum until cooldown elapses. 403 is treated as auth failure (triggers login-redirect abort path per §5.4, not a cooldown). Hard page cap 100/forum. No `Retry-After` honouring (would require extending the HTTP client; fixed cooldown is simpler and safer). **Owner:** M4. **Proof:** `engine::tests::test_429_marks_cooldown_and_aborts` (wiremock).
2. **Local schema evolution (our own JSON/SQLite format).** Probability: medium — we will add fields to topic JSONs or columns to state.db over time. Impact: newer binary reads older files (forward-compatible via additive fields); older binary on newer state.db must refuse rather than silently corrupt. Strategy: **forward-only, no auto-migrations**. `schema_meta.schema_version=1`; bump on breaking change; older binary refuses newer DB with `SchemaTooNew` + "upgrade the binary"; state.db is rebuildable from JSONs via `rebuild-index`. **Owner:** M1. **Proof:** `migrate::tests::test_newer_db_refused_with_actionable_error` + `migrate::tests::test_rebuild_index_from_json_reconstructs_state`.
3. **Parser drift (rutracker HTML changes under us).** Probability: high over months. Impact: `parse_forum_page` or `parse_topic_page` starts returning empty rows / wrong fields; the sync silently mirrors garbage. Mitigation: (a) every parser function asserts non-empty critical fields on fixture tests, so any selector regression is caught before release; (b) `scripts/soak-mirror.sh` release-gate runs against live HTML and fails loudly on empty rows or non-numeric `last_post_id`; (c) introduce a lightweight `ParseSanity` check in the sync engine: if a forum page parses to 0 rows *and* the raw HTML is non-empty, abort the forum pass with `ParseSanityFailed` instead of writing a broken mirror. **Owner:** M3 (parser fixtures) + M4 (sanity check) + M6 (soak). **Proof:** `parser::forum_page::tests::test_empty_listing_raises_sanity_error` + `engine::tests::test_parser_returning_zero_rows_aborts_forum`.

## 12. Expanded test plan

| Layer | Tooling | Default in `cargo test`? | Notes |
|---|---|---|---|
| Unit | `cargo test`, `pretty_assertions` | yes | Delta detection, merge-by-post_id, atomic write round-trip, schema-version rejection. |
| Integration | `wiremock` + synthetic fixtures | yes | Full sync run against stubbed rutracker. Initial, incremental, resume-after-crash scenarios. |
| E2E live | `--features live -- --ignored` | no (opt-in) | `rutracker mirror sync --forum 252 --max-topics 5` against real rutracker. Committed only as a checklist item in Phase M6. |
| Snapshot | Golden JSON fixtures | yes | `forums/252/topics/6843582.json` is a committed golden; test asserts the parser + mirror writer produce it byte-for-byte from the HTML fixture. |
| Observability | `tracing` (declared, workspace-wide) | n/a | `INFO` per forum entered; `DEBUG` per topic. `RUST_LOG=rutracker_mirror=debug` documented in README. |

## 13. Risks & Mitigations

| Risk | Mitigation | Owner | Proof |
|---|---|---|---|
| RuTracker ban / Cloudflare challenge | Default 1 rps; on 429/503 set `cooldown_until = now+1h`, abort gracefully; next runs skip until cooldown. 403 → auth-failure path (not cooldown) | M4 | `test_429_marks_cooldown_and_aborts` |
| Schema drift (forward-only) | `schema_meta.schema_version=1` enforced on `Mirror::open`; newer-db refusal; state.db is rebuildable from JSONs | M1 | `test_newer_db_refused_with_actionable_error`, `test_rebuild_index_from_json_reconstructs_state` |
| Comment pagination (topics > 30 posts) | Sync fetches all pages, commits merged result only if all pages succeeded | M5 | `test_multi_page_comments_merged` |
| Filesystem atomicity on APFS | temp-file + `File::sync_all` (data+metadata) + rename + parent-dir fsync. Documented caveat: network FS out of scope | M4 | `test_atomic_write_roundtrip`, manual durability note in README |
| Unbounded watchlist growth | Soft-cap warning at 100 forums; doc caveat about directory-scale limits | docs | README explicit note |
| Moved-topic quirk (topic_id not monotonic in listing) | 5-consecutive-older-and-known streak before stopping | M5 | `test_stop_streak_tolerates_moved_topic` |
| Concurrent runs | Advisory `.lock` with PID; second process aborts | M4 | `test_concurrent_sync_sees_lock_and_aborts` |
| Edit history loss | Documented as intentional in §5.3; no scope creep | docs | README note |
| Two storage layers drift (state.db ≠ JSONs) | `rebuild-index` command reconstructs from JSONs; sync auto-detects + backfills missing index rows | M1 + M6 | `test_rebuild_index_from_json_reconstructs_state` |
| `structure.json` (~100 KB) churn on refresh | Written once per `mirror structure` invocation (not on every sync); user opts in | docs | behavioural (one write per invocation) |
| `.rutracker/mirror/` committed to git by accident | README note; advise `.gitignore` entry for repo-embedded mirrors | docs | README |
| Network filesystem (NFS/SMB) unsupported | Documented as macOS-local; flock semantics differ, not tested | docs | README |

## 14. Definition of Done

### 14.1 Mechanical DoD (every row is a test or a programmatic check)

| Item | Proven by (phase / test name) |
|---|---|
| Workspace tests green | `cargo test --workspace` exits 0, stdout shows `0 failed` across all crates |
| Clippy clean | `cargo clippy --workspace --all-targets -- -D warnings` exits 0 |
| Format clean | `cargo fmt --all -- --check` exits 0 |
| Mirror init creates schema v1 | M1 `migrate::tests::test_init_seeds_schema_version_1` |
| Schema-version refusal | M1 `migrate::tests::test_newer_db_refused_with_actionable_error` |
| Rebuild-index full | M1 `migrate::tests::test_rebuild_index_from_json_reconstructs_state` |
| Lazy backfill | M1 `state::tests::test_lazy_backfill_populates_missing_rows` |
| `mirror structure` writes ≥ 26 groups | M1 `structure::tests::test_structure_json_contains_at_least_26_groups` |
| `rusqlite` promoted to workspace dep | M1 Shell: `grep -q '^rusqlite' Cargo.toml && grep -q 'rusqlite.workspace = true' crates/cookies-macos/Cargo.toml && grep -q 'rusqlite.workspace = true' crates/mirror/Cargo.toml` |
| Watchlist add+list | M2 `watchlist::tests::test_add_then_list_returns_forum` |
| Watchlist remove | M2 `watchlist::tests::test_remove_removes_entry` |
| Watchlist idempotent | M2 `watchlist::tests::test_add_duplicate_is_idempotent` |
| Watchlist unknown forum error | M2 `watchlist::tests::test_add_unknown_forum_id_errors` |
| `watch list --format json` emits valid JSON | M2 `cli::tests::test_watch_list_json_is_valid_array` (invokes the CLI handler, parses stdout, asserts `forums[]` is an array) |
| Row parser consolidated | M3 `parser::row::tests::test_parse_topic_row_matches_search_and_forum_fixtures` (runs `parse_topic_row` on both a search-page row and a forum-page row from committed fixtures; asserts field-by-field equality with the pre-refactor search-parser output) |
| Forum page parser | M3 `parser::forum_page::tests::test_rows_and_last_post_ids` |
| Parser sanity check | M3 `parser::forum_page::tests::test_empty_listing_raises_sanity_error` |
| Sync writes files+rows | M4 `engine::tests::test_sync_forum_writes_expected_files_and_rows` |
| Topic JSON round-trip | M4 `engine::tests::test_topic_json_round_trips` |
| Transaction boundary commit | M4 `engine::tests::test_transaction_boundary_commit_only_on_success` |
| Concurrent-run guard | M4 `engine::tests::test_concurrent_sync_sees_lock_and_aborts` |
| 429 cooldown | M4 `engine::tests::test_429_marks_cooldown_and_aborts` |
| Parser-zero-rows aborts | M4 `engine::tests::test_parser_returning_zero_rows_aborts_forum` |
| Idempotent no-op | M5 `engine::tests::test_idempotent_no_upstream_changes` |
| Targeted delta | M5 `engine::tests::test_new_post_on_existing_topic_rewrites_only_that_topic` |
| Resume-after-crash | M5 `engine::tests::test_crash_mid_forum_resumes_cleanly` |
| Multi-page comment merge | M5 `engine::tests::test_multi_page_comments_merged` |
| Stop-streak tolerance | M5 `engine::tests::test_stop_streak_tolerates_moved_topic` |
| `show` subcommand | M6 `cli::tests::test_show_prints_topic_title` |
| `status` subcommand | M6 `cli::tests::test_status_reports_counts_and_cooldown` |
| Rebuild index matches file count | M6 `engine::tests::test_rebuild_index_from_json_matches_file_count` |
| Mirror crate platform-neutral | Shell: `! grep rutracker-cookies-macos crates/mirror/Cargo.toml` exits 0 |
| `.mcp.json` unchanged | Shell: `git diff --exit-code .mcp.json` exits 0 |

### 14.2 Manual Release Checklist (not a cargo test)

| Item | How |
|---|---|
| Smoke run against live rutracker | Invoke `rutracker mirror init && … mirror structure && … watch add 252 && … watch add 251 && … sync --max-topics 5`, expect < 2 min wall clock. Log committed as `soak-mirror-<date>.log`. |
| Soak script passes | `bash scripts/soak-mirror.sh` exits 0 (two-pass: initial writes ≥ 6 topic files; second writes 0). |
| README has worked example | `grep -q "Local mirror" README.md && grep -q "rutracker mirror sync" README.md` |
| CHANGELOG 1.1.0 entry mentions mirror | `grep -q "^## \[1.1.0\]" CHANGELOG.md && grep -qi "mirror" CHANGELOG.md` |
| MCP parity deferred to v1.1 | Intentional non-goal; documented in §15. |

## 15. Out-of-scope (revisit later)

- **MCP parity (`mirror_sync`, `mirror_watch_list`, `mirror_status` tools) — deferred to v1.1.** v1 is CLI-only; `.mcp.json` is not touched. Per §8, v1.1 will add these as thin dispatcher wrappers over the v1 CLI handlers once the CLI shape is proven.
- Mirroring attachments (images inside posts).
- Mirroring `.torrent` files into the mirror tree (use existing `download_torrent` if needed).
- Full-text search over the mirror (FTS5 is a cheap add on top of state.db; not v1).
- Comment-edit history (`comment_revisions[]`) — intentional data-loss tradeoff, §5.3.
- Graphical UI / TUI.
- Linux / Windows cookie extraction (mirror crate itself is platform-neutral; only cookies are macOS-bound).
- Network-filesystem (NFS/SMB) support for the mirror root — APFS/ext4 only, `flock` semantics untested on NFS.
- Concurrent multi-forum sync (intentionally serial for politeness).

---

## 16. Shipped state (to be updated post-implementation)

_To be filled in at release time with actual file/line references, test counts, and any deviations from this plan._
