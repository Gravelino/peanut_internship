//! Error types for the `executor` module.

use thiserror::Error;

/// Errors produced by the executor and its recovery machinery.
#[derive(Debug, Error)]
pub enum ExecutorError {
    /// Wrapped exchange-side error.
    #[error("exchange error: {0}")]
    Exchange(#[from] crate::exchange::errors::ExchangeError),

    /// Invalid or expired signal submitted to `execute`.
    #[error("invalid signal: {0}")]
    InvalidSignal(String),

    /// Real-mode execution path not implemented yet.
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),

    /// Persistent-state backend failure (SQLite journal, etc.).
    #[error("persistence error: {0}")]
    Persistence(String),
}

/// Convenience result alias.
pub type ExecutorResult<T> = Result<T, ExecutorError>;
