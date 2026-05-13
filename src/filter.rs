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

    // --- eval_filter_op comprehensive tests ---

    #[test]
    fn test_eval_filter_op_integer_all_ops() {
        assert!(eval_filter_op(&Value::Integer(5), &FilterOp::Eq, &Value::Integer(5)));
        assert!(!eval_filter_op(&Value::Integer(5), &FilterOp::Eq, &Value::Integer(6)));

        assert!(eval_filter_op(&Value::Integer(5), &FilterOp::Ne, &Value::Integer(6)));
        assert!(!eval_filter_op(&Value::Integer(5), &FilterOp::Ne, &Value::Integer(5)));

        assert!(eval_filter_op(&Value::Integer(3), &FilterOp::Lt, &Value::Integer(5)));
        assert!(!eval_filter_op(&Value::Integer(5), &FilterOp::Lt, &Value::Integer(5)));

        assert!(eval_filter_op(&Value::Integer(5), &FilterOp::Le, &Value::Integer(5)));
        assert!(eval_filter_op(&Value::Integer(3), &FilterOp::Le, &Value::Integer(5)));
        assert!(!eval_filter_op(&Value::Integer(6), &FilterOp::Le, &Value::Integer(5)));

        assert!(eval_filter_op(&Value::Integer(6), &FilterOp::Gt, &Value::Integer(5)));
        assert!(!eval_filter_op(&Value::Integer(5), &FilterOp::Gt, &Value::Integer(5)));

        assert!(eval_filter_op(&Value::Integer(5), &FilterOp::Ge, &Value::Integer(5)));
        assert!(eval_filter_op(&Value::Integer(6), &FilterOp::Ge, &Value::Integer(5)));
        assert!(!eval_filter_op(&Value::Integer(4), &FilterOp::Ge, &Value::Integer(5)));
    }

    #[test]
    fn test_eval_filter_op_text_all_ops() {
        let apple = Value::Text("apple".into());
        let banana = Value::Text("banana".into());

        assert!(eval_filter_op(&apple, &FilterOp::Eq, &apple));
        assert!(!eval_filter_op(&apple, &FilterOp::Eq, &banana));

        assert!(eval_filter_op(&apple, &FilterOp::Ne, &banana));
        assert!(eval_filter_op(&apple, &FilterOp::Lt, &banana));
        assert!(eval_filter_op(&apple, &FilterOp::Le, &banana));
        assert!(eval_filter_op(&apple, &FilterOp::Le, &apple));
        assert!(eval_filter_op(&banana, &FilterOp::Gt, &apple));
        assert!(eval_filter_op(&banana, &FilterOp::Ge, &apple));
        assert!(eval_filter_op(&banana, &FilterOp::Ge, &banana));
    }

    #[test]
    fn test_eval_filter_op_real() {
        assert!(eval_filter_op(&Value::Real(3.14), &FilterOp::Lt, &Value::Real(4.0)));
        assert!(eval_filter_op(&Value::Real(3.14), &FilterOp::Ge, &Value::Real(3.14)));
        assert!(!eval_filter_op(&Value::Real(3.14), &FilterOp::Gt, &Value::Real(3.14)));
    }

    #[test]
    fn test_eval_filter_op_boolean() {
        assert!(eval_filter_op(&Value::Boolean(true), &FilterOp::Eq, &Value::Boolean(true)));
        assert!(!eval_filter_op(&Value::Boolean(true), &FilterOp::Eq, &Value::Boolean(false)));
        assert!(eval_filter_op(&Value::Boolean(false), &FilterOp::Lt, &Value::Boolean(true)));
    }

    #[test]
    fn test_eval_filter_op_cross_type_returns_false() {
        // Incompatible types should not match
        assert!(!eval_filter_op(&Value::Integer(5), &FilterOp::Eq, &Value::Text("5".into())));
        assert!(!eval_filter_op(&Value::Text("true".to_string()), &FilterOp::Eq, &Value::Boolean(true)));
    }

    #[test]
    fn test_eval_filter_op_null_handling() {
        // Null < everything else
        assert!(eval_filter_op(&Value::Null, &FilterOp::Lt, &Value::Integer(0)));
        assert!(eval_filter_op(&Value::Null, &FilterOp::Eq, &Value::Null));
        assert!(!eval_filter_op(&Value::Null, &FilterOp::Gt, &Value::Integer(0)));
    }

    // --- eval_filter_raw comprehensive tests ---

    #[test]
    fn test_eval_filter_raw_integer_eq() {
        use crate::row::encode_value_to_vec;
        let raw = encode_value_to_vec(&Value::Integer(42));
        assert_eq!(eval_filter_raw(&raw, &FilterOp::Eq, &Value::Integer(42)), Some(true));
        assert_eq!(eval_filter_raw(&raw, &FilterOp::Eq, &Value::Integer(43)), Some(false));
    }

    #[test]
    fn test_eval_filter_raw_integer_all_ops() {
        use crate::row::encode_value_to_vec;
        let raw = encode_value_to_vec(&Value::Integer(10));

        assert_eq!(eval_filter_raw(&raw, &FilterOp::Lt, &Value::Integer(20)), Some(true));
        assert_eq!(eval_filter_raw(&raw, &FilterOp::Lt, &Value::Integer(10)), Some(false));
        assert_eq!(eval_filter_raw(&raw, &FilterOp::Le, &Value::Integer(10)), Some(true));
        assert_eq!(eval_filter_raw(&raw, &FilterOp::Gt, &Value::Integer(5)), Some(true));
        assert_eq!(eval_filter_raw(&raw, &FilterOp::Gt, &Value::Integer(10)), Some(false));
        assert_eq!(eval_filter_raw(&raw, &FilterOp::Ge, &Value::Integer(10)), Some(true));
        assert_eq!(eval_filter_raw(&raw, &FilterOp::Ne, &Value::Integer(10)), Some(false));
        assert_eq!(eval_filter_raw(&raw, &FilterOp::Ne, &Value::Integer(11)), Some(true));
    }

    #[test]
    fn test_eval_filter_raw_text_eq() {
        use crate::row::encode_value_to_vec;
        let raw = encode_value_to_vec(&Value::Text("hello".into()));
        assert_eq!(eval_filter_raw(&raw, &FilterOp::Eq, &Value::Text("hello".into())), Some(true));
        assert_eq!(eval_filter_raw(&raw, &FilterOp::Eq, &Value::Text("world".into())), Some(false));
    }

    #[test]
    fn test_eval_filter_raw_text_length_mismatch() {
        use crate::row::encode_value_to_vec;
        let raw = encode_value_to_vec(&Value::Text("hi".into()));
        // Different length -> fast false
        assert_eq!(eval_filter_raw(&raw, &FilterOp::Eq, &Value::Text("hello".into())), Some(false));
    }

    #[test]
    fn test_eval_filter_raw_falls_back_for_unsupported() {
        use crate::row::encode_value_to_vec;
        // Real value: eval_filter_raw doesn't handle Real, should return None
        let raw = encode_value_to_vec(&Value::Real(3.14));
        assert_eq!(eval_filter_raw(&raw, &FilterOp::Eq, &Value::Real(3.14)), None);

        // Boolean value: also not handled
        let raw = encode_value_to_vec(&Value::Boolean(true));
        assert_eq!(eval_filter_raw(&raw, &FilterOp::Eq, &Value::Boolean(true)), None);

        // Text with non-Eq op: not handled
        let raw = encode_value_to_vec(&Value::Text("hello".into()));
        assert_eq!(eval_filter_raw(&raw, &FilterOp::Lt, &Value::Text("world".into())), None);
    }

    #[test]
    fn test_eval_filter_raw_empty_bytes() {
        assert_eq!(eval_filter_raw(&[], &FilterOp::Eq, &Value::Integer(0)), None);
    }

    // --- Filter convenience constructors ---

    #[test]
    fn test_filter_all_constructors() {
        let _ = Filter::eq("a", 1i64);
        let _ = Filter::ne("a", 1i64);
        let _ = Filter::lt("a", 1i64);
        let _ = Filter::le("a", 1i64);
        let _ = Filter::gt("a", 1i64);
        let _ = Filter::ge("a", 1i64);
    }

    #[test]
    fn test_filter_le() {
        let f = Filter::le("v", 5i64);
        assert!(f.matches(&Value::Integer(5)));
        assert!(f.matches(&Value::Integer(3)));
        assert!(!f.matches(&Value::Integer(6)));
    }

    #[test]
    fn test_filter_ge() {
        let f = Filter::ge("v", 5i64);
        assert!(f.matches(&Value::Integer(5)));
        assert!(f.matches(&Value::Integer(7)));
        assert!(!f.matches(&Value::Integer(4)));
    }

    #[test]
    fn test_filter_ne() {
        let f = Filter::ne("v", "alice");
        assert!(f.matches(&Value::Text("bob".into())));
        assert!(!f.matches(&Value::Text("alice".into())));
    }

    #[test]
    fn test_filter_lt() {
        let f = Filter::lt("v", 10i64);
        assert!(f.matches(&Value::Integer(5)));
        assert!(!f.matches(&Value::Integer(10)));
        assert!(!f.matches(&Value::Integer(15)));
    }
}
