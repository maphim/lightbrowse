use thiserror::Error;

/// Unified error type across all backends.
#[derive(Debug, Error)]
pub enum Error {
    #[error("transport error: {0}")]
    Transport(String),
    #[error("http error {status}: {message}")]
    Http { status: u16, message: String },
    #[error("invalid url: {0}")]
    InvalidUrl(String),
    #[error("parse error: {0}")]
    Parse(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("backend not initialized: {0}")]
    NotInitialized(String),
}

pub type Result<T> = std::result::Result<T, Error>;
