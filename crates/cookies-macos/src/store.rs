//! Chromium cookie SQLite store reader.

use crate::Result;
use rusqlite::OpenFlags;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct CookieRow {
    pub name: String,
    pub value: String,
    pub encrypted_value: Vec<u8>,
}

pub fn read_rutracker_rows(db_path: &Path) -> Result<Vec<CookieRow>> {
    read_rows_matching(db_path, "%rutracker.org")
}

pub fn read_rows_matching(db_path: &Path, host_like: &str) -> Result<Vec<CookieRow>> {
    let conn = rusqlite::Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    let mut stmt =
        conn.prepare("SELECT name, value, encrypted_value FROM cookies WHERE host_key LIKE ?1")?;
    let mut rows = Vec::new();
    let iter = stmt.query_map([host_like], |row| {
        Ok(CookieRow {
            name: row.get(0)?,
            value: row.get(1)?,
            encrypted_value: row.get(2)?,
        })
    })?;
    for r in iter {
        rows.push(r?);
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn make_fixture_db(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute(
            "CREATE TABLE cookies (name TEXT, value TEXT, encrypted_value BLOB, host_key TEXT)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO cookies (name, value, encrypted_value, host_key) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["bb_session", "abc", &b"enc"[..], ".rutracker.org"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO cookies (name, value, encrypted_value, host_key) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["bb_guid", "xyz", &b""[..], ".rutracker.org"],
        )
        .unwrap();
        // Unrelated domain — must be filtered out.
        conn.execute(
            "INSERT INTO cookies (name, value, encrypted_value, host_key) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["ga", "x", &b""[..], ".other.example"],
        )
        .unwrap();
    }

    #[test]
    fn test_read_rutracker_rows_filters_by_host() {
        let dir = std::env::temp_dir().join(format!("rutracker-store-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("Cookies");
        make_fixture_db(&db);

        let rows = read_rutracker_rows(&db).unwrap();
        let names: Vec<String> = rows.iter().map(|r| r.name.clone()).collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"bb_session".to_string()));
        assert!(names.contains(&"bb_guid".to_string()));
        // Value and encrypted_value round-trip.
        let sess = rows.iter().find(|r| r.name == "bb_session").unwrap();
        assert_eq!(sess.value, "abc");
        assert_eq!(sess.encrypted_value, b"enc".to_vec());
    }

    #[test]
    fn test_read_rows_matching_custom_host() {
        let dir =
            std::env::temp_dir().join(format!("rutracker-store-test2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("Cookies");
        make_fixture_db(&db);

        let rows = read_rows_matching(&db, "%other.example").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "ga");
    }

    #[test]
    fn test_read_missing_db_errors() {
        let bad = std::path::PathBuf::from("/nonexistent/rutracker-missing.db");
        let err = read_rutracker_rows(&bad).unwrap_err();
        // Wraps a rusqlite::Error via #[from].
        assert!(
            matches!(err, crate::Error::Sqlite(_)),
            "expected Sqlite error, got {err:?}"
        );
    }
}
