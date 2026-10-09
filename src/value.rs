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

/// Build a parameter list: `params![id, "name", None::<i64>]`.
#[macro_export]
macro_rules! params {
    ($($v:expr),* $(,)?) => { [$($crate::Value::from($v)),*] };
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
