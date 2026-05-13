use crate::value::Value;
use std::cmp::Ordering;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone)]
pub struct Filter {
    pub column: String,
    pub op: FilterOp,
    pub value: Value,
}

impl Filter {
    pub fn eq(column: impl Into<String>, value: impl Into<Value>) -> Self {
        Self { column: column.into(), op: FilterOp::Eq, value: value.into() }
    }
    pub fn ne(column: impl Into<String>, value: impl Into<Value>) -> Self {
        Self { column: column.into(), op: FilterOp::Ne, value: value.into() }
    }
    pub fn lt(column: impl Into<String>, value: impl Into<Value>) -> Self {
        Self { column: column.into(), op: FilterOp::Lt, value: value.into() }
    }
    pub fn le(column: impl Into<String>, value: impl Into<Value>) -> Self {
        Self { column: column.into(), op: FilterOp::Le, value: value.into() }
    }
    pub fn gt(column: impl Into<String>, value: impl Into<Value>) -> Self {
        Self { column: column.into(), op: FilterOp::Gt, value: value.into() }
    }
    pub fn ge(column: impl Into<String>, value: impl Into<Value>) -> Self {
        Self { column: column.into(), op: FilterOp::Ge, value: value.into() }
    }

    /// Evaluate this filter against a value.
    pub fn matches(&self, actual: &Value) -> bool {
        let cmp = actual.compare(&self.value);
        match cmp {
            Some(ord) => match self.op {
                FilterOp::Eq => ord == Ordering::Equal,
                FilterOp::Ne => ord != Ordering::Equal,
                FilterOp::Lt => ord == Ordering::Less,
                FilterOp::Le => ord != Ordering::Greater,
                FilterOp::Gt => ord == Ordering::Greater,
                FilterOp::Ge => ord != Ordering::Less,
            },
            None => false, // incompatible types don't match
        }
    }
}

/// Evaluate a filter op directly without a Filter struct. Used by B+ tree scan_filtered.
pub fn eval_filter_op(actual: &Value, op: &FilterOp, expected: &Value) -> bool {
    match actual.compare(expected) {
        Some(ord) => match op {
            FilterOp::Eq => ord == Ordering::Equal,
            FilterOp::Ne => ord != Ordering::Equal,
            FilterOp::Lt => ord == Ordering::Less,
            FilterOp::Le => ord != Ordering::Greater,
            FilterOp::Gt => ord == Ordering::Greater,
            FilterOp::Ge => ord != Ordering::Less,
        },
        None => false,
    }
}

// Convenience Into<Value> impls
impl From<&str> for Value {
    fn from(s: &str) -> Self { Value::Text(s.to_string()) }
}
impl From<String> for Value {
    fn from(s: String) -> Self { Value::Text(s) }
}
impl From<i64> for Value {
    fn from(i: i64) -> Self { Value::Integer(i) }
}
impl From<f64> for Value {
    fn from(f: f64) -> Self { Value::Real(f) }
}
impl From<bool> for Value {
    fn from(b: bool) -> Self { Value::Boolean(b) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDir {
    Asc,
    Desc,
}

#[derive(Debug, Clone)]
pub struct Sort {
    pub column: String,
    pub dir: SortDir,
}

impl Sort {
    pub fn asc(column: impl Into<String>) -> Self {
        Self { column: column.into(), dir: SortDir::Asc }
    }
    pub fn desc(column: impl Into<String>) -> Self {
        Self { column: column.into(), dir: SortDir::Desc }
    }
}

#[derive(Debug, Clone, Default)]
pub struct FindOptions {
    pub filters: Vec<Filter>,
    pub sort: Vec<Sort>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
    pub include_total: bool,
}

/// Result of a find() query.
#[derive(Debug, Clone)]
pub struct FindResult {
    pub rows: Vec<crate::db::Row>,
    /// Only populated when FindOptions.include_total is true.
    pub total: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_eq() {
        let f = Filter::eq("name", "alice");
        assert!(f.matches(&Value::Text("alice".into())));
        assert!(!f.matches(&Value::Text("bob".into())));
    }

    #[test]
    fn test_filter_gt_integer() {
        let f = Filter::gt("age", 18i64);
        assert!(f.matches(&Value::Integer(21)));
        assert!(!f.matches(&Value::Integer(18)));
        assert!(!f.matches(&Value::Integer(10)));
    }

    #[test]
    fn test_filter_null() {
        let f = Filter::eq("x", Value::Null);
        assert!(f.matches(&Value::Null));
        assert!(!f.matches(&Value::Integer(0)));
    }
}
