//! Forward-only SQLite schema migrations.
//!
//! Each migration file (`migrations/<NNNN>_<name>.sql`) is embedded via
//! `include_str!` and registered in the [`MIGRATIONS`] table. Migrations run in
//! order within a single transaction each; the last statement of every
//! migration is the corresponding `UPDATE schema_meta SET value = '<N>' …`.
//!
//! [`ensure_schema`] is the single entry point called by
//! [`crate::Mirror::init`] and [`crate::Mirror::open`]:
//! - db_version == SCHEMA_VERSION → no-op
//! - db_version <  SCHEMA_VERSION → [`apply_pending_migrations`]
//! - db_version >  SCHEMA_VERSION → [`Error::SchemaTooNew`]
//!
//! Empty-DB case: when `schema_meta` does not exist, the DB is treated as
//! version 0 and all migrations are applied in order, creating the table as
//! part of migration 0001.

use rusqlite::Connection;

use crate::mirror::SCHEMA_VERSION;
use crate::{Error, Result};

/// Registered migrations, in ascending version order. Each entry is
/// `(version, sql)`. The SQL must end with
/// `UPDATE schema_meta SET value = '<version>' WHERE key = 'schema_version'`
/// (or seed the row when applying 0001 on an empty DB).
pub const MIGRATIONS: &[(u32, &str)] = &[
    (1, include_str!("../migrations/0001_init.sql")),
    (2, include_str!("../migrations/0002_ranker.sql")),
];

/// Read `schema_meta.schema_version`. Returns 0 if the table does not exist
/// (empty DB) so the migration runner can bootstrap from scratch.
pub fn read_db_version(conn: &Connection) -> Result<u32> {
    // Does schema_meta exist?
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_meta'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|_| true)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(false),
            other => Err(other),
        })?;
    if !exists {
        return Ok(0);
    }

    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM schema_meta WHERE key = 'schema_version'",
            [],
            |r| r.get::<_, String>(0),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;

    Ok(raw.and_then(|s| s.parse::<u32>().ok()).unwrap_or(0))
}

/// Apply all registered migrations whose version is greater than the DB's
/// current version. Each migration runs inside its own transaction, so a
/// failed migration leaves the DB at the previous version.
///
/// Returns the list of versions applied, in order.
pub fn apply_pending_migrations(conn: &mut Connection) -> Result<Vec<u32>> {
    apply_pending_migrations_with(conn, MIGRATIONS)
}

/// Internal helper so tests can inject a malformed-migration table.
fn apply_pending_migrations_with(
    conn: &mut Connection,
    migrations: &[(u32, &str)],
) -> Result<Vec<u32>> {
    let current = read_db_version(conn)?;
    let mut applied = Vec::new();
    for &(version, sql) in migrations {
        if version <= current {
            continue;
        }
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.commit()?;
        applied.push(version);
    }
    Ok(applied)
}

/// Bring the DB up to [`SCHEMA_VERSION`].
///
/// - Equal → no-op.
/// - Older → apply pending migrations.
/// - Newer → [`Error::SchemaTooNew`].
pub fn ensure_schema(conn: &mut Connection) -> Result<()> {
    let db = read_db_version(conn)?;
    if db > SCHEMA_VERSION {
        return Err(Error::SchemaTooNew {
            binary: SCHEMA_VERSION,
            db,
        });
    }
    if db < SCHEMA_VERSION {
        apply_pending_migrations(conn)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Error, Mirror};
    use rusqlite::Connection;
    use tempfile::TempDir;

    fn fresh_db_at_v1() -> (TempDir, std::path::PathBuf) {
        let td = TempDir::new().unwrap();
        let path = td.path().join("state.db");
        let mut conn = Connection::open(&path).unwrap();
        // Apply only migration 0001, leaving DB at v1 so v2 is still pending.
        apply_pending_migrations_with(&mut conn, &MIGRATIONS[..1]).unwrap();
        (td, path)
    }

    #[test]
    fn test_init_seeds_schema_version_2() {
        let td = TempDir::new().unwrap();
        let m = Mirror::init(td.path()).unwrap();
        drop(m);

        let conn = Connection::open(td.path().join("state.db")).unwrap();
        let v: String = conn
            .query_row(
                "SELECT value FROM schema_meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, "2");
    }

    #[test]
    fn test_apply_pending_from_v1_to_v2_adds_film_tables() {
        let (_td, path) = fresh_db_at_v1();

        // Sanity: we're really at v1.
        {
            let conn = Connection::open(&path).unwrap();
            assert_eq!(read_db_version(&conn).unwrap(), 1);
        }

        let mut conn = Connection::open(&path).unwrap();
        let applied = apply_pending_migrations(&mut conn).unwrap();
        assert_eq!(applied, vec![2]);

        // film_index / film_topic / film_score now exist.
        for table in ["film_index", "film_topic", "film_score"] {
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(exists, 1, "table `{table}` should exist after migration");
        }

        let v: String = conn
            .query_row(
                "SELECT value FROM schema_meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, "2");
    }

    #[test]
    fn test_no_op_when_already_at_version() {
        let td = TempDir::new().unwrap();
        let m = Mirror::init(td.path()).unwrap();
        drop(m);

        let mut conn = Connection::open(td.path().join("state.db")).unwrap();
        let applied = apply_pending_migrations(&mut conn).unwrap();
        assert!(
            applied.is_empty(),
            "no migrations should be applied when already at current version, got {applied:?}"
        );
        // ensure_schema is also a no-op.
        ensure_schema(&mut conn).unwrap();
    }

    #[test]
    fn test_migration_rolls_back_on_sql_error() {
        let (_td, path) = fresh_db_at_v1();

        let bad_migrations: &[(u32, &str)] = &[
            (1, MIGRATIONS[0].1),
            (
                2,
                "CREATE TABLE film_index (film_id TEXT PRIMARY KEY); \
                 THIS IS NOT VALID SQL;",
            ),
        ];

        let mut conn = Connection::open(&path).unwrap();
        let err = apply_pending_migrations_with(&mut conn, bad_migrations)
            .expect_err("malformed migration must error");
        // Must surface as a SQLite error.
        assert!(
            matches!(err, Error::Sqlite(_)),
            "expected SQLite error, got {err:?}"
        );

        // DB remains at v1 — the failed tx was rolled back.
        let db = read_db_version(&conn).unwrap();
        assert_eq!(db, 1, "DB must stay at v1 after failed migration");

        // film_index must NOT exist (the whole batch was rolled back).
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='film_index'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "film_index must not exist after rollback");
    }

    #[test]
    fn test_newer_db_still_refused_with_actionable_error() {
        let td = TempDir::new().unwrap();
        {
            let m = Mirror::init(td.path()).unwrap();
            drop(m);
        }

        // Simulate a future binary that bumped schema_version to 99.
        {
            let conn = Connection::open(td.path().join("state.db")).unwrap();
            conn.execute(
                "UPDATE schema_meta SET value = '99' WHERE key = 'schema_version'",
                [],
            )
            .unwrap();
        }

        let err = match Mirror::open(td.path(), None) {
            Err(e) => e,
            Ok(_) => panic!("expected SchemaTooNew, got Ok(Mirror)"),
        };
        let msg = format!("{err}");
        assert!(
            matches!(err, Error::SchemaTooNew { binary: 2, db: 99 }),
            "expected SchemaTooNew{{binary:2, db:99}}, got {err:?}"
        );
        assert!(
            msg.contains("upgrade"),
            "error message must hint to upgrade the binary: {msg}"
        );
    }
}
