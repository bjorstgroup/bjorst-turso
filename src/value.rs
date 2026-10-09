//! The values a statement takes and a row gives back.

use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

macro_rules! from_int {
    ($($t:ty),*) => {$(impl From<$t> for Value { fn from(v: $t) -> Self { Value::Integer(v as i64) } })*};
}
from_int!(i8, i16, i32, i64, u8, u16, u32);

impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Integer(v as i64)
    }
}
impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Real(v)
    }
}
impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::Text(v.to_owned())
    }
}
impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Text(v)
    }
}
impl From<Vec<u8>> for Value {
    fn from(v: Vec<u8>) -> Self {
        Value::Blob(v)
    }
}
impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Self {
        v.map_or(Value::Null, Into::into)
    }
}

/// Build a parameter list: `params![id, "name", None::<i64>]`. Each value is
/// cloned, so `&dto.name` and `dto.name` both work.
#[macro_export]
macro_rules! params {
    ($($v:expr),* $(,)?) => { [$($crate::Value::from(($v).clone())),*] };
}

/// Read a column as a Rust type.
pub trait FromValue: Sized {
    fn from_value(v: &Value) -> Result<Self>;
}

fn mismatch<T>(want: &str, got: &Value) -> Result<T> {
    Err(Error::Decode(format!("expected {want}, got {got:?}")))
}

impl FromValue for i64 {
    fn from_value(v: &Value) -> Result<Self> {
        match v {
            Value::Integer(i) => Ok(*i),
            other => mismatch("integer", other),
        }
    }
}
impl FromValue for i32 {
    fn from_value(v: &Value) -> Result<Self> {
        i32::try_from(i64::from_value(v)?).map_err(|e| Error::Decode(e.to_string()))
    }
}
impl FromValue for bool {
    fn from_value(v: &Value) -> Result<Self> {
        Ok(i64::from_value(v)? != 0)
    }
}
impl FromValue for f64 {
    fn from_value(v: &Value) -> Result<Self> {
        match v {
            Value::Real(f) => Ok(*f),
            Value::Integer(i) => Ok(*i as f64),
            other => mismatch("real", other),
        }
    }
}
impl FromValue for String {
    fn from_value(v: &Value) -> Result<Self> {
        match v {
            Value::Text(s) => Ok(s.clone()),
            other => mismatch("text", other),
        }
    }
}
impl FromValue for Vec<u8> {
    fn from_value(v: &Value) -> Result<Self> {
        match v {
            Value::Blob(b) => Ok(b.clone()),
            other => mismatch("blob", other),
        }
    }
}
impl<T: FromValue> FromValue for Option<T> {
    fn from_value(v: &Value) -> Result<Self> {
        match v {
            Value::Null => Ok(None),
            other => T::from_value(other).map(Some),
        }
    }
}

/// `DateTime<Utc>` is `YYYY-MM-DDTHH:MM:SS.mmmZ` text, which sorts correctly;
/// `NaiveDate` is `YYYY-MM-DD`. Reading also accepts SQLite's own
/// `YYYY-MM-DD HH:MM:SS`.
#[cfg(feature = "chrono")]
mod chrono_impls {
    use super::*;
    use chrono::{DateTime, NaiveDate, NaiveDateTime, SecondsFormat, Utc};

    impl From<DateTime<Utc>> for Value {
        fn from(v: DateTime<Utc>) -> Self {
            Value::Text(v.to_rfc3339_opts(SecondsFormat::Millis, true))
        }
    }
    impl From<NaiveDate> for Value {
        fn from(v: NaiveDate) -> Self {
            Value::Text(v.format("%Y-%m-%d").to_string())
        }
    }
    impl FromValue for DateTime<Utc> {
        fn from_value(v: &Value) -> Result<Self> {
            let s = String::from_value(v)?;
            if let Ok(t) = DateTime::parse_from_rfc3339(&s) {
                return Ok(t.with_timezone(&Utc));
            }
            NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S%.f")
                .map(|n| n.and_utc())
                .map_err(|e| Error::Decode(format!("timestamp {s:?}: {e}")))
        }
    }
    impl FromValue for NaiveDate {
        fn from_value(v: &Value) -> Result<Self> {
            let s = String::from_value(v)?;
            NaiveDate::parse_from_str(&s, "%Y-%m-%d")
                .map_err(|e| Error::Decode(format!("date {s:?}: {e}")))
        }
    }
}

/// Decimals are text, so no precision is lost to a float. Reading also accepts
/// an integer or real column.
#[cfg(feature = "bigdecimal")]
mod bigdecimal_impls {
    use super::*;
    use bigdecimal::BigDecimal;
    use std::str::FromStr;

    impl From<BigDecimal> for Value {
        fn from(v: BigDecimal) -> Self {
            Value::Text(v.normalized().to_string())
        }
    }
    impl From<&BigDecimal> for Value {
        fn from(v: &BigDecimal) -> Self {
            Value::Text(v.normalized().to_string())
        }
    }
    impl FromValue for BigDecimal {
        fn from_value(v: &Value) -> Result<Self> {
            let text = match v {
                Value::Text(s) => s.clone(),
                Value::Integer(i) => i.to_string(),
                Value::Real(f) => f.to_string(),
                other => return Err(Error::Decode(format!("expected decimal, got {other:?}"))),
            };
            BigDecimal::from_str(&text).map_err(|e| Error::Decode(format!("decimal {text:?}: {e}")))
        }
    }
}
impl FromValue for Value {
    fn from_value(v: &Value) -> Result<Self> {
        Ok(v.clone())
    }
}

/// JSON columns are text.
#[cfg(feature = "json")]
mod json_impls {
    use super::*;

    impl From<serde_json::Value> for Value {
        fn from(v: serde_json::Value) -> Self {
            Value::Text(v.to_string())
        }
    }
    impl FromValue for serde_json::Value {
        fn from_value(v: &Value) -> Result<Self> {
            let s = String::from_value(v)?;
            serde_json::from_str(&s).map_err(|e| Error::Decode(format!("json: {e}")))
        }
    }
}
