# bjorst-turso

Turso (libSQL) connection builder and a forward-only migration runner.

```rust
use bjorst_turso::{migrate, Db, Migration, TursoConfig};

let db = Db::connect(&TursoConfig {
    url: std::env::var("TURSO_DATABASE_URL")?,      // libsql://… or file:app.db
    auth_token: std::env::var("TURSO_AUTH_TOKEN").ok(),
}).await?;

migrate(&db, &[Migration { version: 1, name: "init", sql: include_str!("../sql/1.sql") }]).await?;
let rows = db.connection()?.query("SELECT 1", ()).await?;
```

- A `libsql://` / `https://` url needs a token; `file:` and `:memory:` are local (tests).
- `:memory:` shares one connection, since each in-memory connection is otherwise its own database.
- Each migration runs in one transaction. Its SQL is checksummed into `_migrations`, and editing it afterwards is an error.
- Dates are stored as ISO-8601 text, ids as text, money as integer minor units. SQLite has no uuid, timestamptz or numeric type.
