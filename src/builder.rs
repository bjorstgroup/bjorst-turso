//! A query assembled piece by piece, for filters that may or may not be there.

use crate::Value;

/// SQL text and its parameters, kept in step: [`push_bind`](Self::push_bind)
/// appends `?N` to the text and the value to the list.
///
/// ```
/// use bjorst_turso::QueryBuilder;
/// let mut qb = QueryBuilder::new("SELECT * FROM t WHERE a = ");
/// qb.push_bind(1).push(" AND b IN (").push_list(&[2, 3]).push(")");
/// assert_eq!(qb.sql(), "SELECT * FROM t WHERE a = ?1 AND b IN (?2,?3)");
/// assert_eq!(qb.params().len(), 3);
/// ```
#[derive(Debug, Clone, Default)]
pub struct QueryBuilder {
    sql: String,
    params: Vec<Value>,
}

impl QueryBuilder {
    pub fn new(sql: &str) -> Self {
        Self {
            sql: sql.to_owned(),
            params: Vec::new(),
        }
    }

    pub fn push(&mut self, sql: &str) -> &mut Self {
        self.sql.push_str(sql);
        self
    }

    pub fn push_bind(&mut self, v: impl Into<Value>) -> &mut Self {
        self.params.push(v.into());
        self.sql.push_str(&format!("?{}", self.params.len()));
        self
    }

    /// `?a,?b,?c` for an `IN (…)` list. An empty list writes `NULL`, which
    /// matches nothing.
    pub fn push_list<T: Clone + Into<Value>>(&mut self, xs: &[T]) -> &mut Self {
        if xs.is_empty() {
            self.sql.push_str("NULL");
        }
        for (i, x) in xs.iter().enumerate() {
            if i > 0 {
                self.sql.push(',');
            }
            self.push_bind(x.clone());
        }
        self
    }

    pub fn sql(&self) -> &str {
        &self.sql
    }

    pub fn params(&self) -> &[Value] {
        &self.params
    }
}
