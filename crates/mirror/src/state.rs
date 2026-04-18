//! SQLite (`state.db`) wrapper: schema init, version query, lazy backfill.

use std::collections::HashSet;
use std::path::Path;

use rusqlite::{params, Connection};

use crate::Result;

const MIGRATION_0001: &str = include_str!("../migrations/0001_init.sql");

pub struct State {
    conn: Connection,
}

impl State {
    /// Open an existing `state.db`. Does not run any migration.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        apply_pragmas(&conn)?;
        Ok(Self { conn })
    }

    /// Create `state.db` and apply migration `0001_init.sql`.
    pub fn init(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        apply_pragmas(&conn)?;
        conn.execute_batch(MIGRATION_0001)?;
        Ok(Self { conn })
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// Read `schema_meta.schema_version` as a `u32`.
    pub fn schema_version(&self) -> Result<u32> {
        let raw: String = self.conn.query_row(
            "SELECT value FROM schema_meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )?;
        Ok(raw.parse::<u32>().unwrap_or(0))
    }

    /// Insert any topic JSON files in `topics_dir` that are absent from
    /// `topic_index`. Returns the number of rows inserted. Idempotent.
    pub fn backfill_missing_index_rows(
        &mut self,
        forum_id: &str,
        topics_dir: &Path,
    ) -> Result<usize> {
        if !topics_dir.exists() {
            return Ok(0);
        }

        let mut existing: HashSet<String> = HashSet::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT topic_id FROM topic_index WHERE forum_id = ?1")?;
            let rows = stmt.query_map([forum_id], |r| r.get::<_, String>(0))?;
            for r in rows {
                existing.insert(r?);
            }
        }

        let tx = self.conn.transaction()?;
        let mut inserted = 0usize;
        for entry in std::fs::read_dir(topics_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if existing.contains(stem) {
                continue;
            }

            let bytes = std::fs::read(&path)?;
            let v: serde_json::Value = serde_json::from_slice(&bytes)?;

            let topic_id = stem.to_string();
            let title = json_str(&v, "title");
            let last_post_id = json_scalar_to_string(&v, "last_post_id");
            let last_post_at = json_str(&v, "last_post_at");
            let fetched_at = json_str(&v, "fetched_at");

            tx.execute(
                "INSERT INTO topic_index \
                 (forum_id, topic_id, title, last_post_id, last_post_at, fetched_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    forum_id,
                    topic_id,
                    title,
                    last_post_id,
                    last_post_at,
                    fetched_at
                ],
            )?;
            inserted += 1;
        }
        tx.commit()?;
        Ok(inserted)
    }
}

fn apply_pragmas(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(())
}

fn json_str(v: &serde_json::Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

fn json_scalar_to_string(v: &serde_json::Value, key: &str) -> String {
    match v.get(key) {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        Some(other) if !other.is_null() => other.to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use crate::Mirror;
    use tempfile::TempDir;

    #[test]
    fn test_lazy_backfill_populates_missing_rows() {
        let td = TempDir::new().unwrap();
        let mut m = Mirror::init(td.path()).unwrap();

        let topics = td.path().join("forums").join("252").join("topics");
        std::fs::create_dir_all(&topics).unwrap();
        std::fs::write(
            topics.join("123.json"),
            r#"{
                "schema_version": 1,
                "topic_id": 123,
                "forum_id": "252",
                "title": "Some topic",
                "last_post_id": 987654,
                "last_post_at": "11-Apr-26 07:23",
                "fetched_at": "2026-04-18T15:00:05Z"
            }"#,
        )
        .unwrap();

        let inserted = m.backfill_missing_index_rows("252").unwrap();
        assert_eq!(inserted, 1, "exactly one new row should be inserted");

        let count: i64 = m
            .state()
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM topic_index \
                 WHERE forum_id = '252' AND topic_id = '123'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        // Idempotency: second call is a no-op.
        let again = m.backfill_missing_index_rows("252").unwrap();
        assert_eq!(again, 0);
    }
}
