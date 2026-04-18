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
