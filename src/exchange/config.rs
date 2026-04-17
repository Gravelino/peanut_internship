use crate::exchange::errors::{ExchangeError, ExchangeResult};

pub const BINANCE_TESTNET_BASE_URL: &str = "https://testnet.binance.vision";
pub const BINANCE_TESTNET_WS_URL: &str = "wss://testnet.binance.vision/ws";

#[derive(Debug, Clone)]
pub struct BinanceConfig {
    pub api_key: String,
    pub secret: String,
    pub base_url: String,
    pub ws_url: String,
    pub sandbox: bool,
    pub enable_rate_limit: bool,
}

impl BinanceConfig {
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
