//! Turso over HTTP: Hrana's `/v2/pipeline`.
//!
//! One `reqwest` client, so TLS is whatever `reqwest` ships, not libsql's.
//! A transaction keeps its `baton` between calls; the server rolls a stream
//! back when it expires, so a dropped `Tx` leaves nothing behind.

use base64::{engine::general_purpose::STANDARD_NO_PAD, Engine};
use serde_json::{json, Value as Json};

use crate::{Error, Result, Value};

pub struct Remote {
    client: reqwest::Client,
    endpoint: String,
    token: String,
}

/// What an `execute` answers.
pub struct Executed {
    pub cols: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    pub affected: u64,
}

impl Remote {
    pub fn new(url: &str, token: String) -> Self {
        let base = url
            .replacen("libsql://", "https://", 1)
            .trim_end_matches('/')
            .to_owned();
        Self {
            client: reqwest::Client::new(),
            endpoint: format!("{base}/v2/pipeline"),
            token,
        }
    }

    /// Run `requests` on the stream `baton` names (or a new one), answering
    /// each request's result and the stream's next baton.
    async fn pipeline(
        &self,
        baton: Option<&str>,
        requests: Vec<Json>,
    ) -> Result<(Vec<Json>, Option<String>)> {
        let res = self
            .client
            .post(&self.endpoint)
            .bearer_auth(&self.token)
            .json(&json!({ "baton": baton, "requests": requests }))
            .send()
            .await?;
        let status = res.status();
        let body: Json = res.json().await.map_err(|e| {
            Error::Decode(format!(
                "turso answered {status} with an unreadable body: {e}"
            ))
        })?;
        if !status.is_success() {
            let msg = body["error"]
                .as_str()
                .or(body["message"].as_str())
                .unwrap_or("no message");
            return Err(Error::Sql {
                code: Some(status.as_u16().to_string()),
                message: msg.to_owned(),
            });
        }
        let next = body["baton"].as_str().map(str::to_owned);
        let results = body["results"].as_array().cloned().unwrap_or_default();
        for r in &results {
            if r["type"] == "error" {
                return Err(Error::Sql {
                    code: r["error"]["code"].as_str().map(str::to_owned),
                    message: r["error"]["message"].as_str().unwrap_or("").to_owned(),
                });
            }
        }
        Ok((results, next))
    }

    /// Execute one statement. `baton` is `None` for a one-shot (the stream is
    /// closed in the same request) and `Some` inside a transaction.
    pub async fn execute(
        &self,
        baton: Option<&str>,
        keep_open: bool,
        sql: &str,
        args: &[Value],
    ) -> Result<(Executed, Option<String>)> {
        let mut reqs = vec![json!({
            "type": "execute",
            "stmt": { "sql": sql, "args": args.iter().map(arg).collect::<Vec<_>>(), "want_rows": true }
        })];
        if !keep_open {
            reqs.push(json!({ "type": "close" }));
        }
        let (results, next) = self.pipeline(baton, reqs).await?;
        let r = &results
            .first()
            .ok_or_else(|| Error::Decode("no result".into()))?["response"]["result"];
        let cols = r["cols"]
            .as_array()
            .map(|c| {
                c.iter()
                    .map(|c| c["name"].as_str().unwrap_or("").to_owned())
                    .collect()
            })
            .unwrap_or_default();
        let rows = r["rows"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .map(|row| {
                        row.as_array()
                            .map_or_else(Vec::new, |r| r.iter().map(value).collect())
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok((
            Executed {
                cols,
                rows,
                affected: r["affected_row_count"].as_u64().unwrap_or(0),
            },
            if keep_open { next } else { None },
        ))
    }

    /// Run several statements (`;`-separated) on a stream.
    pub async fn sequence(
        &self,
        baton: Option<&str>,
        keep_open: bool,
        sql: &str,
    ) -> Result<Option<String>> {
        let mut reqs = vec![json!({ "type": "sequence", "sql": sql })];
        if !keep_open {
            reqs.push(json!({ "type": "close" }));
        }
        let (_, next) = self.pipeline(baton, reqs).await?;
        Ok(if keep_open { next } else { None })
    }

    pub async fn close(&self, baton: &str) -> Result<()> {
        self.pipeline(Some(baton), vec![json!({ "type": "close" })])
            .await
            .map(|_| ())
    }
}

fn arg(v: &Value) -> Json {
    match v {
        Value::Null => json!({ "type": "null" }),
        Value::Integer(i) => json!({ "type": "integer", "value": i.to_string() }),
        Value::Real(f) => json!({ "type": "float", "value": f }),
        Value::Text(s) => json!({ "type": "text", "value": s }),
        Value::Blob(b) => json!({ "type": "blob", "base64": STANDARD_NO_PAD.encode(b) }),
    }
}

fn value(v: &Json) -> Value {
    match v["type"].as_str() {
        Some("integer") => Value::Integer(
            v["value"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
        ),
        Some("float") => Value::Real(v["value"].as_f64().unwrap_or(0.0)),
        Some("text") => Value::Text(v["value"].as_str().unwrap_or("").to_owned()),
        Some("blob") => Value::Blob(
            STANDARD_NO_PAD
                .decode(v["base64"].as_str().unwrap_or("").trim_end_matches('='))
                .unwrap_or_default(),
        ),
        _ => Value::Null,
    }
}
