use thiserror::Error;

/// All errors produced by the richclip library.
#[derive(Debug, Error)]
pub enum Error {
    /// The requested item or MIME format does not exist.
    #[error("not found")]
    NotFound,

    /// A SQLite database error.
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    /// An I/O error (blob file read/write).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization/deserialization error.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// A UUID parse error.
    #[error("invalid id: {0}")]
    InvalidId(#[from] uuid::Error),

    /// Any other error wrapped with context.
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Exit code mapping for CLI error contract.
    /// `2` = not found (no such item/mime), `1` = everything else.
    pub fn code(&self) -> i32 {
        match self {
            Error::NotFound => 2,
            _ => 1,
        }
    }

    /// Machine-readable error code string for JSON output.
    pub fn json_code(&self) -> &'static str {
        match self {
            Error::NotFound => "not_found",
            _ => "error",
        }
    }
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;
