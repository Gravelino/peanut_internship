use thiserror::Error;

#[derive(Debug, Error)]
pub enum ExchangeError {
    #[error("network error: {0}")]
    Network(String),

    #[error("authentication error: {0}")]
    Auth(String),

    #[error("rate limit exceeded: {0}")]
    RateLimit(String),

    #[error("order rejected: {0}")]
    OrderRejected(String),

    #[error("insufficient funds: {0}")]
    InsufficientFunds(String),

    #[error("invalid symbol: {0}")]
    InvalidSymbol(String),

    #[error("api error {code}: {message}")]
    Api { code: i32, message: String },

    #[error("configuration error: {0}")]
    Config(String),

    #[error("json parse error: {0}")]
    JsonParse(#[from] serde_json::Error),

    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("decimal parse error: {0}")]
    DecimalParse(String),

    #[error("connection check failed: {0}")]
    ConnectionCheck(String),
}

pub type ExchangeResult<T> = Result<T, ExchangeError>;
