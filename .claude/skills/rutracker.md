---
name: rutracker
description: Используй когда пользователь спрашивает про поиск/скачивание торрентов с rutracker.org, локальное зеркало форума, ранжирование фильмов по комментариям, или про CLI-команды `rutracker`/`rutracker-mcp` из этого репозитория. Скилл покрывает поиск, браузинг, скачивание `.torrent`, mirror sync с auto-resume, quality ranker на Haiku, resolve имён разделов в числовые id.
---

# Гид по rutracker-mcp

Workspace Rust-проекта с двумя бинарниками и сабагентом-анализатором комментариев.

- **`rutracker`** — CLI для интерактивного использования.
- **`rutracker-mcp`** — MCP-сервер над тем же ядром.
- **`.claude/agents/rutracker-film-scanner.md`** — Haiku-сабагент для сантимента русскоязычных комментариев.

Прогрессивное раскрытие: простые задачи наверху, продвинутые внизу.

---

## Уровень 1 — базовые запросы

### Поиск торрента
```bash
rutracker search "project hail mary"
rutracker search "interstellar" --category 252 --sort-by seeders
```
Возвращает JSON по умолчанию; `--format text` — человеко-читаемо. `--sort-by ∈ {seeders,size,downloads,registered}`, `--order ∈ {desc,asc}`, `--page N`.

### Детали топика (с комментами)
```bash
rutracker topic 6843582 --comments
```

### Скачать `.torrent`
```bash
rutracker download 6843582 --out-dir ~/Downloads
```
Требует куку `bb_dl_key` (подтягивается автоматически из Brave Keychain при первом запуске).

### Список разделов форума
```bash
rutracker categories --format text | grep -i фильм
```
Покажет иерархию `[c-N]` групп и `[id]` форумов.

---

## Уровень 2 — разделы по имени

Везде, где аргумент `<forum>` или `--forum`, можно писать либо id, либо имя (точное или уникальная подстрока, case-insensitive):

```bash
rutracker mirror watch add "Фильмы 2026"      # → резолвится в 252
rutracker mirror sync --forum "Фильмы 2026"
rutracker rank list --forum "Фильмы 2026" --top 20
```

**Правила резолва:**
- Чисто-цифровой ввод (`252`) → проходит без проверки (обратно совместимо для скриптов).
- Exact match по имени → выбирается он.
- Неоднозначная подстрока (`"Фильмы"` матчит 2016-2020 / 2021-2025 / 2026) → ошибка со списком кандидатов.
- Многословные имена требуют кавычек в шелле.

Исходник маппинга: `$HOME/.rutracker/mirror/structure.json` (обновляется командой `rutracker mirror structure`).

---

## Уровень 3 — локальное зеркало (incremental mirror)

Принцип: скачиваем только те разделы, которые указаны в watchlist. Каждый топик = один JSON на диске. Индекс дельт — SQLite в режиме WAL.

### Разовая инициализация

```bash
rutracker mirror init                              # создаёт $HOME/.rutracker/mirror/
rutracker mirror structure                         # подкачивает structure.json с rutracker
rutracker mirror watch add "Фильмы 2026"
```

### Синхронизация (auto-resume)

```bash
rutracker mirror sync --forum "Фильмы 2026" --max-topics 10000 --force-full
```
- `--force-full` — игнорирует stop-streak (обязателен для первичного bulk-fetch после прерывания).
- Cooldown на 429/503/520-526 (Cloudflare transient) — 1h. Один **inline-retry** на 520-526 перед выходом в cooldown.
- `--max-attempts-per-forum N` (default 24) — ceiling попыток на один форум.
- `--cooldown-wait=true/false` — ждать cooldown внутри запуска (default true) или падать сразу (старое поведение для CI).
- `--log-file <path>` — куда писать ndjson-лог. `-` = stderr, `""` = отключить файл, default = `$root/logs/sync-<ts>.log`.

События логов: `forum_start`, `page_parsed`, `topic_fetched`, `cloudflare_retry`, `rate_limit_sleep`, `reading_pause`, `forum_complete`, `sync_complete`.

### Наблюдение

```bash
rutracker mirror status                            # per-forum counts, последний outcome, активные cooldown
rutracker mirror show 252/6843582 --format text    # показать кэшированный топик
rutracker mirror rebuild-index                     # восстановить state.db из JSON-ов (если SQLite потерян)
```

### Расположение данных

```
$HOME/.rutracker/mirror/                           # корень (override: --root или $RUTRACKER_MIRROR_ROOT)
  state.db                                         # SQLite/WAL, индекс дельт
  structure.json                                   # дерево разделов
  watchlist.json                                   # отслеживаемые форумы
  logs/
    sync-<YYYYMMDD>-<HHMMSS>.log                   # ndjson-лог sync, по файлу на запуск
    rank-parse-failures.log                        # невыпарсенные title'ы (Уровень 4)
  forums/<fid>/
    topics/<tid>.json                              # топик = источник истины
    scans/<tid>.scan.json                          # результат сантимент-анализа (см. Уровень 4)
    scan-queue.jsonl                               # очередь для /rank-scan-run (Уровень 4)
```

### Антибот-поведение (humanized)
- Jitter-delay между запросами `U[500ms, 2500ms]` (средняя скорость ≈ 0.67 rps — сознательно медленнее 1 rps).
- Reading-pause: раз в ~20 скачанных топиков — пауза 30-60с.
- `Referer` соответствует клик-through навигации (listing → topic → pagination).
- UA выбирается один раз при старте из пула в 4 реальных браузеров, стабилен на весь процесс.
- `Accept-Language: ru-RU,...`, `Accept-Encoding: gzip`.

---

## Уровень 4 — ранжирование фильмов по качеству

4-шаговый пайплайн. Шаги 1/2/4 — локальный Rust (офлайн). Шаг 3 — Haiku-сабагент в сессии Claude Code.

### Шаг 1 — сгруппировать топики в фильмы

```bash
rutracker rank match --forum "Фильмы 2026"
```
Парсит title (`{ru} / {en} ({director}) [{year}, ...]`), группирует по `(ru, en, year, director)` → `film_id = sha256(key)[:16]`. Сепаратор ключа — `\x1f` (не `|`, чтобы не коллидировало с названиями типа `"First|Last"`).

Невыпарсенные заголовки уходят в `$root/logs/rank-parse-failures.log` (смотреть через `rutracker rank parse-failures`).

### Шаг 2 — подготовить очередь сканирования

```bash
rutracker rank scan-prepare --forum "Фильмы 2026"
```
Пишет `forums/<fid>/scan-queue.jsonl` — по строке на топик, который надо сканировать. Кэш-ключ `(agent_sha, last_post_id)`: если файл `scans/<tid>.scan.json` уже матчит обоим — топик пропускается. Payload truncated to 8 KB (title + opening_post гарантированы, comments добавляются newest-first до бюджета).

### Шаг 3 — запустить анализ комментариев

**В открытой сессии Claude Code:**
```
/rank-scan-run --forum "Фильмы 2026"
```
Skill читает манифест, для каждой строки вызывает `Agent(subagent_type="rutracker-film-scanner", prompt=<payload>)`, парсит JSON-ответ (retry×1 при malformed JSON), атомарно пишет `scans/<tid>.scan.json`. Прогресс в `scan-queue.done.jsonl` — можно прерывать и перезапускать.

Haiku-агент возвращает: `sentiment_score (0-10)`, `confidence`, `themes_positive/negative`, `tech_complaints/praise` (audio/video/subs/dub/sync), `substantive_count`, `red_flags`, `relevance`.

### Шаг 4 — агрегировать и смотреть результаты

```bash
rutracker rank aggregate --forum "Фильмы 2026"
rutracker rank list --forum "Фильмы 2026" --top 20 --format text
rutracker rank show "Проект Конец света" --format text
```

Скор фильма — Bayesian-shrunk mean: `score = (5·5.5 + Σ(w·sentiment)) / (5 + Σw)`, `w = substantive_count × confidence × relevance`. Малое кол-во комментов → score тянется к prior=5.5 (избегаем случайных 9/10 на 1 коммент).

Лучший рип выбирается внутри фильма по формуле `0.40·tech + 0.20·format + 0.15·audio + 0.15·health + 0.10·recency`.

### Калибровка (release gate)

Перед шипингом изменений промпта агента:
```bash
scripts/calibrate-scanner.sh
```
Читает `crates/ranker/tests/fixtures/ranker/labels.jsonl` (пример: `labels.jsonl.example`), считает Spearman ρ между Haiku-скорами и ручными метками. **Release-blocking:** ρ ≥ 0.6.

---

## Уровень 5 — архитектура + разработка

### Крейты
```
crates/
  parser/          pure HTML → models (no I/O)
  http/            reqwest + cp1251 + humanized headers
  cookies-macos/   Brave AES-CBC + Keychain
  mirror/          incremental sync, SQLite schema, SyncEngine, SyncDriver
  ranker/          film-matching, scan-prepare, aggregator, rip-ranker
  cli/             binary `rutracker`
  mcp/             binary `rutracker-mcp` (MCP stdio server)
```

### Тесты, линт, формат
```bash
cargo test --workspace                             # должно быть ≥ 148 зелёных
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

### Установка новой сборки
```bash
cargo install --path crates/cli --locked
cargo install --path crates/mcp --locked           # для MCP
```
Переустановка необходима после любого изменения CLI/mirror/ranker/http — старый бинарь лежит в `~/.cargo/bin/rutracker`.

### Schema migrations
Forward-only, без down-migrations. Бинарь младше DB → отказ с сообщением "upgrade the binary". Миграции в `crates/mirror/migrations/000N_*.sql`, runner — `crates/mirror/src/migrate.rs :: apply_pending_migrations`.

### Ключевые файлы плана
- `.omc/plans/mirror-sync.md` — v1.1 мирра.
- `.omc/plans/sync-automation-humanization.md` — v1.2 auto-resume + humanization.
- `.omc/plans/quality-ranker.md` — v1.3 ранжирование.

### Что выключено/отложено
- MCP-тулы для `mirror_*` и `rank_*` — v1.4+ (CLI-only пока).
- Fuzzy film matching — если 4-поля ключа не совпадают exactly, фильмы считаются разными.
- Personal-taste model — только объективный скор консенсуса.

---

## Быстрый чек-лист диагностики

| Симптом | Проверить |
|---|---|
| `cookies missing bb_dl_key` | Открой rutracker в Brave, залогинься, перезапусти `rutracker download ...`. |
| `parser sanity check failed: 0 rows` (live) | Вероятно Cloudflare-challenge / expired cookies. См. `curl -L -b ...` вручную. |
| `SchemaTooNew` | DB новее бинаря → `cargo install --path crates/cli --locked` → retry. |
| `scan-queue.jsonl` пустой | Все топики уже scanned (agent_sha + last_post_id совпадают). Для принудительного rescan — удалить `forums/<fid>/scans/*.scan.json`. |
| `ambiguous forum name` | Ввод совпал с >1 именем → используй точное имя в кавычках или id. |
| Cooldown не проходит | `sqlite3 $HOME/.rutracker/mirror/state.db "UPDATE forum_state SET cooldown_until=NULL WHERE forum_id='252';"` — вручную сбросить. |

## Команды одной строкой

| Команда | Назначение |
|---|---|
| `rutracker search <q>` | Поиск |
| `rutracker topic <tid> --comments` | Детали топика |
| `rutracker browse <category_id>` | Список торрентов раздела без запроса |
| `rutracker download <tid> --out-dir <d>` | Скачать `.torrent` |
| `rutracker categories --format text` | Маппинг id ↔ имя |
| `rutracker mirror init` | Первичная инициализация |
| `rutracker mirror structure` | Обновить structure.json |
| `rutracker mirror watch add "<name>"` | Добавить в watchlist |
| `rutracker mirror watch remove "<name>"` | Убрать из watchlist |
| `rutracker mirror watch list` | Показать watchlist |
| `rutracker mirror sync --force-full` | Полный sync с auto-resume |
| `rutracker mirror status` | Состояние per-forum |
| `rutracker mirror show <fid>/<tid>` | Показать кэшированный топик |
| `rutracker mirror rebuild-index` | Восстановить state.db из JSON |
| `rutracker rank match` | Сгруппировать топики в фильмы |
| `rutracker rank scan-prepare` | Подготовить очередь сканирования |
| `rutracker rank aggregate` | Агрегировать scan-результаты |
| `rutracker rank list` | Список фильмов по скору |
| `rutracker rank show` | Детали одного фильма |
