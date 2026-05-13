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

/// Evaluate a filter against raw column bytes (type_tag + value_bytes).
/// Avoids decoding/allocating a Value on the hot path.
/// Returns None if comparison can't be done in raw mode (falls back to decode).
pub fn eval_filter_raw(raw: &[u8], op: &FilterOp, expected: &Value) -> Option<bool> {
    if raw.is_empty() {
        return None;
    }
    let tag = raw[0];
    match (tag, expected, op) {
        // Text Eq: compare raw UTF-8 bytes directly — no String allocation
        (1, Value::Text(s), FilterOp::Eq) => {
            if raw.len() < 5 { return None; }
            let len = u32::from_le_bytes(raw[1..5].try_into().unwrap()) as usize;
            let expected_bytes = s.as_bytes();
            if len != expected_bytes.len() {
                Some(false)
            } else if raw.len() < 5 + len {
                None
            } else {
                Some(&raw[5..5 + len] == expected_bytes)
            }
        }
        // Integer Eq: compare i64 directly — no allocation
        (2, Value::Integer(expected_i), FilterOp::Eq) => {
            if raw.len() < 9 { return None; }
            let actual_i = i64::from_le_bytes(raw[1..9].try_into().unwrap());
            Some(actual_i == *expected_i)
        }
        // Integer comparisons
        (2, Value::Integer(expected_i), _) => {
            if raw.len() < 9 { return None; }
            let actual_i = i64::from_le_bytes(raw[1..9].try_into().unwrap());
            let ord = actual_i.cmp(expected_i);
            Some(match op {
                FilterOp::Eq => ord == std::cmp::Ordering::Equal,
                FilterOp::Ne => ord != std::cmp::Ordering::Equal,
                FilterOp::Lt => ord == std::cmp::Ordering::Less,
                FilterOp::Le => ord != std::cmp::Ordering::Greater,
                FilterOp::Gt => ord == std::cmp::Ordering::Greater,
                FilterOp::Ge => ord != std::cmp::Ordering::Less,
            })
        }
        _ => None, // fall back to decode path
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
