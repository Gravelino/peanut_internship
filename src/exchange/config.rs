use crate::exchange::errors::{ExchangeError, ExchangeResult};

/// Base URL for the Binance testnet REST API.
pub const BINANCE_TESTNET_BASE_URL: &str = "https://testnet.binance.vision";
/// WebSocket URL for the Binance testnet streaming API.
pub const BINANCE_TESTNET_WS_URL: &str = "wss://stream.testnet.binance.vision";
pub const BINANCE_PRODUCTION_BASE_URL: &str = "https://api.binance.com";
pub const BINANCE_PRODUCTION_WS_URL: &str = "wss://stream.binance.com:9443/ws";

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
    /// The recvWindow parameter for Binance requests.
    pub recv_window: u64,
}

impl BinanceConfig {
    /// Creates a config from environment variables (`BINANCE_TESTNET_API_KEY`, `BINANCE_TESTNET_SECRET`).
    pub fn from_env() -> ExchangeResult<Self> {
        Self::from_env_for(false)
    }

    pub fn from_env_for(production: bool) -> ExchangeResult<Self> {
        dotenvy::dotenv().ok();
        let (key_var, secret_var, base_url, ws_url, sandbox) = if production {
            (
                "BINANCE_API_KEY",
                "BINANCE_API_SECRET",
                BINANCE_PRODUCTION_BASE_URL,
                BINANCE_PRODUCTION_WS_URL,
                false,
            )
        } else {
            (
                "BINANCE_TESTNET_API_KEY",
                "BINANCE_TESTNET_SECRET",
                BINANCE_TESTNET_BASE_URL,
                BINANCE_TESTNET_WS_URL,
                true,
            )
        };
        let api_key = std::env::var(key_var)
            .map_err(|_| ExchangeError::Config(format!("{key_var} not set")))?;
        let secret = std::env::var(secret_var)
            .map_err(|_| ExchangeError::Config(format!("{secret_var} not set")))?;

        let recv_window = std::env::var("BINANCE_RECV_WINDOW")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(crate::core::types::BINANCE_RECV_WINDOW_MS);

        if api_key.is_empty() || api_key == "your_key_here" {
            return Err(ExchangeError::Config(format!("{key_var} not configured")));
        }
        if secret.is_empty() || secret == "your_secret_here" {
            return Err(ExchangeError::Config(format!(
                "{secret_var} not configured"
            )));
        }

        Ok(Self {
            api_key,
            secret,
            base_url: base_url.to_string(),
            ws_url: ws_url.to_string(),
            sandbox,
            enable_rate_limit: true,
            recv_window,
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
            recv_window: crate::core::types::BINANCE_RECV_WINDOW_MS,
        }
    }
}
