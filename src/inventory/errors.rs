use thiserror::Error;

/// Errors that can occur during inventory operations.
#[derive(Debug, Error)]
pub enum InventoryError {
    /// The requested venue does not exist.
    #[error("venue not found: {0}")]
    VenueNotFound(String),

    /// The requested asset does not exist.
    #[error("asset not found: {0}")]
    AssetNotFound(String),

    /// Not enough balance to fulfill the request.
    #[error("insufficient balance: {0}")]
    InsufficientBalance(String),

    /// A balance would become negative after adjustment.
    #[error("negative balance: {0}")]
    NegativeBalance(String),

    /// No price available to convert an asset to USD.
    #[error("no price for USD valuation: {0}")]
    NoPriceForUsd(String),

    /// An I/O error occurred.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A CSV serialization/deserialization error occurred.
    #[error("csv error: {0}")]
    Csv(#[from] csv::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Convenience alias for `Result<T, InventoryError>`.
pub type InventoryResult<T> = Result<T, InventoryError>;
