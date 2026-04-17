use thiserror::Error;

#[derive(Debug, Error)]
pub enum InventoryError {
    #[error("venue not found: {0}")]
    VenueNotFound(String),

    #[error("asset not found: {0}")]
    AssetNotFound(String),

    #[error("insufficient balance: {0}")]
    InsufficientBalance(String),

    #[error("negative balance: {0}")]
    NegativeBalance(String),

    #[error("no price for USD valuation: {0}")]
    NoPriceForUsd(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("csv error: {0}")]
    Csv(#[from] csv::Error),
}

pub type InventoryResult<T> = Result<T, InventoryError>;
