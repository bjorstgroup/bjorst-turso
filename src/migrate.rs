//! Forward-only migrations, applied in version order, each in one transaction.
//!
//! The applied SQL is checksummed into `_migrations`; editing a migration after
//! it ran is an error rather than a silent drift between environments.

use crate::{Db, Error, Result};

/// One migration. `version` orders them; `sql` may hold several statements.
pub struct Migration {
    pub version: i64,
    pub name: &'static str,
    pub sql: &'static str,
}

/// FNV-1a: a drift detector, not a security boundary.
fn checksum(sql: &str) -> i64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in sql.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
    }
    h as i64
}

/// Apply every migration not yet recorded. Safe to call at every start.
pub async fn migrate(db: &Db, migrations: &[Migration]) -> Result<()> {
    let conn = db.connection()?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS _migrations (
            version INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            checksum INTEGER NOT NULL,
            applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
        )",
        (),
    )
    .await?;

    let mut ordered: Vec<&Migration> = migrations.iter().collect();
    ordered.sort_by_key(|m| m.version);

    for m in ordered {
        let sum = checksum(m.sql);
        let mut rows = conn
            .query("SELECT checksum FROM _migrations WHERE version = ?1", [m.version])
            .await?;
        if let Some(row) = rows.next().await? {
            if row.get::<i64>(0)? != sum {
                return Err(Error::ChecksumMismatch { version: m.version });
            }
            continue;
        }
        drop(rows);
        let tx = conn.transaction().await?;
        tx.execute_batch(m.sql).await?;
        tx.execute(
            "INSERT INTO _migrations (version, name, checksum) VALUES (?1, ?2, ?3)",
            libsql::params![m.version, m.name, sum],
        )
        .await?;
        tx.commit().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TursoConfig;

    async fn db() -> Db {
        Db::connect(&TursoConfig { url: ":memory:".into(), auth_token: None })
            .await
            .unwrap()
    }

    const ONE: Migration = Migration { version: 1, name: "t", sql: "CREATE TABLE t (x INTEGER);" };

    #[tokio::test]
    async fn applies_once_and_is_idempotent() {
        let db = db().await;
        migrate(&db, &[ONE]).await.unwrap();
        migrate(&db, &[ONE]).await.unwrap();
        let mut r = db.connection().unwrap().query("SELECT count(*) FROM _migrations", ()).await.unwrap();
        assert_eq!(r.next().await.unwrap().unwrap().get::<i64>(0).unwrap(), 1);
    }

    #[tokio::test]
    async fn an_edited_migration_is_refused() {
        let db = db().await;
        migrate(&db, &[ONE]).await.unwrap();
        let edited = Migration { version: 1, name: "t", sql: "CREATE TABLE t (y INTEGER);" };
        assert!(matches!(migrate(&db, &[edited]).await, Err(Error::ChecksumMismatch { version: 1 })));
    }

    #[tokio::test]
    async fn a_failing_migration_leaves_nothing_behind() {
        let db = db().await;
        let bad = Migration { version: 1, name: "bad", sql: "CREATE TABLE a (x); NOT SQL;" };
        assert!(migrate(&db, &[bad]).await.is_err());
        let mut r = db.connection().unwrap().query("SELECT count(*) FROM sqlite_master WHERE name='a'", ()).await.unwrap();
        assert_eq!(r.next().await.unwrap().unwrap().get::<i64>(0).unwrap(), 0);
    }
}
