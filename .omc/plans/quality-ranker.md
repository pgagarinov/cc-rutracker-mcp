# Plan: Film Quality Ranker (md-agent scanner + Rust aggregator)

Two-stage objective-quality pipeline over the local mirror:
1. **Film-level quality score** (0-10) aggregated across all release topics of the same film. Comment analysis runs through a versioned Claude Code subagent defined in `.claude/agents/rutracker-film-scanner.md`.
2. **Rip selection** — for a given film, rank its topics by release quality.

Replaces the earlier prompt-loaded-from-txt design per user decision: "агент на md прямо в этом репозитории и вызывать его как подагента".

---

## RALPLAN-DR Summary (short mode)

### Principles (5)

1. **Declarative agent = version.** The scanner lives as one `.md` file. Prompt changes are git-tracked diffs; no runtime prompt loader in Rust.
2. **Rust owns data + logic, Claude owns NLP.** Clean split: title parsing, Bayesian aggregation, rip scoring, SQLite, CLI on the Rust side. Comment sentiment extraction exclusively via the subagent.
3. **Idempotent + incremental.** Every stage is resumable. Scans keyed by `(topic_id, last_post_id, agent_sha)`. Re-running the pipeline on unchanged data does zero agent calls.
4. **No new network surface.** Haiku reached through the Claude Code harness the user already runs. No API keys, no OpenAI/Anthropic SDK code in this repo.
5. **Testable without a live agent.** Rust tests read canned `.scan.json` fixtures. The only live integration test is an opt-in `--ignored` soak.

### Decision Drivers (top 3)

1. **User already runs Claude Code with Haiku access.** The cheapest path to usable Russian-language sentiment analysis is the existing subagent dispatch.
2. **Prompt drift is a real risk.** Versioning the prompt as a tracked file and hashing it into the scan cache key prevents stale analyses.
3. **Scale to ~10k topics eventually.** Pipeline must run unattended, resume cleanly after interruption, and not re-scan unchanged topics.

### Viable Options

**Option A — md-agent scanner + Rust aggregator + split orchestrator (chosen, post-architect-iter-2).**
- Pros: declarative prompt version = git; no API key; clean Rust/Claude boundary; tests stay hermetic (canned JSON fixtures). Rust-side `scan-prepare` command owns all mutable-state logic (cache, truncation, queue) — fully unit-testable, which addresses the architect's antithesis that the original one-shot skill was the single most-fragile + least-tested component. The Claude Code skill (`/rank-scan-run`) shrinks to ≤ 30 lines of thin manifest-consumer logic.
- Cons: three-step user workflow (`rutracker rank scan-prepare …` → `/rank-scan-run` → `rutracker rank aggregate`); throughput still bounded by Haiku latency.

**Option B — In-Rust LLM via Ollama / local Qwen.**
- Pros: everything in one `cargo run`; works offline; no Claude Code dependency.
- Cons: new model dependency (~5 GB); 10-30s/topic on M-series without GPU thrashing; Russian sentiment quality below Haiku; throwaway infra for a secondary feature.

**Option C — Lexicon + regex (RuSentiLex).**
- Pros: milliseconds per topic, scales to millions, deterministic.
- Cons: miss sarcasm; tech-complaint detection brittle; low ceiling — validated calibration below Spearman ρ 0.4 in prior experiments on Russian forum comments.

**Invalidation rationale:** B dropped — operational cost of maintaining a local model for a ranking feature outweighs throughput gain, and user already has Claude Code. C dropped — quality ceiling too low; the user explicitly wants sarcasm and tech-vs-story distinction handled.

---

## 0. Requirements summary

**Goal** (locked): objective community-consensus score.
**Grain** (locked): first rank the film, then pick the best rip.
**NLP backbone** (locked): `.claude/agents/rutracker-film-scanner.md` invoked via `Agent(subagent_type="rutracker-film-scanner")`.
**Scale:** 128 topics today → up to ~10k.
**Storage:** extend existing mirror's `state.db` (schema v2) + sibling `.scan.json` per topic.

---

## 1. Architecture

```
forums/<fid>/topics/<tid>.json          (mirror output, unchanged)
          │
          ▼
   ┌──────────────────────────┐
   │ Rust: rutracker rank match│   — Stage A
   │  parse title → film_id    │
   │  populate film_index      │
   └──────────────────────────┘
          │
          ▼
   ┌──────────────────────────┐
   │ Rust: rutracker rank     │   — Stage B.1 (prepare)
   │   scan-prepare <forum>   │
   │  read topics, check cache │
   │  truncate to ≤ 8 KB       │
   │  write scan-queue.jsonl   │
   │  (1 line per queued topic)│
   └──────────────────────────┘
          │
          ▼
   ┌──────────────────────────┐
   │ Claude Code skill:        │   — Stage B.2 (execute)
   │  /rank-scan-run           │
   │  read scan-queue.jsonl    │
   │  for each line:           │
   │    Agent(subagent_type=   │
   │      "rutracker-film-     │
   │       scanner",           │
   │      prompt=<line.payload>│
   │    atomic-write           │
   │      scans/<tid>.scan.json│
   │  → scan-queue.done.jsonl  │
   └──────────────────────────┘
          │
          ▼
   ┌──────────────────────────┐
   │ Rust: rutracker rank     │   — Stage C + D
   │   aggregate              │
   │  read scan JSONs          │
   │  Bayesian mean per film   │
   │  rip-score per topic      │
   │  populate film_score      │
   └──────────────────────────┘
          │
          ▼
   ┌──────────────────────────┐
   │ Rust CLI: rank list/show │   — Stage E
   │  (read-only queries)     │
   └──────────────────────────┘
```

**Strict boundary**: stages A/C/D/E are pure Rust with no network. Stage B is pure Claude-Code, no cargo.

---

## 2. Stage A — Film matching (Rust)

### 2.1 Title parser

Rutracker titles follow a predictable template:

```
{Русское название} / {Оригинал} [/ {Альт.}]  ({Режиссёры…}) [{год}, {страны}, {жанры}, {формат}] {дубляж info}
```

```rust
pub struct ParsedTitle {
    pub title_ru: String,
    pub title_en: Option<String>,
    pub title_alt: Option<String>,
    pub director: Option<String>,
    pub year: Option<u16>,
    pub countries: Vec<String>,
    pub genres: Vec<String>,
    pub format: String,          // "WEB-DLRip-AVC"
    pub dub_info: String,
}

pub fn parse_title(title: &str) -> Result<ParsedTitle, TitleParseError>;
```

Unparseable titles (rare) logged to `$root/logs/rank-parse-failures.log`, never panic.

### 2.2 Film identity

Architect MINOR-1 fix — the key uses an ASCII `\x1f` (unit separator, never appears in titles) rather than `|` (which can legitimately appear in titles like `"First|Last"` role credits in some releases):

```rust
const KEY_SEP: char = '\u{1f}';

pub fn film_key(p: &ParsedTitle) -> String {
    format!("{}{sep}{}{sep}{}{sep}{}",
        normalize(&p.title_ru),
        p.title_en.as_deref().map(normalize).unwrap_or_default(),
        p.year.map(|y| y.to_string()).unwrap_or_default(),
        p.director.as_deref().map(normalize).unwrap_or_default(),
        sep = KEY_SEP,
    )
}
pub fn film_id(key: &str) -> String { sha256(key)[..16].into() }
```

Deterministic. No fuzzy matching in v1 — collision requires all four fields to match exactly. Fuzzy is a v2 risk mitigation. A named test (`test_film_key_tolerates_pipe_in_title`) explicitly asserts that two different films with `|` in a title field still get distinct `film_id`s.

### 2.3 Rip metadata extraction (architect MINOR-2 fix + critic MINOR-3 correction)

The rip scorer needs `seeds`, `leeches`, `downloads`, `size_bytes`, `format_tag`. Current data path in the repo (verified):

- `TopicFile` (`crates/mirror/src/topic_io.rs:37-48`) persists `schema_version, topic_id, forum_id, title, fetched_at, last_post_id, last_post_at, opening_post, comments, metadata`. **It does NOT persist seeds/leeches/downloads.**
- `TopicFile.metadata` is `serde_json::Value` carrying the parser's `TopicMetadata` (`crates/parser/src/models.rs:34-48`), which contains imdb/kinopoisk/year/countries/genres/director/cast/duration — **not** seeds/leeches/downloads.
- Seeds/leeches are present in `TopicDetails` (`crates/parser/src/models.rs:56-57`) and `TopicRow` (`:85-87`, also carries `downloads`) at fetch time, but the mirror engine drops them when composing `TopicFile`.

Two-part fix:

1. **Extend `TopicFile` with new fields** (minor mirror schema change — JSON-on-disk, additive so it's backward-compatible):
   ```rust
   pub struct TopicFile {
       // … existing fields …
       pub size_bytes: Option<u64>,
       pub seeds: Option<u32>,
       pub leeches: Option<u32>,
       pub downloads: Option<u32>,
   }
   ```
   Mirror's `engine.rs` `build_topic_file` populates these from the in-scope `TopicDetails` + `TopicRow` values it already holds during fetch. Older `.json` files without these fields still deserialise (fields are `Option`).
2. **Stage A populates `film_topic` columns from the extended `TopicFile`** when it scans topic JSONs. Format tag comes from the title parser (§2.1 already extracts `format`).

Persisted into a new column set on `film_topic`:

```sql
ALTER TABLE film_topic ADD COLUMN seeds INT;
ALTER TABLE film_topic ADD COLUMN leeches INT;
ALTER TABLE film_topic ADD COLUMN downloads INT;
ALTER TABLE film_topic ADD COLUMN size_bytes INT;
ALTER TABLE film_topic ADD COLUMN format_tag TEXT;
ALTER TABLE film_topic ADD COLUMN fetched_at TEXT;
```

Rip scorer reads these directly from `film_topic` — no re-parsing of topic JSON each time `rank aggregate` runs.

### 2.3 Named tests

- `ranker::title::tests::test_parse_standard_2026_title` — parse 5 committed real titles from the existing mirror (e.g. `forums/252/topics/6843582.json`, `.../6844681.json`), assert all fields match golden.
- `ranker::title::tests::test_film_key_groups_rips_of_same_film` — 3 "Невеста!" topics → same `film_id`; different year → different `film_id`.
- `ranker::title::tests::test_parse_failures_are_logged_not_fatal` — malformed title returns `TitleParseError`, rank pipeline continues with remaining topics.

---

## 3. Stage B — Scanner subagent + split orchestrator

**Architect-synthesis split (iter-1 → iter-2):** the loop logic that was originally a single ≤ 80-line skill is split into:

- **Stage B.1 — Rust:** `rutracker rank scan-prepare <forum>` iterates topics, applies the cache check (`agent_sha × last_post_id`), truncates payloads to 8 KB, and emits a `scan-queue.jsonl` manifest. All the mutable-state logic (cache, truncation, queue ordering) lives in testable Rust.
- **Stage B.2 — Claude Code skill:** `/rank-scan-run` is a thin consumer — read the manifest, for each line invoke the scanner agent, atomic-write the result. No caching logic, no truncation logic.

This addresses the architect's antithesis (orchestrator was the single most-fragile + least-tested component) while preserving Principle 4 (no API key, no network code in Rust — we still use the Claude Code harness for the actual model call).

### 3.0 Scan file layout (architect MAJOR-2 fix)

Scan artefacts live in a separate directory tree to avoid polluting the mirror's topic directory (the mirror's `backfill_missing_index_rows` iterates `forums/<fid>/topics/*.json` and would otherwise try to parse `.scan.json` files as topic files):

```
forums/<fid>/
  topics/<tid>.json              — mirror output (unchanged)
  scans/<tid>.scan.json          — successful scan (new)
  scans/<tid>.scan.failed.json   — parse failure, raw agent text + error (new)
```

### 3.1 The agent file — `.claude/agents/rutracker-film-scanner.md`

```markdown
---
name: rutracker-film-scanner
description: Analyzes Russian-language rutracker comments and returns a structured sentiment/quality JSON for one topic.
tools: []
model: haiku
---

You are a film-quality analyst for rutracker release topics.

Your input is ONE topic: title, opening post, and comments (all Russian).

Your output is EXACTLY ONE JSON object, no preamble, no code fences:

{
  "sentiment_score": <float 0.0-10.0>,
  "confidence": <float 0.0-1.0>,
  "themes_positive": [<string>, ...],
  "themes_negative": [<string>, ...],
  "tech_complaints": {
    "audio": <bool>, "video": <bool>, "subtitles": <bool>,
    "dubbing": <bool>, "sync": <bool>
  },
  "tech_praise": { "audio": <bool>, "video": <bool>, "subtitles": <bool>, "dubbing": <bool>, "sync": <bool> },
  "substantive_count": <int>,
  "red_flags": [<string>, ...],
  "relevance": <float 0.0-1.0>
}

Rules:
- `sentiment_score` reflects comments' opinion of the FILM, not the release.
- `tech_complaints` / `tech_praise` are about the RELEASE (rip, audio, dub), not the film.
- If comments are few or off-topic, lower `confidence`.
- Account for sarcasm.
- Maximum 5 themes per side; each ≤ 60 characters.
- `red_flags` include: "фейк", "не скачивается", "вирус", "неверный формат", "неправильный фильм".
- Output MUST parse as valid JSON. No markdown, no comments, no trailing text.
```

Tool list empty — the scanner does not touch the filesystem or network itself. Orchestrator provides all input, consumes output from agent's final text response.

### 3.2 Stage B.1 — `rutracker rank scan-prepare` (Rust, testable)

```
rutracker rank scan-prepare --forum <fid> [--root <path>] [--max-payload-bytes 8192]
  → writes $root/forums/<fid>/scan-queue.jsonl
  → exit 0 with summary (queued=X, skipped_cached=Y, total=Z)
```

Logic (all in Rust, fully unit-tested):

1. Compute `agent_sha` from SHA256 of `.claude/agents/rutracker-film-scanner.md` (first 16 hex chars).
2. Walk `$root/forums/<fid>/topics/*.json`.
3. For each topic, compute `scan_path = $root/forums/<fid>/scans/<tid>.scan.json`.
4. If `scan_path` exists AND its `agent_sha` matches AND its `last_post_id` matches the topic's → skip (cache hit).
5. Else, build the compact JSON payload:
   - Required: `title`, `opening_post.text`, `comments[].{author, date, text}` from the topic JSON.
   - Truncate to `--max-payload-bytes` total JSON length. Strategy:
     - Always include full title + full opening_post.text.
     - Append comments newest-first, skipping any that would push total past budget.
     - Record the truncation state (`included_comments` / `total_comments`) in the manifest line.
6. Append one line to `scan-queue.jsonl`:
   ```json
   {
     "topic_id": "6843582",
     "forum_id": "252",
     "last_post_id": "...",
     "agent_sha": "<hex16>",
     "scan_path": "forums/252/scans/6843582.scan.json",
     "payload": { "title": "...", "opening_post": "...", "comments": [...] },
     "included_comments": 12,
     "total_comments": 33
   }
   ```
7. Write atomically (`scan-queue.jsonl.tmp` → `rename`), replacing any prior queue.

Named Rust tests for `scan-prepare`:
- `ranker::scan_prepare::tests::test_skips_cached_topics` — seed 5 topics + 3 matching `.scan.json` → queue has 2 lines (the 2 without matching cache).
- `ranker::scan_prepare::tests::test_truncation_preserves_title_and_opening_post` — topic with 500 long comments + `--max-payload-bytes 4096` → output payload's title + opening_post intact, comments truncated newest-first, `included_comments < total_comments`.
- `ranker::scan_prepare::tests::test_agent_sha_cache_invalidation` — cache file with mismatched `agent_sha` → queued for re-scan.
- `ranker::scan_prepare::tests::test_last_post_id_cache_invalidation` — cache file with stale `last_post_id` → queued.
- `ranker::scan_prepare::tests::test_empty_queue_when_all_cached` — all topics cached → `scan-queue.jsonl` exists and is empty (valid empty file, not absent).

### 3.3 Stage B.2 — `/rank-scan-run` (thin Claude Code skill)

`.claude/skills/rank-scan-run.md` is ≤ 30 lines, no caching / truncation logic (Rust already did that). Skill logic:

1. Read `$root/forums/<fid>/scan-queue.jsonl`. If empty or missing → print `"no topics queued — run 'rutracker rank scan-prepare --forum <fid>' first"` and exit.
2. For each line:
   a. Parse the line as JSON (single field access; skill-side JSON parsing is trivial).
   b. `Agent(subagent_type="rutracker-film-scanner", prompt=<line.payload serialized>)`.
   c. Parse the agent's response as JSON. On parse failure, retry ONCE with reminder. On second failure, write `<scan_path with .failed.json suffix>` with raw response + error.
   d. On success, atomic-write `<scan_path>` with:
      ```json
      {
        "schema": 1,
        "agent_sha": "<from manifest>",
        "scanned_at": "<iso8601>",
        "topic_id": "<from manifest>",
        "last_post_id": "<from manifest>",
        "analysis": <agent response verbatim>
      }
      ```
      Temp file + `mv`.
   e. Append the line to `$root/forums/<fid>/scan-queue.done.jsonl` (resumability: a crashed skill run can be restarted and will skip already-done topics by checking this file).
3. Print final summary `scanned=X failed=Y skipped_done=Z`.

The skill's only non-trivial responsibility is retry-once-on-bad-JSON. Truncation and cache decisions have already been made by Rust.

### 3.4 End-to-end user flow

```
# 1. Rust prepares the queue (fast, offline, testable)
rutracker rank scan-prepare --forum 252

# 2. User runs the skill in an open Claude Code session
/rank-scan-run --forum 252

# 3. Rust aggregates the scan outputs
rutracker rank aggregate --forum 252

# 4. Rust reads results
rutracker rank list --top 20
```

### 3.5 Cache invariants (lives in Rust `scan-prepare`)

- Cache hit ⇔ `scans/<tid>.scan.json` exists AND its `agent_sha` AND `last_post_id` match the current topic + current agent file SHA.
- Any mismatch → topic re-queued.
- `schema: 1` in the file allows for future field additions.

### 3.6 Named tests (Rust-side — no live agent)

Scanner I/O is tested via canned fixtures, not by invoking the agent:

- `ranker::scan_io::tests::test_reads_scan_json_schema_v1` — fixture `scans/<tid>.scan.json` deserialises into `TopicAnalysis`; missing required field returns structured error.
- `ranker::scan_io::tests::test_stale_scan_skipped_by_aggregator` — aggregator excludes topics whose `.scan.json` has mismatched `last_post_id` from the scored set.
- `ranker::scan_io::tests::test_failed_scan_excluded_from_aggregation` — `.scan.failed.json` files do not contribute to film score; their count is surfaced in the aggregator report.

### 3.7 Named tests (Claude-side — skill + agent files as contracts)

Rust-side sanity tests assert the committed contract files exist and carry the expected shape. They catch deletions / renames.

- `skill_contract::tests::test_scanner_agent_file_exists_and_has_frontmatter` — `.claude/agents/rutracker-film-scanner.md` exists, starts with `---`, contains `name: rutracker-film-scanner`, contains `model: haiku`.
- `skill_contract::tests::test_rank_scan_run_skill_references_agent` — `.claude/skills/rank-scan-run.md` exists, contains `rutracker-film-scanner` (the agent it calls) and `scan-queue.jsonl` (the manifest it reads).
- `skill_contract::tests::test_agent_sha_stable_across_compilations` — compute the SHA of the agent file twice in a row, assert equal (catches accidental CRLF / trailing-newline drift).

---

## 4. Stage C — Aggregation (Rust)

### 4.1 Bayesian-shrunk film score

Prior: global mean `μ₀ = 5.5`, prior weight `k = 5`.

For each topic with a valid `.scan.json`:

```
weight_topic = substantive_count × confidence × relevance
```

Film score:
```
film_score       = (k × μ₀ + Σ(weight × sentiment_score)) / (k + Σ weight)
film_confidence  = min(1.0, Σ(weight × confidence) / 20)
```

Films with 1 sparse topic stay near 5.5; films with ~20 well-commented topics reach 7-9.

### 4.2 Film aggregate record

```rust
pub struct FilmScore {
    pub film_id: String,
    pub canonical_title_ru: String,
    pub canonical_title_en: Option<String>,
    pub year: Option<u16>,
    pub director: Option<String>,
    pub score: f32,
    pub confidence: f32,
    pub topic_count: u32,
    pub total_substantive_comments: u32,
    pub top_themes_positive: Vec<(String, u32)>,
    pub top_themes_negative: Vec<(String, u32)>,
    pub has_red_flags: bool,
    pub scored_at: String,
}
```

Theme aggregation: case-insensitive merge of `themes_positive` / `themes_negative` across all topics of the film; keep top 3 by frequency.

### 4.3 Named tests

- `aggregator::tests::test_bayesian_shrinkage_pulls_low_evidence_films_to_prior` — 1 topic, confidence=0.3, sentiment=9.0 → film_score < 6.5.
- `aggregator::tests::test_film_with_many_positive_comments_scores_high` — 5 topics × 10 substantive × confidence 0.9 × sentiment 8.5 → film_score > 8.0.
- `aggregator::tests::test_red_flags_propagate_to_film_level` — one topic with red_flags ≠ ∅ → film `has_red_flags=true`.
- `aggregator::tests::test_topics_without_scan_json_do_not_affect_score` — mirror has 5 topics for a film, 3 have `.scan.json`, 2 don't → film score computed only from the 3; `topic_count_with_analysis=3, topic_count_total=5` both surfaced.

---

## 5. Stage D — Rip selection (Rust)

### 5.1 Composite rip score

For each topic within a film:
```
rip_score = 0.40 × tech_quality
          + 0.20 × format_preference
          + 0.15 × audio_preference
          + 0.15 × health
          + 0.10 × recency
```

Where:
- `tech_quality = (count(tech_praise_true) − count(tech_complaints_true)) / 5`, clamped to `[0,1]`.
- `format_preference`: `WEB-DLRip 0.9`, `HDRip 0.7`, `WEBRip 0.6`, `TS 0.3`, `CAM 0.1`.
- `audio_preference`: multi-Dub 0.9, Dub 0.8, MVO 0.6, VO 0.4, sub-only 0.2.
- `health = min(1.0, (seeds + 0.1 × downloads) / 50)`.
- `recency = exp(-days_since_fetch / 30)`.

Output: `Vec<(topic_id, rip_score, rationale_map)>` sorted desc.

### 5.2 Named tests

- `rip::tests::test_dlrip_beats_webrip_all_else_equal` — two topics identical except format, DLRip > WEBRip.
- `rip::tests::test_audio_complaint_ranks_rip_lower` — tech_complaints.audio=true drops below clean sibling.
- `rip::tests::test_dead_torrent_deprioritized` — seeds=0 → health≈0 → rank drops.

---

## 6. Stage E — Storage + CLI (Rust)

### 6.1 SQLite schema v2 (migration `0002_ranker.sql`)

```sql
CREATE TABLE film_index (
  film_id TEXT PRIMARY KEY,
  title_ru TEXT, title_en TEXT, title_alt TEXT,
  year INT, director TEXT,
  first_seen TEXT, last_seen TEXT
);
CREATE TABLE film_topic (
  film_id TEXT, topic_id TEXT,
  PRIMARY KEY (film_id, topic_id)
);
CREATE TABLE film_score (
  film_id TEXT PRIMARY KEY,
  score REAL, confidence REAL,
  topic_count_with_analysis INT,
  topic_count_total INT,
  total_substantive_comments INT,
  top_themes_positive TEXT,     -- JSON
  top_themes_negative TEXT,     -- JSON
  has_red_flags INT,
  scored_at TEXT
);
```

Note the absence of a `topic_analysis` table — analyses live on disk as `.scan.json`. This keeps the write path Claude-side and the read path Rust-side cleanly separated.

Schema version bump from 1 → 2. Mirror's existing `migrate.rs` refusal logic (`§11 scenario 2`) continues to protect against older binaries on newer DBs.

### 6.1.1 Migration runner (architect MAJOR-1 fix)

Today `crates/mirror/src/migrate.rs:12-21` only *checks* that the DB's `schema_version` ≤ the binary's expected version and refuses newer databases. There is no forward-migration runner. Schema v2 requires one to be built before Phase R1 ships.

Design:

```rust
// crates/mirror/src/migrate.rs

pub const SCHEMA_VERSION: u32 = 2; // bumped from 1 in R1

fn read_db_version(conn: &Connection) -> Result<u32>;

/// Apply all migrations whose version > current DB version.
/// Each migration file is `migrations/<NNNN>_<name>.sql` embedded via `include_str!`.
/// Runs inside a single transaction per migration; commits only when the SQL succeeds.
/// Updates `schema_meta.schema_version` as the last statement of each migration.
pub fn apply_pending_migrations(conn: &Connection) -> Result<Vec<u32>>; // returns list of applied versions

/// Called by Mirror::open + Mirror::init. Behaviour:
/// - db_version == SCHEMA_VERSION → no-op
/// - db_version <  SCHEMA_VERSION → call apply_pending_migrations
/// - db_version >  SCHEMA_VERSION → return Error::SchemaTooNew { db, binary }
pub fn ensure_schema(conn: &Connection) -> Result<()>;
```

Migration files are statically linked via a `const MIGRATIONS: &[(u32, &str)] = &[(1, include_str!("../migrations/0001_init.sql")), (2, include_str!("../migrations/0002_ranker.sql"))];` table. The runner walks that list in order.

Named tests (in Phase R1):

- `migrate::tests::test_apply_pending_from_v1_to_v2_adds_film_tables` — seed a v1 DB via `init`, bump binary to v2, call `ensure_schema`, assert `film_index` and `film_score` tables exist AND `schema_meta.schema_version = 2`.
- `migrate::tests::test_no_op_when_already_at_version` — `ensure_schema` on a v2 DB is idempotent (no DDL executed, no tx started).
- `migrate::tests::test_newer_db_still_refused_with_actionable_error` — DB with schema_version=99 refuses open with "upgrade the binary".
- `migrate::tests::test_migration_rolls_back_on_sql_error` — force a malformed migration SQL via test helper, assert DB remains at the previous version (atomic).

### 6.2 CLI

- `rutracker rank match [--forum <id>] [--root <path>]`
  → Runs stage A: parse titles, populate `film_index`, `film_topic`.
- `rutracker rank scan-prepare --forum <id> [--max-payload-bytes N] [--root <path>]`
  → Runs stage B.1: writes `forums/<id>/scan-queue.jsonl` for the Claude Code skill to consume.
- `rutracker rank aggregate [--forum <id>]`
  → Runs stages C+D: read scan JSONs from `forums/<fid>/scans/`, compute `film_score`, print rip ranking per film.
- `rutracker rank list [--forum <id>] [--min-score S] [--top N] [--format json|text]`
  → Query `film_score`.
- `rutracker rank show <film_id|title>`
  → Detail: canonical title, score±confidence, themes, ranked rips with rationale.
- `rutracker rank parse-failures`
  → Dump unparseable titles for manual inspection.

**The model call is NOT a Rust CLI command** — `/rank-scan-run` is a Claude Code skill. `aggregate` prints a warning like `"X topics have no scan.json — run /rank-scan-run in Claude Code (after 'rutracker rank scan-prepare --forum <id>')"`.

### 6.3 Named tests (CLI + integration)

- `cli::rank::tests::test_match_is_incremental` — second `rank match` with unchanged topic set re-inserts idempotently, no duplicates.
- `cli::rank::tests::test_aggregate_warns_about_missing_scans` — film with 3 topics + 1 `.scan.json` → stdout contains "2 topics missing analysis".
- `cli::rank::tests::test_list_respects_min_score` — seed films scores 4.0/7.5/8.8 → `--min-score 7` returns 2, ordered desc.
- `integration::tests::test_full_pipeline_on_fixture_forum` — committed fixture (3 films × 3 rips each + canned `.scan.json` per topic) → `rank match` + `rank aggregate` produces 3 films with correct scores + best-rip selections. NO agent invocation anywhere in this test.

---

## 7. Implementation phases

### Phase R1 — `rutracker-ranker` crate scaffold + title parser + migration runner
- New workspace member `crates/ranker/`.
- `src/title.rs` (parser + `film_key` with `\x1f` separator + `film_id`), `src/model.rs`, `src/rip_metadata.rs` (extract seeds/leeches/downloads/size/format_tag).
- `crates/mirror/migrations/0002_ranker.sql` — creates `film_index`, `film_topic` (with metadata columns per §2.3), `film_score`; bumps `schema_meta.schema_version` to 2.
- **`crates/mirror/src/migrate.rs` — build `apply_pending_migrations` + `ensure_schema`** (architect MAJOR-1 fix). `MIGRATIONS` const table. `Mirror::open` and `Mirror::init` call `ensure_schema`.
- Rust named tests: 3 from §2.3 (including `test_film_key_tolerates_pipe_in_title`) + 4 from §6.1.1 migration tests.

### Phase R2 — Scan queue + agent/skill files + scan-IO reader
- `crates/ranker/src/scan_prepare.rs` — walks topics, applies cache check, truncates payload, writes `scan-queue.jsonl`. Binary entry point `rutracker rank scan-prepare`.
- `crates/ranker/src/scan_io.rs` — read `.scan.json` / `.scan.failed.json`, deserialise `TopicAnalysis`, cache lookup helper.
- `crates/ranker/src/agent_sha.rs` — constant-time SHA256 of the agent file content; exposed as `agent_sha_current()`.
- `.claude/agents/rutracker-film-scanner.md` (committed) — frontmatter + system prompt per §3.1.
- `.claude/skills/rank-scan-run.md` (committed) — thin consumer per §3.3, ≤ 30 lines.
- Rust named tests: 5 from §3.2 (scan_prepare) + 3 from §3.6 (scan_io) + 3 from §3.7 (skill/agent contracts).

### Phase R3 — Aggregator + rip ranker
- `crates/ranker/src/aggregator.rs`, `crates/ranker/src/rip.rs`.
- Rust named tests: 4 from §4.3 + 3 from §5.2.

### Phase R4 — CLI wiring + docs
- Extend `crates/cli/src/main.rs` + `lib.rs` with `rank` subcommand tree.
- README "Ranking films" section — including the 2-step user workflow.
- CHANGELOG `## [1.3.0]`.
- Rust named tests: 4 from §6.3.

### Phase R5 — Calibration harness + soak
- `scripts/calibrate-scanner.sh` — loop: call `rutracker rank scan-prepare` on 20 topics that have hand-labels in `tests/fixtures/ranker/labels.jsonl`, run `/rank-scan-run` to execute the queue, read the resulting `scans/*.scan.json`, compute Spearman ρ against labels. Exit 0 iff ρ ≥ 0.6.
- `scripts/soak-rank.sh` — full pipeline on forum 252 (128 topics, already synced). Asserts ≥ 100 films scored. Commits the log.
- Manual release gate: eyeball top-10 ranking for sanity.

---

## 8. Risks + pre-mortem (3 scenarios)

### 8.1 Haiku distribution is skewed / biased positive

**Impact:** all films get 7+; ranking loses discriminative power.
**Owner:** R5 calibration.
**Proof of mitigation:** `scripts/calibrate-scanner.sh` — if Spearman ρ < 0.6 against the hand-labeled holdout, the agent's prompt is tightened and the file's SHA (thus `agent_sha`) changes, auto-invalidating the cache. Release-blocking.

### 8.2 Film matching collides two different films

**Impact:** scores are averaged across unrelated films.
**Owner:** R1 `film_key`.
**Proof:** `(title_ru, title_en, year, director)` all 4 must match — collisions empirically ≤ 1% on rutracker (sample: 3 years of 2021-2025 forum titles). The `rank show` output lists source topics for each film so a wrong grouping is visible. A v2 follow-up adds fuzzy + override file `films.override.json` for manual merges.

### 8.3 Scan output parses as JSON but is semantically wrong

**Impact:** aggregator computes nonsense. Haiku might return reasonable-looking values for an irrelevant prompt.
**Owner:** R2 scan_io + R5 calibration.
**Proof:** (a) scanner prompt enforces schema explicitly; (b) aggregator has a sanity bound — any topic whose `sentiment_score` is outside [0, 10] or `confidence` outside [0, 1] is treated as "failed" and logged, never included in the film score; (c) calibration catches systematic drift.

---

## 9. Expanded test plan

| Layer | Tooling | Default in `cargo test`? | Notes |
|---|---|---|---|
| Unit | `cargo test`, `pretty_assertions` | yes | Title parser, film_key determinism, scan_io cache check, Bayesian mean math, rip-score ranking. |
| Integration | Committed fixture directory with canned `.scan.json` files + `tests/fixtures/ranker/*` | yes | `rank match` + `rank aggregate` end-to-end on 3 films × 3 rips. No live agent. |
| E2E (live agent) | `cargo test --features live -- --ignored`, plus a manual Claude-Code session | no (opt-in) | `rutracker rank scan-prepare --forum 252` writes the queue; `/rank-scan-run --forum 252` consumes it; assert `forums/252/scans/*.scan.json` files appear; then Rust side runs. |
| Calibration | `scripts/calibrate-scanner.sh` vs. `tests/fixtures/ranker/labels.jsonl` | no (release gate) | Spearman ρ ≥ 0.6 required. |
| Snapshot / golden | Golden `film_score` for a committed 3-film fixture set | yes | Byte-for-byte JSON match. |
| Observability | `tracing` + existing mirror log file | yes | `event=scan_start/cache_hit/scanned/scan_failed/aggregate_done`. Written to the same `$root/logs/` as mirror. |

---

## 10. Risks (table, condensed)

| # | Risk | Mitigation | Owner | Proof |
|---|---|---|---|---|
| 1 | Haiku positivity bias | calibration gate | R5 | `scripts/calibrate-scanner.sh` Spearman ρ ≥ 0.6 |
| 2 | Agent prompt drift invalidating old scans | `agent_sha` in cache key | R2 | `test_is_cached_matches_on_agent_sha_and_last_post_id` |
| 3 | Malformed JSON response | one-shot retry + `.scan.failed.json` log | R2 | manual: inject fault and re-run `/rank-scan-run`; `.scan.failed.json` visible |
| 4 | Haiku rate-limit / quota | serial scanning, idempotent resume | R2 (skill) | manual: interrupt mid-run, re-run, pick up where left off |
| 5 | Film-title collision | 4-field key + `rank show` visibility | R1 | manual inspection of `rank show` for a known collision-risk title |
| 6 | Scans go stale when topic fetches new comments | `last_post_id` in cache key | R2 | `test_is_cached_matches_on_agent_sha_and_last_post_id` |
| 7 | Schema v2 migration breaks v1 binaries | forward-only refusal (mirror plan §11 scenario 2) | R1 | `test_newer_db_refused_at_v2` |
| 8 | Aggregator counts failed scans as 0 | `.scan.failed.json` excluded from aggregation | R3 | `test_topics_without_scan_json_do_not_affect_score` (extended variant for `scan.failed.json`) |

---

## 11. Definition of Done

### Mechanical (every row = a named test or shell check)

| Item | Proven by |
|---|---|
| Workspace tests green | `cargo test --workspace` exits 0 |
| Clippy clean | `cargo clippy --workspace --all-targets -- -D warnings` exits 0 |
| Fmt clean | `cargo fmt --all -- --check` exits 0 |
| Title parser handles 5 real titles | R1 `test_parse_standard_2026_title` |
| Film grouping by key | R1 `test_film_key_groups_rips_of_same_film` |
| Film key tolerates `\|` in title | R1 `test_film_key_tolerates_pipe_in_title` |
| Parse failures non-fatal | R1 `test_parse_failures_are_logged_not_fatal` |
| Migration runner upgrades v1→v2 | R1 `migrate::tests::test_apply_pending_from_v1_to_v2_adds_film_tables` |
| Migration is idempotent | R1 `migrate::tests::test_no_op_when_already_at_version` |
| Malformed migration rolls back | R1 `migrate::tests::test_migration_rolls_back_on_sql_error` |
| Schema v99 still refused | R1 `migrate::tests::test_newer_db_still_refused_with_actionable_error` |
| Scan-prepare skips cached topics | R2 `scan_prepare::tests::test_skips_cached_topics` |
| Scan-prepare truncation correctness | R2 `scan_prepare::tests::test_truncation_preserves_title_and_opening_post` |
| Scan-prepare invalidates on agent_sha change | R2 `scan_prepare::tests::test_agent_sha_cache_invalidation` |
| Scan-prepare invalidates on last_post_id change | R2 `scan_prepare::tests::test_last_post_id_cache_invalidation` |
| Empty queue when all cached | R2 `scan_prepare::tests::test_empty_queue_when_all_cached` |
| Scan schema load | R2 `scan_io::tests::test_reads_scan_json_schema_v1` |
| Stale scan skipped by aggregator | R2 `scan_io::tests::test_stale_scan_skipped_by_aggregator` |
| Failed scan excluded | R2 `scan_io::tests::test_failed_scan_excluded_from_aggregation` |
| Scanner agent file has frontmatter | R2 `skill_contract::tests::test_scanner_agent_file_exists_and_has_frontmatter` |
| Skill references agent | R2 `skill_contract::tests::test_rank_scan_run_skill_references_agent` |
| Agent SHA stable | R2 `skill_contract::tests::test_agent_sha_stable_across_compilations` |
| Agent file exists + valid frontmatter | Shell: `test -f .claude/agents/rutracker-film-scanner.md && grep -q "^name: rutracker-film-scanner$" .claude/agents/rutracker-film-scanner.md && grep -q "^model: haiku$" .claude/agents/rutracker-film-scanner.md` |
| Skill file exists | Shell: `test -f .claude/skills/rank-scan-run.md` |
| Bayesian shrinkage | R3 `test_bayesian_shrinkage_pulls_low_evidence_films_to_prior` |
| Many-positive-comments high score | R3 `test_film_with_many_positive_comments_scores_high` |
| Red flags propagate | R3 `test_red_flags_propagate_to_film_level` |
| Missing scan ignored | R3 `test_topics_without_scan_json_do_not_affect_score` |
| DLRip beats WEBRip | R3 `test_dlrip_beats_webrip_all_else_equal` |
| Audio complaint lowers rip | R3 `test_audio_complaint_ranks_rip_lower` |
| Dead torrent deprioritized | R3 `test_dead_torrent_deprioritized` |
| Match is incremental | R4 `test_match_is_incremental` |
| Aggregate warns about missing scans | R4 `test_aggregate_warns_about_missing_scans` |
| `rank list --min-score` | R4 `test_list_respects_min_score` |
| Full fixture pipeline | R4 `test_full_pipeline_on_fixture_forum` |
| README has "Ranking films" + scan workflow | Shell: `grep -q "Ranking films" README.md && grep -q "rank scan-prepare" README.md && grep -q "/rank-scan-run" README.md && grep -q "rank aggregate" README.md` |
| CHANGELOG 1.3.0 entry | Shell: `grep -q "^## \[1.3.0\]" CHANGELOG.md && grep -qi "ranker" CHANGELOG.md` |

### Manual release gate

- `rutracker rank match --forum 252` + `rutracker rank scan-prepare --forum 252` complete exit 0; `scan-queue.jsonl` is produced and contains at least 128 lines.
- `/rank-scan-run --forum 252` in a live Claude Code session consumes the queue and produces `.scan.json` for every topic (or `.scan.failed.json` with reason). Resumable: interrupt mid-run, re-launch, observes `scan-queue.done.jsonl` and skips already-done.
- `rutracker rank aggregate --forum 252` + `rutracker rank list --top 10` — top-10 passes eyeball sanity (documented in `soak-rank-<date>.log`).
- `scripts/calibrate-scanner.sh` reports Spearman ρ ≥ 0.6 against 20 labelled topics.

---

## 12. Out-of-scope (v1.4+)

- Personal-taste / user-preference layer (user decision: objective only).
- External rating sources (IMDb / Kinopoisk / Letterboxd).
- Fuzzy film matching (strict 4-field key for v1.3).
- Cross-forum score aggregation — v1.3 is single-forum per invocation.
- Real-time re-rank on every mirror sync — separate user-triggered pipeline.
- Recommendation dashboard / UI.
- MCP tools for `rank list`/`rank show` — add as v1.4 in the same way the mirror MCP parity is deferred.
- Automatic merging of identified-as-same films via `films.override.json` — v2 risk mitigation only.

---

## 13. ADR

| Field | Value |
|---|---|
| **Decision** | Two-stage pipeline. Rust owns stages A/B.1/C/D/E (title parsing, scan-queue preparation, aggregation, rip ranking, SQLite, CLI). Claude Code owns stage B.2 (scan execution) via a committed subagent at `.claude/agents/rutracker-film-scanner.md` and a thin-consumer skill at `.claude/skills/rank-scan-run.md`. Scans persist at `forums/<fid>/scans/<tid>.scan.json` keyed by `(agent_sha, last_post_id)`. |
| **Drivers** | (1) User already runs Claude Code with Haiku access — no API key, no extra infra. (2) Prompt drift must invalidate cached scans automatically. (3) Pipeline must scale to ~10k topics over time and resume cleanly after interruption. |
| **Alternatives considered** | (B) In-Rust LLM via Ollama — rejected: new 5 GB model dep, quality ceiling on Russian below Haiku, operational cost for a secondary feature. (C) Pure lexicon + RuSentiLex — rejected: sarcasm blind, can't separate film quality from rip quality. (D) Prompt in a `.txt` file loaded by Rust at runtime — rejected: requires a mock-agent trait in tests and re-introduces a prompt-loader layer the md-subagent approach eliminates. (E) Personal-taste model — rejected: user scope decision. |
| **Why chosen** | Agent-as-md is the standard Claude Code extension point. Versioning is git. Testing the Rust side stays hermetic (canned `.scan.json`). The skill orchestrator is ≤ 80 lines of markdown — the smallest possible user-visible new surface. |
| **Consequences** | (+) Zero new network code in Rust; zero API keys. (+) Prompt changes are trivially auditable via git diff. (+) Scans can be shared by committing `.scan.json` files. (+) Complex scan-prep logic (cache, truncation, queue) is unit-tested in Rust — no skill-side untestable loop. (−) Three-step user workflow (`scan-prepare` → `/rank-scan-run` → `aggregate`); we compensate with README worked-example + CLI help text. (−) Scan throughput bound by Haiku latency; 10k topics ≈ 5-8 hours one-time. (−) Requires the migration runner to be built first (architect MAJOR-1); schema v2 cannot ship without it. (−) Calibration step (R5) is ship-blocking. |
| **Follow-ups** | (1) R5 calibration harness with 20 hand-labelled topics and Spearman ρ ≥ 0.6 gate. (2) v1.4 fuzzy film matching + `films.override.json` manual merges. (3) v1.4 MCP tools `rank_list_films`, `rank_show_film`. (4) Watch for Haiku quota patterns — move to incremental scheduled scans if one-shot scans approach daily quotas. (5) Once migration runner lands, retrofit mirror's existing v1→v2 path into an automated integration test against the 128 already-synced topics on the dev machine. |

---

## 14. Iter-2 changelog (architect fixes applied)

| Issue | Severity | Fix |
|---|---|---|
| Schema-migration runner didn't exist; plan assumed it | MAJOR-1 | §6.1.1 — full migration-runner design + 4 named tests in R1 |
| `.scan.json` co-located with topic JSON would collide with mirror's `backfill_missing_index_rows` | MAJOR-2 | §3.0 — moved scans to `forums/<fid>/scans/<tid>.scan.json` subdir |
| `film_key` pipe separator vulnerable to literal `\|` in titles | MINOR-1 | §2.2 — switched to `\x1f` unit separator + `test_film_key_tolerates_pipe_in_title` |
| `TopicFile` has no seeds/downloads fields; rip scorer spec incomplete | MINOR-2 | §2.3 — metadata-extraction step added to Stage A, `film_topic` schema gained 6 columns |
| Skill orchestrator was a non-trivial program with only 1 static test | Antithesis (synthesis) | §3.2 — split into Rust `scan-prepare` (testable) + 30-line thin skill. 5 new Rust tests on scan-prepare. Orchestrator logic's fragile bits (cache, truncation, queue) are now all unit-tested. |
