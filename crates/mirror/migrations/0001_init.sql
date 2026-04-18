-- rutracker-mirror state.db — schema v1.
-- Applied by State::init(). See .omc/plans/mirror-sync.md §4.1.

CREATE TABLE schema_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

INSERT INTO schema_meta (key, value) VALUES ('schema_version', '1');

CREATE TABLE forum_state (
    forum_id                TEXT PRIMARY KEY,
    name                    TEXT,
    parent_group_id         TEXT,
    last_sync_started_at    TEXT,
    last_sync_completed_at  TEXT,
    last_sync_outcome       TEXT,
    cooldown_until          TEXT,
    topic_high_water_mark   TEXT,
    topics_count            INTEGER
);

CREATE TABLE topic_index (
    forum_id      TEXT NOT NULL,
    topic_id      TEXT NOT NULL,
    title         TEXT,
    last_post_id  TEXT,
    last_post_at  TEXT,
    fetched_at    TEXT,
    PRIMARY KEY (forum_id, topic_id)
) WITHOUT ROWID;

CREATE INDEX idx_topic_by_id ON topic_index(topic_id);
