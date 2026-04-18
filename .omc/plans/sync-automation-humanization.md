# Sync Automation + Human-Like Traffic Plan

Two features for `rutracker-mirror`:

1. **Sync-until-done**: a single `rutracker mirror sync` invocation runs until 100% complete (or a hard ceiling). It waits through cooldowns, reports progress, and writes a log.
2. **Humanized requests**: outgoing HTTP traffic looks like an interactive browser session, not a scripted crawler.

Both ship in v1.2 of the mirror feature. Builds on the v1.1 crate at `crates/mirror/`.

---

## 0. Goals

- Replace today's "run, cooldown, re-run" manual loop with a single foreground command that doesn't exit until the work is done.
- Make that command's progress visible in stdout and persisted to a log file.
- Reduce Cloudflare 520s / 429s by looking less like a bot.
- Keep everything under the existing politeness ceiling — we are not trying to go *faster*, we are trying to go *unattended* and *sustainable*.

---

## 1. Feature 1 — Sync-until-done

### 1.1 CLI contract

```
rutracker mirror sync --forum 252 --max-topics 10000 --force-full
```

After this returns exit 0, ONE of:
- all watch-listed (or `--forum`-listed) forums reached `last_sync_outcome='ok'` with no uncovered topics up to `--max-topics`, OR
- the hard per-forum attempt ceiling was hit and the command printed a clear "gave up on forum X after N rate-limits".

Exit codes:
- `0`: all target forums completed.
- `1`: at least one forum hit the attempt ceiling (still wrote what it got, but not 100%).
- `2`: unrecoverable error (parser broken, filesystem, schema).

New flags (additive to §5.1 of `mirror-sync.md`):

| Flag | Default | Meaning |
|---|---|---|
| `--max-attempts-per-forum` | `24` | Hard ceiling on sync retries per forum (with 1h cooldowns = ~1 day wall time). Prevents runaway loops when rutracker is down for extended periods. |
| `--cooldown-wait` | `true` | When `false`, exits on first rate-limit (current behaviour — kept for CI). |
| `--log-file` | `$root/logs/sync-<YYYYMMDD>-<HHMMSS>.log` | Structured log file (ndjson). `--log-file -` writes to stderr, `--log-file ""` disables file logging. |

### 1.2 Behaviour

Per target forum:

```
attempts = 0
while attempts < max_attempts_per_forum:
  attempts += 1
  report = engine.sync_forum(forum_id, opts)
  if report.forums_rate_limited == 0:
    # Normal completion — listing ran out or streak triggered.
    break
  # Rate-limited.
  cooldown = read forum_state.cooldown_until
  if cooldown in past or missing:
    continue  # try again immediately
  wait_seconds = cooldown - now
  log("cooldown until {cooldown}, sleeping {wait_seconds}s (attempt {attempts}/{max})")
  sleep(wait_seconds)
```

Once the inner `while` exits:
- if completed normally → log `forum {id}: ok, {topics_count} topics`, advance to next forum.
- if ceiling hit → log `forum {id}: GAVE UP after {max} attempts, partial mirror`, record in summary, continue with next forum.

Across forums: iterate serially (no concurrency — §5.4 politeness principle). Final stdout summary lists per-forum outcome.

### 1.3 Progress reporting (stdout)

Human-readable live stream, **not** the current terminal-dump-JSON-at-end pattern:

```
2026-04-18T21:30:01Z INFO  sync start: forum=252 (Фильмы 2026) attempt=1/24
2026-04-18T21:30:02Z INFO  page 1: 50 rows parsed, 3 new, 47 known
2026-04-18T21:30:03Z INFO  fetch topic 6843582 "Проект «Конец света»..." (1/3 new on page)
2026-04-18T21:30:07Z INFO  fetch topic 6843390 "Грозовой перевал..." (2/3 new on page)
...
2026-04-18T21:32:11Z WARN  cloudflare 520 on viewforum.php start=50, retrying in 30s
2026-04-18T21:32:41Z INFO  page 2: 50 rows parsed, 44 new, 6 known
...
2026-04-18T21:45:00Z WARN  rate-limited at row 94, cooldown until 22:45:00Z (sleeping 60m)
2026-04-18T22:45:02Z INFO  resuming forum=252 attempt=2/24
...
2026-04-18T22:50:20Z INFO  forum=252 complete: 128 topics, 3 pages walked
2026-04-18T22:50:20Z INFO  ALL DONE: 1 forum synced, 0 failed
```

Implementation: `tracing` + `tracing_subscriber::fmt` layer with a compact format. Already a declared workspace dep (§12 of mirror plan).

### 1.4 Log file

ndjson (one JSON event per line) at `$root/logs/sync-<YYYYMMDD>-<HHMMSS>.log`. Events:

```json
{"ts":"...","level":"info","event":"forum_start","forum_id":"252","attempt":1,"max_attempts":24}
{"ts":"...","level":"info","event":"page_parsed","forum_id":"252","page":1,"rows":50,"new":3,"known":47}
{"ts":"...","level":"info","event":"topic_fetched","forum_id":"252","topic_id":"6843582","title":"..."}
{"ts":"...","level":"warn","event":"cloudflare_retry","url":"viewforum.php?f=252&start=50","status":520,"retry_in_ms":30000}
{"ts":"...","level":"warn","event":"rate_limit_sleep","forum_id":"252","cooldown_until":"...","sleep_seconds":3600}
{"ts":"...","level":"info","event":"forum_complete","forum_id":"252","topics_count":128,"pages_walked":3}
{"ts":"...","level":"info","event":"sync_complete","forums_ok":1,"forums_failed":0}
```

Implementation: a `tracing_subscriber::fmt::layer().json()` filtered to only the `rutracker_mirror::sync` target.

### 1.5 Acceptance criteria

- **AC1** `cargo test -p rutracker-mirror auto_resume` passes (see named tests below).
- **AC2** Live run on forum 252 completes unattended to 100% (the plan's top-level goal). Evidence: `soak-mirror-unattended.log` showing all 128 topics fetched across ≥2 cooldown cycles.
- **AC3** `--log-file ""` disables file logging (for CI stability).
- **AC4** `--cooldown-wait=false` preserves legacy early-exit behaviour (backwards-compatible CI path).
- **AC5** stdout stays line-delimited (no interleaved progress spinners) — grep-able.
- **AC6** ceiling works: `--max-attempts-per-forum=2` combined with wiremock permanent-429 exits with code 1 after 2 tries, not infinity.

### 1.6 Named tests (TDD red→green)

In `crates/mirror/tests/auto_resume.rs` (new integration-test file):

1. `test_auto_resume_waits_through_cooldown_and_completes`
   - wiremock forum listing returns 429 on request N, 200 on request N+1.
   - `SyncDriver::run_until_done(forum_id, opts{max_attempts:3})` returns Ok; total elapsed > cooldown_until delta; final `forums_rate_limited == 0`, `last_sync_outcome == 'ok'`.
   - For fast testing: override `--cooldown-multiplier 0.001` (new test-only SyncOpts field) so 1h cooldown becomes 3.6s.

2. `test_auto_resume_hits_ceiling_on_persistent_429`
   - wiremock always returns 429.
   - `SyncDriver::run_until_done(forum_id, opts{max_attempts:2})` returns `Err(GaveUp)` after exactly 2 attempts. stdout contains "GAVE UP after 2 attempts".

3. `test_auto_resume_emits_ndjson_log_file`
   - Run a short sync; assert log file exists, is ndjson, first event is `forum_start`, last event is `sync_complete`.

4. `test_multi_forum_continues_after_one_fails`
   - watchlist has forums 251, 252. 251 → wiremock always-429 (ceiling hit). 252 → normal completion.
   - `SyncDriver::run_until_done_all()` returns Ok with 1 forum_ok + 1 forum_failed; exit code 1 at CLI layer.

5. `test_progress_stream_line_delimited`
   - capture stdout during a mock sync; assert every line parses as `<rfc3339> <LEVEL> <msg>` via regex.

### 1.7 Implementation steps

1. **`crates/mirror/src/driver.rs` (new module)** — `SyncDriver` type:
   - `new(mirror, client) -> SyncDriver`
   - `async fn run_until_done(&mut self, forum_id, opts) -> Result<ForumSummary>`
   - `async fn run_until_done_all(&mut self, forum_ids: &[String], opts) -> Result<SyncSummary>`
   - Owns the attempt loop, cooldown-wait, ceiling logic.
2. **`SyncOpts`** — add `max_attempts_per_forum: u32 = 24`, `cooldown_wait: bool = true`, `cooldown_multiplier: f32 = 1.0` (test knob). Keep existing fields.
3. **`crates/cli/src/lib.rs` `run_mirror_sync`** — build driver instead of calling `SyncEngine::sync_forum` directly. Exit code mapping.
4. **`crates/cli/src/main.rs`** — add `--max-attempts-per-forum`, `--cooldown-wait`, `--log-file` flags. Wire `tracing_subscriber` init based on `--log-file`.
5. **`crates/mirror/Cargo.toml`** — add `tracing-subscriber` with `json` + `fmt` features (if not already there).
6. **Tests first (TDD)**: red tests 1-5 → implement driver → green.

### 1.8 Risks

- **Long sleep in test suite.** Mitigation: `cooldown_multiplier` test-only field; never ship >1.0.
- **Ceiling too low for outages.** Mitigation: 24 attempts × 1h = 1 day; user can raise via flag. Logs tell you if you hit it.
- **tracing subscriber interferes with already-running tests.** Mitigation: init subscriber only in CLI main, not in library; tests can enable manually.
- **Log file bloats unbounded on large watchlists.** Mitigation: one file per invocation (timestamped). User rotates. Could add size-cap later.

---

## 2. Feature 2 — Humanized requests

### 2.1 Why

At 1 rps we still see Cloudflare 520s + occasional 429. Some of that is pure origin flakiness (§11 scenario 1), but some is Cloudflare's bot-heuristic layer flagging regular-interval client behaviour. Making traffic look human costs us nothing in correctness and likely reduces rate-limit events.

### 2.2 Scope (v1.2)

Implement in order of expected payoff-per-line-of-code:

| # | Tactic | Payoff | Cost |
|---|---|---|---|
| 1 | **Jittered rate** (uniform U[0.5s, 2.5s]) instead of fixed 1 rps | Breaks uniform-interval fingerprint | 10 lines |
| 2 | **Referer header** (forum listing → topic click-through) | Matches real navigation graph | 15 lines |
| 3 | **Realistic Accept-Language + Accept-Encoding headers** | Russian user fingerprint | 5 lines |
| 4 | **Occasional long pause** (every 15-25 topics, sleep 30-60s) | Mimics "user reading" | 20 lines |
| 5 | **User-Agent rotation** (one real UA per process from a pool of 4) | Varies across sessions | 15 lines |

Out of scope for v1.2 (revisit if 520 rate doesn't drop): fetch-order shuffling, mouse-simulator heuristics, canvas fingerprinting.

### 2.3 Rate jitter model

Today: sleep `1.0 / rate_rps` seconds between requests (fixed 1000ms at rate=1).

New: sleep `U[min_delay_ms, max_delay_ms]` where defaults are `min=500, max=2500` giving expected 1.5s / ~0.67 rps — *polite*, not slower on average than 1 rps (actually ~50% slower — a deliberate trade-off for sustainability).

Implementation: `SyncOpts { min_delay_ms: u64 = 500, max_delay_ms: u64 = 2500 }` replaces `rate_rps`. Legacy `--rate-rps` flag kept and translated: `rate_rps=R → min=max=1000/R` (deterministic mode).

### 2.4 Referer header

When fetching `viewtopic.php?t=<id>`, send `Referer: <base>/forum/viewforum.php?f=<forum_id>`.

When fetching comment pages 2+ of same topic, send `Referer: <base>/forum/viewtopic.php?t=<id>` (self-referral).

When fetching forum listing pages, send `Referer: <base>/forum/index.php` (from the category tree).

Implementation: `Client::get_text` gains an optional `referer: Option<&str>` argument, OR a new `get_text_with_referer` method. The engine passes the appropriate referer per call site.

### 2.5 Accept-Language + Accept-Encoding

Add to every request:

```
Accept-Language: ru-RU,ru;q=0.9,en-US;q=0.8,en;q=0.7
Accept-Encoding: gzip, deflate, br
Accept: text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8
```

Implementation: configure in `rutracker_http::Client::new()` default header map.

### 2.6 Reading pauses

After every `pause_every_n` (default 20, jitter U[15,25]) successful topic fetches, sleep `U[30s, 60s]` before the next fetch. Log as `event: reading_pause`.

Implementation: counter in `SyncDriver`, checked after each topic write.

### 2.7 User-Agent rotation

Pick ONE UA at `Client::new()` time from an array of 4 realistic recent Chrome/Firefox UAs (updated 2026-04-18). Use the same UA for the entire process lifetime (a human wouldn't swap browsers mid-session).

Pool example (kept in `crates/http/src/user_agents.rs`):

```rust
pub const POOL: &[&str] = &[
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/133.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 14.4; rv:124.0) Gecko/20100101 Firefox/124.0",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/133.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:124.0) Gecko/20100101 Firefox/124.0",
];
```

Selection: seeded by current UNIX hour (rotates naturally across days but stable within a run).

### 2.8 Acceptance criteria

- **AC7** stdout of a dry-run (or test) shows 5 consecutive request intervals; none equal to each other (jitter proven).
- **AC8** wiremock-captured Referer header on viewtopic.php request = `forum/viewforum.php?f=<forum_id>`.
- **AC9** wiremock-captured Accept-Language = `ru-RU,...`
- **AC10** reading-pause event emitted after every 15-25 topics (depending on jitter).
- **AC11** UA for a full test run is one of the 4 pool entries, and identical across all requests in that run.

### 2.9 Named tests

In `crates/mirror/tests/humanize.rs` + `crates/http/tests/headers.rs`:

1. `test_jittered_delay_is_nonuniform` — run 10 sequential fetches in a controlled test (with wiremock returning 200 immediately), record the sleep durations, assert not all equal and all ∈ [min, max].
2. `test_referer_set_on_topic_fetch` — wiremock asserts request carries `Referer: <host>/forum/viewforum.php?f=252`.
3. `test_accept_language_header` — every request carries `Accept-Language: ru-RU...`.
4. `test_reading_pause_every_n_topics` — `pause_every_n = 3`; drive 10 topics; assert 3 reading-pause events logged (after topic 3, 6, 9).
5. `test_user_agent_is_from_pool_and_stable` — one `Client::new()` → 5 requests → all 5 carry the same UA, and that UA is in `POOL`.

### 2.10 Implementation steps

1. **`crates/http/src/user_agents.rs` (new)** — 4-entry `POOL`, `pick_by_hour() -> &'static str`.
2. **`crates/http/src/lib.rs` `Client::new()`** — set default UA + Accept-Language + Accept-Encoding + Accept headers.
3. **`crates/http/src/lib.rs` `Client::get_text`** — add optional `referer: Option<&str>` (or parallel method).
4. **`crates/mirror/src/engine.rs`** — replace `tokio::time::sleep(Duration::from_secs_f32(1.0 / opts.rate_rps))` with `sleep_jittered(&opts)` helper; threaded through listing + topic + comment-page call sites.
5. **`SyncOpts`** — add `min_delay_ms`, `max_delay_ms`, `pause_every_n` (default 20, jitter U[15,25]), `pause_min_secs` (30), `pause_max_secs` (60). Deprecate `rate_rps` — keep as legacy knob that sets both min & max.
6. **`SyncDriver`** — thread the topic-write counter for reading-pause triggering (pauses happen between topics, not page fetches).

### 2.11 Risks

- **Jitter breaks determinism in tests.** Mitigation: every test that asserts timing uses a seeded `rand::rngs::StdRng`, and `SyncOpts` accepts a `rng_seed: Option<u64>` test knob.
- **Referer leak.** We already are the only party seeing our own Referer (it goes to rutracker who we explicitly want to convince). No exfiltration risk.
- **UA pool goes stale** (Chrome 133 won't look plausible in 2027). Mitigation: include in README a 6-month reminder; it's a 4-line constant update.
- **Reading-pause accidentally triggers during crawl completion.** Harmless — logs an extra sleep that affects nothing.

---

## 3. Definition of Done

Mechanical (runnable):

| Check | Proven by |
|---|---|
| Workspace builds + tests green | `cargo test --workspace` exits 0 |
| Clippy clean | `cargo clippy --workspace --all-targets -- -D warnings` |
| Fmt clean | `cargo fmt --all -- --check` |
| Auto-resume ceiling | M-AR1 `test_auto_resume_hits_ceiling_on_persistent_429` |
| Auto-resume cooldown-wait | M-AR2 `test_auto_resume_waits_through_cooldown_and_completes` |
| Log file ndjson | M-AR3 `test_auto_resume_emits_ndjson_log_file` |
| Multi-forum continues | M-AR4 `test_multi_forum_continues_after_one_fails` |
| Stdout line-delimited | M-AR5 `test_progress_stream_line_delimited` |
| Jitter nonuniform | M-H1 `test_jittered_delay_is_nonuniform` |
| Referer header | M-H2 `test_referer_set_on_topic_fetch` |
| Accept-Language | M-H3 `test_accept_language_header` |
| Reading pause | M-H4 `test_reading_pause_every_n_topics` |
| UA from pool | M-H5 `test_user_agent_is_from_pool_and_stable` |
| `--max-attempts-per-forum` wired | shell: `rutracker mirror sync --help | grep -q max-attempts-per-forum` |
| `--log-file` wired | shell: `rutracker mirror sync --help | grep -q log-file` |

Manual release gate:

- Live unattended run on forum 252: from 93 topics → 128 topics (the current remainder), exit 0, no operator touch. Save `soak-mirror-unattended-<date>.log` as evidence.
- README "Local mirror" section: add a subsection on the automation flags and a caveat about the jitter-induced 0.67 rps effective rate.
- CHANGELOG `## [1.2.0]`.

---

## 4. Out of scope (v1.3+)

- Concurrent multi-forum sync (serial-for-politeness preserved from §15 of mirror plan).
- Proxy / IP rotation.
- MCP tools (`mirror_sync`, `mirror_status`, `mirror_watch_list`) — still deferred.
- Canvas fingerprint / JS execution.
- Bloom-filter-based "seen before" acceleration (current HashMap is fine at <10k rows).

---

## 5. ADR (short)

| Field | Value |
|---|---|
| **Decision** | Add `SyncDriver` wrapping `SyncEngine` to handle cooldown-waits + retries until 100% complete; add jitter, Referer, reading-pauses, UA rotation to the `rutracker_http::Client`. |
| **Drivers** | (1) user wants a fire-and-forget `sync` command. (2) reduce Cloudflare 520/429 events. (3) preserve politeness principle from mirror-sync.md §5.4. |
| **Alternatives considered** | (B) Move auto-resume into the engine itself — rejected, keeps engine deterministic/unit-testable. (C) Shell-script wrapper around the CLI — rejected, no real-time progress UX. (D) Proxy rotation — rejected, out-of-scope complexity, v1.3+. |
| **Why chosen** | Driver layer preserves engine simplicity. Humanization via HTTP client layer means all future callers benefit. |
| **Consequences** | Effective request rate drops ~50% (1 rps → ~0.67 rps average) — acceptable trade for sustainability. `rate_rps` flag semantics change (becomes legacy, fixed-delay mode). |
| **Follow-ups** | (1) Monitor 520 rate post-ship to validate humanization assumption. (2) If still high, consider proxy rotation in v1.3. (3) Refresh UA pool every 6 months. |

---

## 6. Estimated effort

- Feature 1: ~300 lines of code + 5 tests. 0.5-1 working day.
- Feature 2: ~200 lines of code + 5 tests. 0.5 working day.
- Total: ~1-1.5 day of focused work. TDD red→green for each named test.
