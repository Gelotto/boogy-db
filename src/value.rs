use std::cmp::Ordering;

/// Column data types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Type {
    Text,
    Integer,
    Real,
    Blob,
    Boolean,
}

/// A dynamically-typed value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Text(String),
    Integer(i64),
    Real(f64),
    Blob(Vec<u8>),
    Boolean(bool),
}

impl Value {
    pub fn value_type(&self) -> Option<Type> {
        match self {
            Value::Null => None,
            Value::Text(_) => Some(Type::Text),
            Value::Integer(_) => Some(Type::Integer),
            Value::Real(_) => Some(Type::Real),
            Value::Blob(_) => Some(Type::Blob),
            Value::Boolean(_) => Some(Type::Boolean),
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Compare two values for ordering. Returns None if types are incompatible.
    pub fn compare(&self, other: &Value) -> Option<Ordering> {
        match (self, other) {
            (Value::Null, Value::Null) => Some(Ordering::Equal),
            (Value::Null, _) => Some(Ordering::Less),
            (_, Value::Null) => Some(Ordering::Greater),
            (Value::Integer(a), Value::Integer(b)) => Some(a.cmp(b)),
            (Value::Real(a), Value::Real(b)) => {
                match (a.is_nan(), b.is_nan()) {
                    (true, true) => Some(Ordering::Equal),
                    (true, false) => Some(Ordering::Greater), // NaN sorts last
                    (false, true) => Some(Ordering::Less),
                    (false, false) => a.partial_cmp(b),
                }
            }
            (Value::Text(a), Value::Text(b)) => Some(a.cmp(b)),
            (Value::Boolean(a), Value::Boolean(b)) => Some(a.cmp(b)),
            (Value::Blob(a), Value::Blob(b)) => Some(a.cmp(b)),
            // Cross-type: integer/real comparison
            (Value::Integer(a), Value::Real(b)) => {
                if b.is_nan() {
                    Some(Ordering::Less) // finite < NaN
                } else {
                    (*a as f64).partial_cmp(b)
                }
            }
            (Value::Real(a), Value::Integer(b)) => {
                if a.is_nan() {
                    Some(Ordering::Greater) // NaN > finite
                } else {
                    a.partial_cmp(&(*b as f64))
                }
            }
            _ => None,
        }
    }
}

/// Column definition for CREATE TABLE.
#[derive(Debug, Clone)]
pub struct ColumnDef {
    pub name: String,
    pub col_type: Type,
    pub nullable: bool,
    pub unique: bool,
    /// Value returned for rows that predate this column (or that omit it
    /// on insert). `None` => such reads return `Value::Null`.
    pub default: Option<Value>,
    /// Tombstone: a dropped column keeps its slot (so later columns'
    /// `col_id`s don't shift) but is invisible to reads/finds. Its
    /// `col_id` is never reused.
    pub dropped: bool,
}

impl ColumnDef {
    pub fn new(name: impl Into<String>, col_type: Type) -> Self {
        Self {
            name: name.into(),
            col_type,
            nullable: true,
            unique: false,
            default: None,
            dropped: false,
        }
    }

    pub fn not_null(mut self) -> Self {
        self.nullable = false;
        self
    }

    pub fn unique(mut self) -> Self {
        self.unique = true;
        self
    }

    pub fn default(mut self, v: Value) -> Self {
        self.default = Some(v);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_def_default_and_dropped_fields() {
        // Builder sets default correctly; dropped stays false.
        let col = ColumnDef::new("x", Type::Integer).default(Value::Integer(5));
        assert_eq!(col.default, Some(Value::Integer(5)));
        assert!(!col.dropped);

        // new() alone yields default == None and dropped == false.
        let col2 = ColumnDef::new("y", Type::Text);
        assert_eq!(col2.default, None);
        assert!(!col2.dropped);
    }
}
