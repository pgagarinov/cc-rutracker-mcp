-- rutracker-mirror state.db — schema v2.
-- Adds ranker tables (film_index / film_topic / film_score) per quality-ranker plan §6.1.

CREATE TABLE film_index (
  film_id TEXT PRIMARY KEY,
  title_ru TEXT,
  title_en TEXT,
  title_alt TEXT,
  year INT,
  director TEXT,
  first_seen TEXT,
  last_seen TEXT
);

CREATE TABLE film_topic (
  film_id TEXT NOT NULL,
  topic_id TEXT NOT NULL,
  forum_id TEXT NOT NULL,
  seeds INT,
  leeches INT,
  downloads INT,
  size_bytes INT,
  format_tag TEXT,
  fetched_at TEXT,
  PRIMARY KEY (film_id, topic_id)
) WITHOUT ROWID;

CREATE INDEX idx_film_topic_by_topic ON film_topic(topic_id);

CREATE TABLE film_score (
  film_id TEXT PRIMARY KEY,
  score REAL,
  confidence REAL,
  topic_count_with_analysis INT,
  topic_count_total INT,
  total_substantive_comments INT,
  top_themes_positive TEXT,   -- JSON
  top_themes_negative TEXT,   -- JSON
  has_red_flags INT,
  scored_at TEXT
);

UPDATE schema_meta SET value = '2' WHERE key = 'schema_version';
