//! Turso (libSQL) connection builder and a versioned migration runner.
//!
//! One [`Db`] per process, cloned freely. [`Db::connect`] picks the transport
//! from the URL: `libsql://…` or `https://…` talks to Turso over the network
//! and needs a token; `file:…` and `:memory:` open a local database, which is
//! what tests use.
//!
//! A local `:memory:` database is private to the connection that opened it, so
//! [`Db::connection`] hands out the one connection for `:memory:` and a fresh
//! one otherwise. Tests that need several connections should use a temp file.

mod migrate;

pub use libsql::{params, Connection, Row, Rows, Value};
pub use migrate::{migrate, Migration};

use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("turso: {0}")]
    Libsql(#[from] libsql::Error),
    #[error("a remote url ({0}) needs an auth token")]
    MissingToken(String),
    #[error("migration {version} was applied with different SQL than it has now")]
    ChecksumMismatch { version: i64 },
}

pub type Result<T> = std::result::Result<T, Error>;

/// Where the database is and how to sign in to it.
#[derive(Clone)]
pub struct TursoConfig {
    /// `libsql://name-org.turso.io`, `https://…`, `file:path.db` or `:memory:`.
    pub url: String,
    /// Required for a remote url, ignored for a local one.
    pub auth_token: Option<String>,
}

impl std::fmt::Debug for TursoConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TursoConfig")
            .field("url", &self.url)
            .field("auth_token", &self.auth_token.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

impl TursoConfig {
    fn is_remote(&self) -> bool {
        self.url.starts_with("libsql://")
            || self.url.starts_with("https://")
            || self.url.starts_with("http://")
    }
}

/// A handle to the database. Cheap to clone.
#[derive(Clone)]
pub struct Db {
    inner: Arc<libsql::Database>,
    /// `:memory:` only — see the module docs.
    shared: Option<Connection>,
}

impl Db {
    pub async fn connect(cfg: &TursoConfig) -> Result<Self> {
        let (inner, shared) = if cfg.is_remote() {
            let token = cfg
                .auth_token
                .clone()
                .filter(|t| !t.is_empty())
                .ok_or_else(|| Error::MissingToken(cfg.url.clone()))?;
            let db = libsql::Builder::new_remote(cfg.url.clone(), token)
                .build()
                .await?;
            (db, None)
        } else {
            let path = cfg.url.strip_prefix("file:").unwrap_or(&cfg.url);
            let db = libsql::Builder::new_local(path).build().await?;
            let shared = (path == ":memory:").then(|| db.connect()).transpose()?;
            (db, shared)
        };
        Ok(Self {
            inner: Arc::new(inner),
            shared,
        })
    }

    pub fn connection(&self) -> Result<Connection> {
        match &self.shared {
            Some(c) => Ok(c.clone()),
            None => Ok(self.inner.connect()?),
        }
    }

    /// `SELECT 1`.
    pub async fn healthcheck(&self) -> Result<()> {
        let mut rows = self.connection()?.query("SELECT 1", ()).await?;
        rows.next().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory() -> TursoConfig {
        TursoConfig {
            url: ":memory:".into(),
            auth_token: None,
        }
    }

    #[tokio::test]
    async fn a_local_database_answers() {
        Db::connect(&memory()).await.unwrap().healthcheck().await.unwrap();
    }

    #[tokio::test]
    async fn memory_connections_see_each_other() {
        let db = Db::connect(&memory()).await.unwrap();
        db.connection().unwrap().execute("CREATE TABLE t (x)", ()).await.unwrap();
        db.connection().unwrap().execute("INSERT INTO t VALUES (1)", ()).await.unwrap();
    }

    #[tokio::test]
    async fn a_remote_url_without_a_token_is_refused() {
        let cfg = TursoConfig {
            url: "libsql://x-y.turso.io".into(),
            auth_token: None,
        };
        assert!(matches!(Db::connect(&cfg).await, Err(Error::MissingToken(_))));
    }

    #[test]
    fn debug_does_not_print_the_token() {
        let cfg = TursoConfig {
            url: "libsql://x".into(),
            auth_token: Some("secret-token".into()),
        };
        assert!(!format!("{cfg:?}").contains("secret-token"));
    }
}
