//! Schema version check. Forward-only — no down-migrations.
//!
//! On [`Mirror::open`](crate::Mirror::open) we read `schema_meta.schema_version`;
//! if the DB is *newer* than the binary, we refuse with [`Error::SchemaTooNew`]
//! and ask the user to upgrade. If the DB is *older*, future migrations will
//! run here; v1 has nothing to upgrade from so this is a no-op.

use crate::mirror::SCHEMA_VERSION;
use crate::state::State;
use crate::{Error, Result};

pub fn check_schema_version(state: &State) -> Result<()> {
    let db = state.schema_version()?;
    if db > SCHEMA_VERSION {
        return Err(Error::SchemaTooNew {
            binary: SCHEMA_VERSION,
            db,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::{Error, Mirror};
    use rusqlite::Connection;
    use tempfile::TempDir;

    #[test]
    fn test_init_seeds_schema_version_1() {
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
        assert_eq!(v, "1");
    }

    #[test]
    fn test_newer_db_refused_with_actionable_error() {
        let td = TempDir::new().unwrap();
        {
            let m = Mirror::init(td.path()).unwrap();
            drop(m);
        }

        // Simulate a future binary that bumped schema_version to 2.
        {
            let conn = Connection::open(td.path().join("state.db")).unwrap();
            conn.execute(
                "UPDATE schema_meta SET value = '2' WHERE key = 'schema_version'",
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
            matches!(err, Error::SchemaTooNew { binary: 1, db: 2 }),
            "expected SchemaTooNew{{binary:1, db:2}}, got {err:?}"
        );
        assert!(
            msg.contains("upgrade"),
            "error message must hint to upgrade the binary: {msg}"
        );
    }
}
