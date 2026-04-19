use crate::exchange::errors::{ExchangeError, ExchangeResult};

/// Base URL for the Bybit testnet REST API.
pub const BYBIT_TESTNET_BASE_URL: &str = "https://api-testnet.bybit.com";
/// WebSocket URL for the Bybit testnet streaming API.
pub const BYBIT_TESTNET_WS_URL: &str = "wss://stream-testnet.bybit.com/v5/public/spot";

/// Configuration for connecting to the Bybit exchange.
#[derive(Debug, Clone)]
pub struct BybitConfig {
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
    /// RecvWindow for request signing (milliseconds).
    pub recv_window: u64,
}

impl BybitConfig {
    /// Creates a config from environment variables (`BYBIT_API_KEY`, `BYBIT_SECRET`).
    pub fn from_env() -> ExchangeResult<Self> {
        dotenvy::dotenv().ok();
        let api_key = std::env::var("BYBIT_API_KEY")
            .map_err(|_| ExchangeError::Config("BYBIT_API_KEY not set".into()))?;
        let secret = std::env::var("BYBIT_SECRET")
            .map_err(|_| ExchangeError::Config("BYBIT_SECRET not set".into()))?;

        if api_key.is_empty() || api_key == "your_key_here" {
            return Err(ExchangeError::Config("BYBIT_API_KEY not configured".into()));
        }
        if secret.is_empty() || secret == "your_secret_here" {
            return Err(ExchangeError::Config("BYBIT_SECRET not configured".into()));
        }

        Ok(Self {
            api_key,
            secret,
            base_url: BYBIT_TESTNET_BASE_URL.to_string(),
            ws_url: BYBIT_TESTNET_WS_URL.to_string(),
            sandbox: true,
            enable_rate_limit: true,
            recv_window: 20_000,
        })
    }

    /// Creates a config with a custom REST API base URL.
    pub fn with_custom_url(api_key: String, secret: String, base_url: String) -> Self {
        Self {
            api_key,
            secret,
            base_url,
            ws_url: BYBIT_TESTNET_WS_URL.to_string(),
            sandbox: true,
            enable_rate_limit: true,
            recv_window: 20_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bybit_config_custom_url() {
        let config = BybitConfig::with_custom_url(
            "key".into(),
            "secret".into(),
            "https://custom.bybit.com".into(),
        );
        assert_eq!(config.api_key, "key");
        assert_eq!(config.secret, "secret");
        assert_eq!(config.base_url, "https://custom.bybit.com");
        assert_eq!(config.ws_url, BYBIT_TESTNET_WS_URL);
        assert!(config.sandbox);
        assert!(config.enable_rate_limit);
        assert_eq!(config.recv_window, 20_000);
    }

    #[test]
    fn test_bybit_config_constants() {
        assert!(BYBIT_TESTNET_BASE_URL.contains("bybit"));
        assert!(BYBIT_TESTNET_WS_URL.contains("bybit"));
    }
}
