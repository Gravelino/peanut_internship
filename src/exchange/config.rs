use crate::exchange::errors::{ExchangeError, ExchangeResult};

/// Base URL for the Binance testnet REST API.
pub const BINANCE_TESTNET_BASE_URL: &str = "https://testnet.binance.vision";
/// WebSocket URL for the Binance testnet streaming API.
pub const BINANCE_TESTNET_WS_URL: &str = "wss://testnet.binance.vision/ws";

/// Configuration for connecting to the Binance exchange.
#[derive(Debug, Clone)]
pub struct BinanceConfig {
    /// API key for authentication.
    pub api_key: String,
    /// API secret for signing requests.
    pub secret: String,
    /// Base URL for REST API endpoints.
    pub base_url: String,
    /// WebSocket URL for streaming data.
    pub ws_url: String,
    /// Whether the connection uses the sandbox/testnet environment.
    pub sandbox: bool,
    /// Whether built-in rate limiting is enabled.
    pub enable_rate_limit: bool,
}

impl BinanceConfig {
    /// Creates a config from environment variables (`BINANCE_TESTNET_API_KEY`, `BINANCE_TESTNET_SECRET`).
    pub fn from_env() -> ExchangeResult<Self> {
        dotenvy::dotenv().ok();
        let api_key = std::env::var("BINANCE_TESTNET_API_KEY")
            .map_err(|_| ExchangeError::Config("BINANCE_TESTNET_API_KEY not set".into()))?;
        let secret = std::env::var("BINANCE_TESTNET_SECRET")
            .map_err(|_| ExchangeError::Config("BINANCE_TESTNET_SECRET not set".into()))?;

        if api_key.is_empty() || api_key == "your_key_here" {
            return Err(ExchangeError::Config(
                "BINANCE_TESTNET_API_KEY not configured".into(),
            ));
        }
        if secret.is_empty() || secret == "your_secret_here" {
            return Err(ExchangeError::Config(
                "BINANCE_TESTNET_SECRET not configured".into(),
            ));
        }

        Ok(Self {
            api_key,
            secret,
            base_url: BINANCE_TESTNET_BASE_URL.to_string(),
            ws_url: BINANCE_TESTNET_WS_URL.to_string(),
            sandbox: true,
            enable_rate_limit: true,
        })
    }

    /// Creates a config with a custom REST API base URL.
    pub fn with_custom_url(api_key: String, secret: String, base_url: String) -> Self {
        Self {
            api_key,
            secret,
            base_url,
            ws_url: BINANCE_TESTNET_WS_URL.to_string(),
            sandbox: true,
            enable_rate_limit: true,
        }
    }
}
