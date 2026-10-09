//! Forward-only migrations, applied in version order, each in one transaction.
//!
//! The applied SQL is checksummed into `_migrations`; editing a migration after
//! it ran is an error rather than a silent drift between environments.

use crate::{params, Db, Error, Result};

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
    db.execute(
        "CREATE TABLE IF NOT EXISTS _migrations (
            version INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            checksum INTEGER NOT NULL,
            applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
        )",
        &[],
    )
    .await?;

    let mut ordered: Vec<&Migration> = migrations.iter().collect();
    ordered.sort_by_key(|m| m.version);

    for m in ordered {
        let sum = checksum(m.sql);
        if let Some(row) = db
            .query_opt(
                "SELECT checksum FROM _migrations WHERE version = ?1",
                &params![m.version],
            )
            .await?
        {
            if row.get::<i64>(0)? != sum {
                return Err(Error::ChecksumMismatch { version: m.version });
            }
            continue;
        }
        let mut tx = db.begin().await?;
        let applied = async {
            tx.execute_batch(m.sql).await?;
            tx.execute(
                "INSERT INTO _migrations (version, name, checksum) VALUES (?1, ?2, ?3)",
                &params![m.version, m.name, sum],
            )
            .await
        }
        .await;
        match applied {
            Ok(_) => tx.commit().await?,
            Err(e) => {
                let _ = tx.rollback().await;
                return Err(e);
            }
        }
    }
    Ok(())
}
