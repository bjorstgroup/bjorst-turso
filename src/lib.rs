//! Turso client and a versioned migration runner.
//!
//! [`Db::connect`] picks the transport from the URL: `libsql://…` / `https://…`
//! talks to Turso over Hrana/HTTP (needs a token); `file:…` and `:memory:` open
//! a local libSQL database, which is what tests use. Both answer the same
//! calls, so callers never see which they got.
//!
//! Dates are ISO-8601 text, ids text, money integer minor units: SQLite has no
//! uuid, timestamptz or numeric type.
//!
//! `:memory:` shares one connection (each in-memory connection is otherwise its
//! own database), so a transaction there blocks nothing and rolls back nothing
//! on drop. Use a temp file for anything concurrent.

mod builder;
mod hrana;
mod migrate;
mod value;

pub use builder::QueryBuilder;
pub use migrate::{migrate, Migration};
pub use value::{FromValue, Value};

use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("turso: {0}")]
    Local(#[from] libsql::Error),
    #[error("turso: request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("turso: {message}{}", code.as_deref().map(|c| format!(" ({c})")).unwrap_or_default())]
    Sql {
        code: Option<String>,
        message: String,
    },
    #[error("turso: unexpected response: {0}")]
    Decode(String),
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
            .field(
                "auth_token",
                &self.auth_token.as_ref().map(|_| "[redacted]"),
            )
            .finish()
    }
}

/// One result row.
#[derive(Debug, Clone)]
pub struct Row {
    cols: Arc<Vec<String>>,
    vals: Vec<Value>,
}

/// A column, by position or by name.
pub trait Idx {
    fn find(&self, row: &Row) -> Option<usize>;
}
impl Idx for usize {
    fn find(&self, row: &Row) -> Option<usize> {
        (*self < row.vals.len()).then_some(*self)
    }
}
impl Idx for &str {
    fn find(&self, row: &Row) -> Option<usize> {
        row.cols.iter().position(|c| c == self)
    }
}

impl Row {
    pub fn get<T: FromValue>(&self, idx: impl Idx) -> Result<T> {
        let i = idx
            .find(self)
            .ok_or_else(|| Error::Decode("no such column".into()))?;
        T::from_value(&self.vals[i]).map_err(|e| match e {
            Error::Decode(m) => Error::Decode(format!(
                "column {}: {m}",
                self.cols.get(i).map_or("?", String::as_str)
            )),
            e => e,
        })
    }
}

/// A type built from a result row. See [`row_struct!`].
pub trait FromRow: Sized {
    fn from_row(row: &Row) -> Result<Self>;
}

/// Declare a struct and read it from a row by field name:
///
/// ```ignore
/// bjorst_turso::row_struct! {
///     struct AccountRow { id: i64, name: String, note: Option<String> }
/// }
/// ```
#[macro_export]
macro_rules! row_struct {
    ($(#[$m:meta])* $vis:vis struct $name:ident { $($(#[$fm:meta])* $fvis:vis $f:ident : $t:ty),* $(,)? }) => {
        $(#[$m])* $vis struct $name { $($(#[$fm])* $fvis $f: $t),* }
        impl $crate::FromRow for $name {
            fn from_row(row: &$crate::Row) -> $crate::Result<Self> {
                Ok(Self { $($f: row.get(stringify!($f))?),* })
            }
        }
    };
}

macro_rules! tuple_rows {
    ($(($($t:ident $i:tt),+)),+) => {$(
        impl<$($t: FromValue),+> FromRow for ($($t,)+) {
            fn from_row(row: &Row) -> Result<Self> {
                Ok(($(row.get::<$t>($i)?,)+))
            }
        }
    )+};
}
tuple_rows!(
    (A 0), (A 0, B 1), (A 0, B 1, C 2), (A 0, B 1, C 2, D 3), (A 0, B 1, C 2, D 3, E 4),
    (A 0, B 1, C 2, D 3, E 4, F 5), (A 0, B 1, C 2, D 3, E 4, F 5, G 6),
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7)
);

fn all<T: FromRow>(rows: Vec<Row>) -> Result<Vec<T>> {
    rows.iter().map(T::from_row).collect()
}

fn one<T>(found: Option<T>) -> Result<T> {
    found.ok_or_else(|| Error::Decode("expected one row, got none".into()))
}

enum Backend {
    Remote(hrana::Remote),
    Local {
        db: libsql::Database,
        /// `:memory:` only.
        shared: Option<libsql::Connection>,
    },
}

/// A handle to the database. Cheap to clone.
#[derive(Clone)]
pub struct Db {
    backend: Arc<Backend>,
}

impl Db {
    pub async fn connect(cfg: &TursoConfig) -> Result<Self> {
        let remote = ["libsql://", "https://", "http://"]
            .iter()
            .any(|p| cfg.url.starts_with(p));
        let backend = if remote {
            // Plain http is a local `turso dev` server, which wants no token.
            let token = cfg.auth_token.clone().filter(|t| !t.is_empty());
            let token = match token {
                Some(t) => t,
                None if cfg.url.starts_with("http://") => String::new(),
                None => return Err(Error::MissingToken(cfg.url.clone())),
            };
            Backend::Remote(hrana::Remote::new(&cfg.url, token))
        } else {
            let path = cfg.url.strip_prefix("file:").unwrap_or(&cfg.url);
            let db = libsql::Builder::new_local(path).build().await?;
            let shared = (path == ":memory:").then(|| db.connect()).transpose()?;
            if let Some(c) = &shared {
                c.query("PRAGMA foreign_keys = ON", ()).await?;
            }
            Backend::Local { db, shared }
        };
        Ok(Self {
            backend: Arc::new(backend),
        })
    }

    /// A handle to a remote database without connecting: nothing is sent until
    /// the first call. For tests that point at an address nobody listens on.
    pub fn remote(url: &str, token: impl Into<String>) -> Self {
        Self {
            backend: Arc::new(Backend::Remote(hrana::Remote::new(url, token.into()))),
        }
    }

    /// A local connection with foreign keys on, as Turso has them.
    async fn local_conn(&self) -> Result<Option<libsql::Connection>> {
        match &*self.backend {
            Backend::Remote(_) => Ok(None),
            Backend::Local {
                shared: Some(c), ..
            } => Ok(Some(c.clone())),
            Backend::Local { db, .. } => {
                let c = db.connect()?;
                c.query("PRAGMA foreign_keys = ON", ()).await?;
                c.query("PRAGMA busy_timeout = 5000", ()).await?;
                Ok(Some(c))
            }
        }
    }

    /// Run a statement; the number of rows it changed.
    pub async fn execute(&self, sql: &str, params: &[Value]) -> Result<u64> {
        match &*self.backend {
            Backend::Remote(r) => Ok(r.execute(None, false, sql, params).await?.0.affected),
            Backend::Local { .. } => {
                local_execute(&self.local_conn().await?.unwrap(), sql, params).await
            }
        }
    }

    pub async fn query(&self, sql: &str, params: &[Value]) -> Result<Vec<Row>> {
        match &*self.backend {
            Backend::Remote(r) => Ok(rows_of(r.execute(None, false, sql, params).await?.0)),
            Backend::Local { .. } => {
                local_query(&self.local_conn().await?.unwrap(), sql, params).await
            }
        }
    }

    /// The first row, if any.
    pub async fn query_opt(&self, sql: &str, params: &[Value]) -> Result<Option<Row>> {
        Ok(self.query(sql, params).await?.into_iter().next())
    }

    pub async fn query_as<T: FromRow>(&self, sql: &str, params: &[Value]) -> Result<Vec<T>> {
        all(self.query(sql, params).await?)
    }

    pub async fn query_opt_as<T: FromRow>(&self, sql: &str, params: &[Value]) -> Result<Option<T>> {
        self.query_opt(sql, params)
            .await?
            .as_ref()
            .map(T::from_row)
            .transpose()
    }

    pub async fn query_one_as<T: FromRow>(&self, sql: &str, params: &[Value]) -> Result<T> {
        one(self.query_opt_as(sql, params).await?)
    }

    /// The first column of the first row, which must exist.
    pub async fn scalar<T: FromValue>(&self, sql: &str, params: &[Value]) -> Result<T> {
        one(self.query_opt(sql, params).await?)?.get(0)
    }

    /// The first column of every row.
    pub async fn scalars<T: FromValue>(&self, sql: &str, params: &[Value]) -> Result<Vec<T>> {
        self.query(sql, params)
            .await?
            .iter()
            .map(|r| r.get(0))
            .collect()
    }

    /// The first column of the first row, if there is a row.
    pub async fn scalar_opt<T: FromValue>(&self, sql: &str, params: &[Value]) -> Result<Option<T>> {
        self.query_opt(sql, params)
            .await?
            .map(|r| r.get(0))
            .transpose()
    }

    /// Start a transaction. Dropping it without [`Tx::commit`] rolls it back
    /// (on Turso, when the server expires the stream).
    pub async fn begin(&self) -> Result<Tx> {
        let inner = match &*self.backend {
            Backend::Remote(r) => {
                let (_, baton) = r.execute(None, true, "BEGIN IMMEDIATE", &[]).await?;
                TxInner::Remote {
                    db: self.clone(),
                    baton,
                }
            }
            Backend::Local { .. } => {
                let conn = self.local_conn().await?.unwrap();
                conn.execute("BEGIN IMMEDIATE", ()).await?;
                TxInner::Local(conn)
            }
        };
        Ok(Tx { inner })
    }

    /// `SELECT 1`.
    pub async fn healthcheck(&self) -> Result<()> {
        self.query("SELECT 1", &[]).await.map(|_| ())
    }
}

enum TxInner {
    Remote { db: Db, baton: Option<String> },
    Local(libsql::Connection),
}

/// An open transaction.
pub struct Tx {
    inner: TxInner,
}

impl Tx {
    fn remote(&self) -> &hrana::Remote {
        match &self.inner {
            TxInner::Remote { db, .. } => match &*db.backend {
                Backend::Remote(r) => r,
                Backend::Local { .. } => unreachable!(),
            },
            TxInner::Local(_) => unreachable!(),
        }
    }

    pub async fn execute(&mut self, sql: &str, params: &[Value]) -> Result<u64> {
        Ok(self.run(sql, params).await?.affected_rows())
    }

    pub async fn query(&mut self, sql: &str, params: &[Value]) -> Result<Vec<Row>> {
        Ok(self.run(sql, params).await?.rows())
    }

    pub async fn query_opt(&mut self, sql: &str, params: &[Value]) -> Result<Option<Row>> {
        Ok(self.query(sql, params).await?.into_iter().next())
    }

    pub async fn query_as<T: FromRow>(&mut self, sql: &str, params: &[Value]) -> Result<Vec<T>> {
        all(self.query(sql, params).await?)
    }

    pub async fn query_opt_as<T: FromRow>(
        &mut self,
        sql: &str,
        params: &[Value],
    ) -> Result<Option<T>> {
        self.query_opt(sql, params)
            .await?
            .as_ref()
            .map(T::from_row)
            .transpose()
    }

    pub async fn query_one_as<T: FromRow>(&mut self, sql: &str, params: &[Value]) -> Result<T> {
        one(self.query_opt_as(sql, params).await?)
    }

    pub async fn scalars<T: FromValue>(&mut self, sql: &str, params: &[Value]) -> Result<Vec<T>> {
        self.query(sql, params)
            .await?
            .iter()
            .map(|r| r.get(0))
            .collect()
    }

    pub async fn scalar<T: FromValue>(&mut self, sql: &str, params: &[Value]) -> Result<T> {
        one(self.query_opt(sql, params).await?)?.get(0)
    }

    pub async fn scalar_opt<T: FromValue>(
        &mut self,
        sql: &str,
        params: &[Value],
    ) -> Result<Option<T>> {
        self.query_opt(sql, params)
            .await?
            .map(|r| r.get(0))
            .transpose()
    }

    /// Several `;`-separated statements, no parameters.
    pub async fn execute_batch(&mut self, sql: &str) -> Result<()> {
        match &self.inner {
            TxInner::Local(c) => {
                c.execute_batch(sql).await?;
            }
            TxInner::Remote { baton, .. } => {
                let next = self.remote().sequence(baton.as_deref(), true, sql).await?;
                if let TxInner::Remote { baton, .. } = &mut self.inner {
                    *baton = next;
                }
            }
        }
        Ok(())
    }

    async fn run(&mut self, sql: &str, params: &[Value]) -> Result<Ran> {
        match &self.inner {
            TxInner::Local(c) => {
                if returns_rows(sql) {
                    Ok(Ran::Rows(local_query(c, sql, params).await?))
                } else {
                    Ok(Ran::Affected(local_execute(c, sql, params).await?))
                }
            }
            TxInner::Remote { baton, .. } => {
                let (done, next) = self
                    .remote()
                    .execute(baton.as_deref(), true, sql, params)
                    .await?;
                if let TxInner::Remote { baton, .. } = &mut self.inner {
                    *baton = next;
                }
                Ok(Ran::Remote(done))
            }
        }
    }

    pub async fn commit(self) -> Result<()> {
        self.finish("COMMIT").await
    }

    pub async fn rollback(self) -> Result<()> {
        self.finish("ROLLBACK").await
    }

    async fn finish(self, verb: &str) -> Result<()> {
        match &self.inner {
            TxInner::Local(c) => {
                c.execute(verb, ()).await?;
            }
            TxInner::Remote { baton, .. } => {
                let (_, next) = self
                    .remote()
                    .execute(baton.as_deref(), true, verb, &[])
                    .await?;
                if let Some(b) = next {
                    self.remote().close(&b).await?;
                }
            }
        }
        Ok(())
    }
}

enum Ran {
    Rows(Vec<Row>),
    Affected(u64),
    Remote(hrana::Executed),
}
impl Ran {
    fn rows(self) -> Vec<Row> {
        match self {
            Ran::Rows(r) => r,
            Ran::Affected(_) => vec![],
            Ran::Remote(e) => rows_of(e),
        }
    }
    fn affected_rows(self) -> u64 {
        match self {
            Ran::Affected(n) => n,
            Ran::Rows(r) => r.len() as u64,
            Ran::Remote(e) => e.affected,
        }
    }
}

fn rows_of(e: hrana::Executed) -> Vec<Row> {
    let cols = Arc::new(e.cols);
    e.rows
        .into_iter()
        .map(|vals| Row {
            cols: cols.clone(),
            vals,
        })
        .collect()
}

/// A local statement either returns rows or it does not; libSQL wants to know.
fn returns_rows(sql: &str) -> bool {
    let s = sql.trim_start().to_ascii_uppercase();
    s.starts_with("SELECT")
        || s.starts_with("WITH")
        || s.starts_with("PRAGMA")
        || s.contains(" RETURNING ")
        || s.contains("\nRETURNING ")
}

fn to_local(v: &Value) -> libsql::Value {
    match v {
        Value::Null => libsql::Value::Null,
        Value::Integer(i) => libsql::Value::Integer(*i),
        Value::Real(f) => libsql::Value::Real(*f),
        Value::Text(s) => libsql::Value::Text(s.clone()),
        Value::Blob(b) => libsql::Value::Blob(b.clone()),
    }
}

fn from_local(v: libsql::Value) -> Value {
    match v {
        libsql::Value::Null => Value::Null,
        libsql::Value::Integer(i) => Value::Integer(i),
        libsql::Value::Real(f) => Value::Real(f),
        libsql::Value::Text(s) => Value::Text(s),
        libsql::Value::Blob(b) => Value::Blob(b),
    }
}

async fn local_execute(c: &libsql::Connection, sql: &str, params: &[Value]) -> Result<u64> {
    if returns_rows(sql) {
        return Ok(local_query(c, sql, params).await?.len() as u64);
    }
    Ok(
        c.execute(sql, params.iter().map(to_local).collect::<Vec<_>>())
            .await?,
    )
}

async fn local_query(c: &libsql::Connection, sql: &str, params: &[Value]) -> Result<Vec<Row>> {
    let mut rows = c
        .query(sql, params.iter().map(to_local).collect::<Vec<_>>())
        .await?;
    let cols: Arc<Vec<String>> = Arc::new(
        (0..rows.column_count())
            .map(|i| rows.column_name(i).unwrap_or("").to_owned())
            .collect(),
    );
    let mut out = Vec::new();
    while let Some(r) = rows.next().await? {
        let vals = (0..cols.len() as i32)
            .map(|i| r.get_value(i).map(from_local))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        out.push(Row {
            cols: cols.clone(),
            vals,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
