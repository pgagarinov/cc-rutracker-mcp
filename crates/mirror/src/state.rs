//! SQLite (`state.db`) wrapper: schema init, version query, lazy backfill.

use std::collections::HashSet;
use std::path::Path;

use rusqlite::{params, Connection};

use crate::Result;

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

    /// Create a fresh `state.db`. Migrations are applied by the caller via
    /// [`crate::migrate::ensure_schema`], which handles the empty-DB case
    /// (no `schema_meta` table → treat as version 0 → apply all).
    pub fn init(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        apply_pragmas(&conn)?;
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

    /// US-008: `backfill_missing_index_rows` returns `0` without touching the
    /// DB when the topics directory does not exist (L56–L58). Previously
    /// only the "directory exists and is empty" case was exercised.
    #[test]
    fn test_backfill_returns_zero_when_topics_dir_absent() {
        let td = TempDir::new().unwrap();
        let mut m = Mirror::init(td.path()).unwrap();
        // forums/252/topics intentionally not created.
        let got = m.backfill_missing_index_rows("252").unwrap();
        assert_eq!(got, 0, "absent topics dir must yield zero insertions");
    }

    /// US-008: non-`.json` files in the topics directory must be skipped
    /// (L76–L78). We drop a README.md alongside one valid topic JSON and
    /// expect exactly one insertion.
    #[test]
    fn test_backfill_skips_non_json_files() {
        let td = TempDir::new().unwrap();
        let mut m = Mirror::init(td.path()).unwrap();
        let topics = td.path().join("forums").join("252").join("topics");
        std::fs::create_dir_all(&topics).unwrap();
        // Non-JSON sibling — must be skipped.
        std::fs::write(topics.join("README.md"), "ignore me").unwrap();
        // Valid topic JSON.
        std::fs::write(
            topics.join("777.json"),
            r#"{"topic_id":777,"title":"t","last_post_id":1,"last_post_at":"x","fetched_at":"y"}"#,
        )
        .unwrap();
        let inserted = m.backfill_missing_index_rows("252").unwrap();
        assert_eq!(
            inserted, 1,
            "only the .json file must be inserted; README.md is skipped"
        );
    }

    /// US-008: `json_scalar_to_string` must coerce numeric `last_post_id`
    /// fields (which appear as `Number` in the topic JSON) into the canonical
    /// text representation. Covers the `Number(n) => n.to_string()` branch
    /// (L131) via a topic JSON with an integer `last_post_id`.
    #[test]
    fn test_backfill_accepts_numeric_last_post_id() {
        let td = TempDir::new().unwrap();
        let mut m = Mirror::init(td.path()).unwrap();
        let topics = td.path().join("forums").join("252").join("topics");
        std::fs::create_dir_all(&topics).unwrap();
        std::fs::write(
            topics.join("42.json"),
            // last_post_id is a JSON number, not a string — exercises the
            // Number(n) arm of json_scalar_to_string.
            r#"{"title":"Numeric LPI","last_post_id":123456,"last_post_at":"x","fetched_at":"y"}"#,
        )
        .unwrap();
        let inserted = m.backfill_missing_index_rows("252").unwrap();
        assert_eq!(inserted, 1);
        let stored: String = m
            .state()
            .conn()
            .query_row(
                "SELECT last_post_id FROM topic_index WHERE topic_id = '42'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            stored, "123456",
            "numeric last_post_id must be stringified to \"123456\""
        );
    }

    /// US-008: `json_scalar_to_string` must also handle a "non-null other"
    /// type (bool/array/object). Covers the `Some(other) if !other.is_null()`
    /// arm at L132. We feed `last_post_id: true` for coverage.
    #[test]
    fn test_backfill_handles_non_scalar_last_post_id_via_string_form() {
        let td = TempDir::new().unwrap();
        let mut m = Mirror::init(td.path()).unwrap();
        let topics = td.path().join("forums").join("252").join("topics");
        std::fs::create_dir_all(&topics).unwrap();
        std::fs::write(
            topics.join("43.json"),
            r#"{"title":"Weird LPI","last_post_id":true,"last_post_at":"x","fetched_at":"y"}"#,
        )
        .unwrap();
        let inserted = m.backfill_missing_index_rows("252").unwrap();
        assert_eq!(inserted, 1);
        let stored: String = m
            .state()
            .conn()
            .query_row(
                "SELECT last_post_id FROM topic_index WHERE topic_id = '43'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        // serde_json::Value::to_string on bool renders "true".
        assert_eq!(
            stored, "true",
            "non-scalar last_post_id must fall through to Value::to_string"
        );
    }

    /// US-008: missing fields (`last_post_id`, `last_post_at`, `fetched_at`
    /// absent or null) must produce empty strings. Covers the `_ => String::new()`
    /// arm at L133 of `json_scalar_to_string` and the `unwrap_or_default` in
    /// `json_str`.
    #[test]
    fn test_backfill_missing_fields_produce_empty_strings() {
        let td = TempDir::new().unwrap();
        let mut m = Mirror::init(td.path()).unwrap();
        let topics = td.path().join("forums").join("252").join("topics");
        std::fs::create_dir_all(&topics).unwrap();
        // Only `title` is present.
        std::fs::write(topics.join("44.json"), r#"{"title":"Only title"}"#).unwrap();
        let inserted = m.backfill_missing_index_rows("252").unwrap();
        assert_eq!(inserted, 1);
        let (lpid, lpat, fat): (String, String, String) = m
            .state()
            .conn()
            .query_row(
                "SELECT last_post_id, last_post_at, fetched_at \
                 FROM topic_index WHERE topic_id = '44'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(lpid, "");
        assert_eq!(lpat, "");
        assert_eq!(fat, "");
    }
}
