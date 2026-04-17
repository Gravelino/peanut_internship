use thiserror::Error;

/// Errors that can occur when interacting with an exchange.
#[derive(Debug, Error)]
pub enum ExchangeError {
    /// A network connectivity or transport failure.
    #[error("network error: {0}")]
    Network(String),

    /// An authentication or credential failure.
    #[error("authentication error: {0}")]
    Auth(String),

    /// The exchange API rate limit has been exceeded.
    #[error("rate limit exceeded: {0}")]
    RateLimit(String),

    /// An order submission was rejected by the exchange.
    #[error("order rejected: {0}")]
    OrderRejected(String),

    /// Insufficient balance to complete the operation.
    #[error("insufficient funds: {0}")]
    InsufficientFunds(String),

    /// An invalid or unsupported trading symbol was provided.
    #[error("invalid symbol: {0}")]
    InvalidSymbol(String),

    /// A generic API error with a code and message from the exchange.
    #[error("api error {code}: {message}")]
    Api { code: i32, message: String },

    /// A configuration or setup error.
    #[error("configuration error: {0}")]
    Config(String),

    /// A JSON serialization/deserialization error.
    #[error("json parse error: {0}")]
    JsonParse(#[from] serde_json::Error),

    /// An HTTP request/response error.
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    /// A failure to parse a decimal value.
    #[error("decimal parse error: {0}")]
    DecimalParse(String),

    /// A connectivity check to the exchange failed.
    #[error("connection check failed: {0}")]
    ConnectionCheck(String),
}

/// A specialized `Result` type for exchange operations.
pub type ExchangeResult<T> = Result<T, ExchangeError>;
