use super::*;
use crate::migrate::Migration;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A fresh file database: `:memory:` cannot show rollback.
async fn file_db() -> Db {
    let p = std::env::temp_dir().join(format!(
        "bjorst-turso-{}-{:?}.db",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_file(&p);
    Db::connect(&TursoConfig {
        url: format!("file:{}", p.display()),
        auth_token: None,
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn values_round_trip_and_nulls_read_as_none() {
    let db = file_db().await;
    db.execute(
        "CREATE TABLE t (i INTEGER, r REAL, s TEXT, b BLOB, n TEXT)",
        &[],
    )
    .await
    .unwrap();
    db.execute(
        "INSERT INTO t VALUES (?1,?2,?3,?4,?5)",
        &params![7_i64, 1.5, "hé", vec![1_u8, 2], None::<String>],
    )
    .await
    .unwrap();
    let r = db.query_opt("SELECT * FROM t", &[]).await.unwrap().unwrap();
    assert_eq!(r.get::<i64>("i").unwrap(), 7);
    assert_eq!(r.get::<f64>("r").unwrap(), 1.5);
    assert_eq!(r.get::<String>("s").unwrap(), "hé");
    assert_eq!(r.get::<Vec<u8>>(3).unwrap(), vec![1, 2]);
    assert_eq!(r.get::<Option<String>>("n").unwrap(), None);
    assert!(r.get::<i64>("s").is_err());
}

#[tokio::test]
async fn a_transaction_commits_or_rolls_back() {
    let db = file_db().await;
    db.execute("CREATE TABLE t (x INTEGER)", &[]).await.unwrap();
    let mut tx = db.begin().await.unwrap();
    tx.execute("INSERT INTO t VALUES (1)", &[]).await.unwrap();
    tx.rollback().await.unwrap();
    let mut tx = db.begin().await.unwrap();
    tx.execute("INSERT INTO t VALUES (2)", &[]).await.unwrap();
    assert_eq!(tx.query("SELECT x FROM t", &[]).await.unwrap().len(), 1);
    tx.commit().await.unwrap();
    let rows = db.query("SELECT x FROM t", &[]).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<i64>(0).unwrap(), 2);
}

#[tokio::test]
async fn returning_gives_rows_inside_and_outside_a_transaction() {
    let db = file_db().await;
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x TEXT)", &[])
        .await
        .unwrap();
    let rows = db
        .query("INSERT INTO t (x) VALUES ('a') RETURNING id", &[])
        .await
        .unwrap();
    assert_eq!(rows[0].get::<i64>(0).unwrap(), 1);
    let mut tx = db.begin().await.unwrap();
    let rows = tx
        .query("INSERT INTO t (x) VALUES ('b') RETURNING id", &[])
        .await
        .unwrap();
    assert_eq!(rows[0].get::<i64>("id").unwrap(), 2);
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn a_remote_url_without_a_token_is_refused() {
    let cfg = TursoConfig {
        url: "libsql://x-y.turso.io".into(),
        auth_token: None,
    };
    assert!(matches!(
        Db::connect(&cfg).await,
        Err(Error::MissingToken(_))
    ));
}

#[test]
fn debug_does_not_print_the_token() {
    let cfg = TursoConfig {
        url: "libsql://x".into(),
        auth_token: Some("secret-token".into()),
    };
    assert!(!format!("{cfg:?}").contains("secret-token"));
}

fn ok_result(extra: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "baton": null, "results": [ { "type": "ok", "response": { "type": "execute", "result": extra } }, { "type": "ok", "response": { "type": "close" } } ] })
}

#[tokio::test]
async fn a_remote_query_speaks_hrana_and_reads_typed_values() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/pipeline"))
        .and(header("authorization", "Bearer tok"))
        .and(body_partial_json(serde_json::json!({
            "requests": [ { "type": "execute", "stmt": { "sql": "SELECT ?1", "args": [ { "type": "integer", "value": "5" } ] } }, { "type": "close" } ]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_result(serde_json::json!({
            "cols": [ { "name": "a" }, { "name": "b" }, { "name": "c" } ],
            "rows": [ [ { "type": "integer", "value": "9007199254740993" }, { "type": "text", "value": "hi" }, { "type": "null" } ] ],
            "affected_row_count": 0
        }))))
        .expect(1)
        .mount(&server)
        .await;
    let db = Db::connect(&TursoConfig {
        url: server.uri(),
        auth_token: Some("tok".into()),
    })
    .await
    .unwrap();
    let r = db
        .query_opt("SELECT ?1", &params![5])
        .await
        .unwrap()
        .unwrap();
    // Beyond f64's exact integers: Hrana sends integers as strings for this.
    assert_eq!(r.get::<i64>("a").unwrap(), 9_007_199_254_740_993);
    assert_eq!(r.get::<String>("b").unwrap(), "hi");
    assert_eq!(r.get::<Option<i64>>("c").unwrap(), None);
}

#[tokio::test]
async fn a_remote_sql_error_carries_the_servers_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "baton": null, "results": [ { "type": "error", "error": { "message": "no such table: x", "code": "SQLITE_ERROR" } } ]
        })))
        .mount(&server)
        .await;
    let db = Db::connect(&TursoConfig {
        url: server.uri(),
        auth_token: Some("t".into()),
    })
    .await
    .unwrap();
    let e = db.query("SELECT * FROM x", &[]).await.unwrap_err();
    assert!(e.to_string().contains("no such table: x"), "{e}");
}

#[tokio::test]
async fn migrations_apply_once_refuse_edits_and_roll_back_failures() {
    const ONE: Migration = Migration {
        version: 1,
        name: "t",
        sql: "CREATE TABLE t (x INTEGER);",
    };
    let db = file_db().await;
    migrate(&db, &[ONE]).await.unwrap();
    migrate(&db, &[ONE]).await.unwrap();
    let n = db
        .query_opt("SELECT count(*) FROM _migrations", &[])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(n.get::<i64>(0).unwrap(), 1);
    let edited = Migration {
        version: 1,
        name: "t",
        sql: "CREATE TABLE t (y INTEGER);",
    };
    assert!(matches!(
        migrate(&db, &[edited]).await,
        Err(Error::ChecksumMismatch { version: 1 })
    ));

    let db = file_db().await;
    let bad = Migration {
        version: 1,
        name: "bad",
        sql: "CREATE TABLE a (x); NOT SQL;",
    };
    assert!(migrate(&db, &[bad]).await.is_err());
    let n = db
        .query_opt("SELECT count(*) FROM sqlite_master WHERE name='a'", &[])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(n.get::<i64>(0).unwrap(), 0);
}

/// Against a real Turso database, when `TURSO_TEST_URL` and `TURSO_TEST_TOKEN`
/// are set. Uses a temp table so nothing is left behind.
#[tokio::test]
async fn live_turso_round_trip() {
    let (Ok(url), Ok(token)) = (
        std::env::var("TURSO_TEST_URL"),
        std::env::var("TURSO_TEST_TOKEN"),
    ) else {
        return;
    };
    let db = Db::connect(&TursoConfig {
        url,
        auth_token: Some(token),
    })
    .await
    .unwrap();
    db.healthcheck().await.unwrap();
    let mut tx = db.begin().await.unwrap();
    tx.execute(
        "CREATE TABLE IF NOT EXISTS _bjorst_turso_probe (id INTEGER PRIMARY KEY, s TEXT)",
        &[],
    )
    .await
    .unwrap();
    let id = tx
        .query(
            "INSERT INTO _bjorst_turso_probe (s) VALUES (?1) RETURNING id",
            &params!["é"],
        )
        .await
        .unwrap()[0]
        .get::<i64>(0)
        .unwrap();
    assert!(id > 0);
    tx.rollback().await.unwrap();
    let gone = db
        .query(
            "SELECT name FROM sqlite_master WHERE name='_bjorst_turso_probe'",
            &[],
        )
        .await
        .unwrap();
    assert!(
        gone.is_empty(),
        "rollback should have removed the probe table"
    );
}

#[cfg(all(feature = "chrono", feature = "bigdecimal"))]
#[tokio::test]
async fn dates_and_decimals_round_trip_without_loss() {
    use bigdecimal::BigDecimal;
    use chrono::{DateTime, NaiveDate, TimeZone, Utc};
    use std::str::FromStr;
    let db = file_db().await;
    db.execute("CREATE TABLE t (at TEXT, d TEXT, q TEXT, def TEXT DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')), sq TEXT DEFAULT (datetime('now')))", &[]).await.unwrap();
    let at = Utc.with_ymd_and_hms(2026, 10, 9, 7, 8, 9).unwrap();
    let d = NaiveDate::from_ymd_opt(2026, 2, 28).unwrap();
    let q = BigDecimal::from_str("0.0001234567891").unwrap();
    db.execute(
        "INSERT INTO t (at, d, q) VALUES (?1,?2,?3)",
        &params![at, d, &q],
    )
    .await
    .unwrap();
    let r = db.query_opt("SELECT * FROM t", &[]).await.unwrap().unwrap();
    assert_eq!(r.get::<DateTime<Utc>>("at").unwrap(), at);
    assert_eq!(r.get::<NaiveDate>("d").unwrap(), d);
    assert_eq!(r.get::<BigDecimal>("q").unwrap(), q);
    r.get::<DateTime<Utc>>("def").unwrap();
    r.get::<DateTime<Utc>>("sq").unwrap();
}

row_struct! {
    #[derive(Debug, PartialEq)]
    struct Person { id: i64, name: String, nick: Option<String> }
}

#[tokio::test]
async fn rows_become_structs_and_scalars() {
    let db = file_db().await;
    db.execute(
        "CREATE TABLE p (id INTEGER PRIMARY KEY, name TEXT NOT NULL, nick TEXT)",
        &[],
    )
    .await
    .unwrap();
    db.execute("INSERT INTO p (name) VALUES ('a'), ('b')", &[])
        .await
        .unwrap();
    let all: Vec<Person> = db
        .query_as("SELECT * FROM p ORDER BY id", &[])
        .await
        .unwrap();
    assert_eq!(
        all[1],
        Person {
            id: 2,
            name: "b".into(),
            nick: None
        }
    );
    assert!(db
        .query_opt_as::<Person>("SELECT * FROM p WHERE id = ?1", &params![9])
        .await
        .unwrap()
        .is_none());
    assert!(db
        .query_one_as::<Person>("SELECT * FROM p WHERE id = ?1", &params![9])
        .await
        .is_err());
    assert_eq!(
        db.scalar::<i64>("SELECT count(*) FROM p", &[])
            .await
            .unwrap(),
        2
    );
    let mut tx = db.begin().await.unwrap();
    let id: i64 = tx
        .scalar("INSERT INTO p (name) VALUES ('c') RETURNING id", &[])
        .await
        .unwrap();
    assert_eq!(id, 3);
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn foreign_keys_are_enforced_locally_as_they_are_on_turso() {
    let db = file_db().await;
    db.execute("CREATE TABLE a (id INTEGER PRIMARY KEY)", &[])
        .await
        .unwrap();
    db.execute("CREATE TABLE b (a_id INTEGER REFERENCES a(id))", &[])
        .await
        .unwrap();
    assert!(db.execute("INSERT INTO b VALUES (99)", &[]).await.is_err());
}

#[test]
fn params_take_owned_values_and_references() {
    struct Dto {
        name: String,
        note: Option<String>,
    }
    let dto = Dto {
        name: "n".into(),
        note: None,
    };
    let p = params![&dto.name, &dto.note, dto.name, "lit", 5_i64, &5_i64, true];
    assert_eq!(p[0], Value::Text("n".into()));
    assert_eq!(p[1], Value::Null);
    assert_eq!(p[3], Value::Text("lit".into()));
    assert_eq!(p[5], Value::Integer(5));
    assert_eq!(p[6], Value::Integer(1));
}

#[tokio::test]
async fn tuples_scalars_and_json() {
    let db = file_db().await;
    db.execute("CREATE TABLE p (a INTEGER, b TEXT, j TEXT)", &[])
        .await
        .unwrap();
    db.execute(
        "INSERT INTO p VALUES (1, 'x', ?1), (2, 'y', NULL)",
        &params![serde_json::json!({"k": [1]})],
    )
    .await
    .unwrap();
    let t: Vec<(i64, String)> = db
        .query_as("SELECT a, b FROM p ORDER BY a", &[])
        .await
        .unwrap();
    assert_eq!(t, vec![(1, "x".to_string()), (2, "y".to_string())]);
    assert_eq!(
        db.scalars::<i64>("SELECT a FROM p ORDER BY a", &[])
            .await
            .unwrap(),
        vec![1, 2]
    );
    let j: Option<serde_json::Value> = db.scalar("SELECT j FROM p WHERE a = 1", &[]).await.unwrap();
    assert_eq!(j.unwrap()["k"][0], 1);
}

#[tokio::test]
async fn two_migrators_at_once_apply_it_once() {
    const ONE: Migration = Migration {
        version: 1,
        name: "t",
        sql: "CREATE TABLE t (x INTEGER);",
    };
    let db = file_db().await;
    let (a, b) = tokio::join!(migrate(&db, &[ONE]), migrate(&db, &[ONE]));
    a.unwrap();
    b.unwrap();
}
