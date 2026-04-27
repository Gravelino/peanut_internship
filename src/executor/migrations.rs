use rusqlite::Connection;

use crate::executor::errors::{ExecutorError, ExecutorResult};

#[derive(Debug, Clone, Copy)]
pub struct SqlMigration {
    pub version: u32,
    pub sql: &'static str,
}

pub fn migrate_executor_db(
    conn: &mut Connection,
    migrations: &[SqlMigration],
) -> ExecutorResult<()> {
    let current: u32 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|e| ExecutorError::Persistence(format!("read user_version: {e}")))?;

    for migration in migrations.iter().filter(|m| m.version > current) {
        let tx = conn.transaction().map_err(|e| {
            ExecutorError::Persistence(format!("begin migration v{}: {e}", migration.version))
        })?;
        tx.execute_batch(migration.sql).map_err(|e| {
            ExecutorError::Persistence(format!("apply migration v{}: {e}", migration.version))
        })?;
        tx.pragma_update(None, "user_version", migration.version)
            .map_err(|e| {
                ExecutorError::Persistence(format!("set user_version v{}: {e}", migration.version))
            })?;
        tx.commit().map_err(|e| {
            ExecutorError::Persistence(format!("commit migration v{}: {e}", migration.version))
        })?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_migrations_once_and_sets_user_version() {
        let mut conn = Connection::open_in_memory().unwrap();
        let migrations = [
            SqlMigration {
                version: 1,
                sql: "CREATE TABLE one(id INTEGER PRIMARY KEY);",
            },
            SqlMigration {
                version: 2,
                sql: "CREATE TABLE two(id INTEGER PRIMARY KEY);",
            },
        ];

        migrate_executor_db(&mut conn, &migrations).unwrap();
        migrate_executor_db(&mut conn, &migrations).unwrap();

        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2);
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('one', 'two')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
    }
}
