use std::fmt;
use std::io;

#[derive(Debug)]
pub enum BoogyError {
    Io(io::Error),
    Corruption(String),
    TableNotFound(String),
    TableExists(String),
    RowNotFound(String),
    DuplicateKey(u64),
    SchemaMismatch(String),
    TypeMismatch(String),
    IndexNotFound(String),
    IndexExists(String),
    PageFull,
    TransactionConflict,
}

impl fmt::Display for BoogyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BoogyError::Io(e) => write!(f, "I/O error: {e}"),
            BoogyError::Corruption(msg) => write!(f, "corruption: {msg}"),
            BoogyError::TableNotFound(t) => write!(f, "table '{t}' not found"),
            BoogyError::TableExists(t) => write!(f, "table '{t}' already exists"),
            BoogyError::RowNotFound(id) => write!(f, "row '{id}' not found"),
            BoogyError::DuplicateKey(id) => write!(f, "duplicate key {id}"),
            BoogyError::SchemaMismatch(msg) => write!(f, "schema mismatch: {msg}"),
            BoogyError::TypeMismatch(msg) => write!(f, "type mismatch: {msg}"),
            BoogyError::IndexNotFound(idx) => write!(f, "index '{idx}' not found"),
            BoogyError::IndexExists(idx) => write!(f, "index '{idx}' already exists"),
            BoogyError::PageFull => write!(f, "page full"),
            BoogyError::TransactionConflict => write!(f, "transaction conflict"),
        }
    }
}

impl std::error::Error for BoogyError {}

impl From<io::Error> for BoogyError {
    fn from(e: io::Error) -> Self {
        BoogyError::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, BoogyError>;
