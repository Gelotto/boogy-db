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
            (Value::Real(a), Value::Real(b)) => a.partial_cmp(b),
            (Value::Text(a), Value::Text(b)) => Some(a.cmp(b)),
            (Value::Boolean(a), Value::Boolean(b)) => Some(a.cmp(b)),
            (Value::Blob(a), Value::Blob(b)) => Some(a.cmp(b)),
            // Cross-type: integer/real comparison
            (Value::Integer(a), Value::Real(b)) => (*a as f64).partial_cmp(b),
            (Value::Real(a), Value::Integer(b)) => a.partial_cmp(&(*b as f64)),
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
}

impl ColumnDef {
    pub fn new(name: impl Into<String>, col_type: Type) -> Self {
        Self {
            name: name.into(),
            col_type,
            nullable: true,
            unique: false,
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
}
