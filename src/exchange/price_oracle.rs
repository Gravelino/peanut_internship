use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::core::types::{ESTIMATED_SPREAD_BPS, split_pair_symbols};
use crate::exchange::errors::{ExchangeError, ExchangeResult};
use crate::exchange::http_client::{HttpClient, RetryConfig};
use crate::exchange::rate_limiter::RateLimiter;

/// A single price source with its name and quoted price.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceSource {
    /// Name of the price source (e.g. "binance_ticker", "coingecko").
    pub name: String,
    /// Quoted price for the pair.
    pub price: Decimal,
}

/// Aggregated price data collected from multiple sources.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregatedPrice {
    /// Trading pair (e.g. "ETH/USDT").
    pub pair: String,
    /// Individual price sources that contributed.
    pub sources: Vec<PriceSource>,
    /// Median price across all sources.
    pub median: Decimal,
    /// Mean price across all sources.
    pub mean: Decimal,
    /// Estimated best bid derived from the orderbook mid and spread.
    pub best_bid: Option<Decimal>,
    /// Estimated best ask derived from the orderbook mid and spread.
    pub best_ask: Option<Decimal>,
    /// Mid price (orderbook mid or median fallback).
    pub mid_price: Decimal,
    /// ISO-8601 timestamp of aggregation.
    pub timestamp: String,
}

/// Multi-source price oracle that aggregates prices from Binance, CoinGecko, CoinCap, and Kraken.
///
/// Each source gets its own `HttpClient` with an independent rate limiter and retry config.
pub struct PriceOracle {
    binance_base_url: String,
    clients: Vec<(&'static str, HttpClient)>,
}

impl std::fmt::Debug for PriceOracle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PriceOracle")
            .field("binance_base_url", &self.binance_base_url)
            .finish_non_exhaustive()
    }
}

impl PriceOracle {
    /// Creates a new `PriceOracle` targeting the given Binance base URL.
    pub fn new(binance_base_url: String) -> Self {
        let sources = ["binance", "coingecko", "coincap", "kraken"];
        let retry = RetryConfig::default();
        let clients = sources
            .iter()
            .map(|&name| {
                let limiter =
                    std::sync::Arc::new(tokio::sync::Mutex::new(RateLimiter::default_limiter()));
                let client = HttpClient::with_limiter(limiter, retry.clone(), true)
                    .expect("http client build");
                (name, client)
            })
            .collect();

        Self {
            binance_base_url,
            clients,
        }
    }

    fn find_client(&self, name: &str) -> &HttpClient {
        self.clients
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, c)| c)
            .expect("all source clients must be registered")
    }

    /// Fetches and aggregates prices from all sources, optionally including a CEX orderbook mid.
    pub async fn fetch_aggregated(
        &self,
        pair: &str,
        orderbook_mid: Option<Decimal>,
    ) -> ExchangeResult<AggregatedPrice> {
        let (base_asset, _) = split_pair_symbols(pair)
            .map_err(|error| ExchangeError::InvalidSymbol(error.to_string()))?;
        let mut sources: Vec<PriceSource> = Vec::new();

        if let Ok(price) = self.fetch_binance_ticker(pair).await {
            sources.push(PriceSource {
                name: "binance_ticker".into(),
                price,
            });
        }

        if let Ok(price) = self.fetch_coingecko(base_asset).await {
            sources.push(PriceSource {
                name: "coingecko".into(),
                price,
            });
        }

        if let Ok(price) = self.fetch_coincap(base_asset).await {
            sources.push(PriceSource {
                name: "coincap".into(),
                price,
            });
        }

        if let Ok(price) = self.fetch_kraken(pair).await {
            sources.push(PriceSource {
                name: "kraken".into(),
                price,
            });
        }

        if let Some(mid) = orderbook_mid {
            sources.push(PriceSource {
                name: "cex_orderbook".into(),
                price: mid,
            });
        }

        if sources.is_empty() {
            return Err(ExchangeError::Network("no price source available".into()));
        }

        let mut prices: Vec<Decimal> = sources.iter().map(|s| s.price).collect();
        prices.sort();

        let median = Self::compute_median(&prices);
        let mean = Self::compute_mean(&prices);

        let best_bid = orderbook_mid.map(|m| {
            let spread_est = m * Decimal::from_str_exact(ESTIMATED_SPREAD_BPS)
                .expect("ESTIMATED_SPREAD_BPS is a valid Decimal");
            m - spread_est / Decimal::TWO
        });
        let best_ask = orderbook_mid.map(|m| {
            let spread_est = m * Decimal::from_str_exact(ESTIMATED_SPREAD_BPS)
                .expect("ESTIMATED_SPREAD_BPS is a valid Decimal");
            m + spread_est / Decimal::TWO
        });

        let mid_price = orderbook_mid.unwrap_or(median);

        Ok(AggregatedPrice {
            pair: pair.to_string(),
            sources,
            median,
            mean,
            best_bid,
            best_ask,
            mid_price,
            timestamp: chrono::Utc::now().to_rfc3339(),
        })
    }

    /// Fetches the current price for a pair from the Binance ticker API.
    pub async fn fetch_binance_ticker(&self, pair: &str) -> ExchangeResult<Decimal> {
        let symbol = pair.replace('/', "");
        let url = format!(
            "{}/api/v3/ticker/price?symbol={}",
            self.binance_base_url, symbol
        );
        debug!(url = %url, "Fetching Binance ticker");

        let client = self.find_client("binance");
        let resp: serde_json::Value = client.get(&url, None, 1).await?.json().await?;

        let price_str = resp["price"]
            .as_str()
            .ok_or_else(|| ExchangeError::DecimalParse("no price field".into()))?;
        let price = Decimal::from_str_exact(price_str)
            .map_err(|e| ExchangeError::DecimalParse(format!("{e}")))?;

        debug!(source = "binance_ticker", price = %price, "Got price");
        Ok(price)
    }

    async fn fetch_coingecko(&self, base_asset: &str) -> ExchangeResult<Decimal> {
        let coin_id = match base_asset {
            "ETH" => "ethereum".to_string(),
            "BTC" => "bitcoin".to_string(),
            "USDT" | "USDC" => return Ok(Decimal::ONE),
            _ => base_asset.to_lowercase(),
        };

        let url = format!(
            "https://api.coingecko.com/api/v3/simple/price?ids={}&vs_currencies=usd",
            coin_id
        );
        debug!(url = %url, "Fetching CoinGecko price");

        let client = self.find_client("coingecko");
        let resp: serde_json::Value = client.get(&url, None, 1).await?.json().await?;

        let price = resp[coin_id]["usd"]
            .as_f64()
            .ok_or_else(|| ExchangeError::DecimalParse("coingecko parse failed".into()))?;

        let price_dec = Decimal::from_f64_retain(price)
            .ok_or_else(|| ExchangeError::DecimalParse("coingecko decimal convert".into()))?;

        debug!(source = "coingecko", price = %price_dec, "Got price");
        Ok(price_dec)
    }

    async fn fetch_coincap(&self, base_asset: &str) -> ExchangeResult<Decimal> {
        let coin_id = match base_asset {
            "ETH" => "ethereum".to_string(),
            "BTC" => "bitcoin".to_string(),
            _ => base_asset.to_lowercase(),
        };

        let url = format!("https://api.coincap.io/v2/assets/{}", coin_id);
        debug!(url = %url, "Fetching CoinCap price");

        let client = self.find_client("coincap");
        let resp: serde_json::Value = client.get(&url, None, 1).await?.json().await?;

        let price_f64 = resp["data"]["priceUsd"]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .ok_or_else(|| ExchangeError::DecimalParse("coincap parse failed".into()))?;

        let price = Decimal::from_f64_retain(price_f64)
            .ok_or_else(|| ExchangeError::DecimalParse("coincap decimal convert".into()))?;

        debug!(source = "coincap", price = %price, "Got price");
        Ok(price)
    }

    async fn fetch_kraken(&self, pair: &str) -> ExchangeResult<Decimal> {
        let kraken_pair = match pair {
            "ETH/USDT" => "ETHUSDT".to_string(),
            "ETH/USDC" => "ETHUSDC".to_string(),
            "BTC/USDT" => "XBTUSDT".to_string(),
            "BTC/USDC" => "XBTUSDC".to_string(),
            _ => pair.replace('/', ""),
        };

        let url = format!(
            "https://api.kraken.com/0/public/Ticker?pair={}",
            kraken_pair
        );
        debug!(url = %url, "Fetching Kraken ticker");

        let client = self.find_client("kraken");
        let resp: serde_json::Value = client.get(&url, None, 1).await?.json().await?;

        let result = resp
            .get("result")
            .ok_or_else(|| ExchangeError::DecimalParse("kraken no result".into()))?;

        let key = result
            .as_object()
            .and_then(|o| o.keys().find(|k| !k.is_empty()).cloned())
            .ok_or_else(|| ExchangeError::DecimalParse("kraken no pair key".into()))?;

        let close_price = result[&key]["c"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|v| v.as_str())
            .ok_or_else(|| ExchangeError::DecimalParse("kraken no close price".into()))?;

        let price = Decimal::from_str_exact(close_price)
            .map_err(|e| ExchangeError::DecimalParse(format!("{e}")))?;

        debug!(source = "kraken", price = %price, "Got price");
        Ok(price)
    }

    /// Computes the median of a sorted slice of decimals; returns zero for empty.
    fn compute_median(sorted: &[Decimal]) -> Decimal {
        if sorted.is_empty() {
            return Decimal::ZERO;
        }
        let len = sorted.len();
        if len % 2 == 1 {
            sorted[len / 2]
        } else {
            (sorted[len / 2 - 1] + sorted[len / 2]) / Decimal::TWO
        }
    }

    /// Computes the arithmetic mean of a slice of decimals; returns zero for empty.
    fn compute_mean(values: &[Decimal]) -> Decimal {
        if values.is_empty() {
            return Decimal::ZERO;
        }
        let sum: Decimal = values.iter().sum();
        sum / Decimal::from(values.len() as i32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_median_empty() {
        let values: Vec<Decimal> = vec![];
        assert_eq!(PriceOracle::compute_median(&values), Decimal::ZERO);
    }

    #[test]
    fn test_compute_median_odd_count() {
        let values = vec![Decimal::from(1), Decimal::from(2), Decimal::from(3)];
        assert_eq!(PriceOracle::compute_median(&values), Decimal::from(2));
    }

    #[test]
    fn test_compute_median_even_count() {
        let values = vec![
            Decimal::from(1),
            Decimal::from(2),
            Decimal::from(3),
            Decimal::from(4),
        ];
        assert_eq!(
            PriceOracle::compute_median(&values),
            Decimal::from(5) / Decimal::from(2)
        );
    }

    #[test]
    fn test_compute_median_single() {
        let values = vec![Decimal::from(42)];
        assert_eq!(PriceOracle::compute_median(&values), Decimal::from(42));
    }

    #[test]
    fn test_compute_mean_empty() {
        let values: Vec<Decimal> = vec![];
        assert_eq!(PriceOracle::compute_mean(&values), Decimal::ZERO);
    }

    #[test]
    fn test_compute_mean_single() {
        let values = vec![Decimal::from(100)];
        assert_eq!(PriceOracle::compute_mean(&values), Decimal::from(100));
    }

    #[test]
    fn test_compute_mean_multiple() {
        let values = vec![Decimal::from(10), Decimal::from(20), Decimal::from(30)];
        assert_eq!(PriceOracle::compute_mean(&values), Decimal::from(20));
    }

    #[test]
    fn test_compute_median_already_sorted() {
        let values = vec![
            Decimal::from_str_exact("1000.5").unwrap(),
            Decimal::from_str_exact("2000.0").unwrap(),
            Decimal::from_str_exact("2001.5").unwrap(),
            Decimal::from_str_exact("2010.0").unwrap(),
            Decimal::from_str_exact("2050.0").unwrap(),
        ];
        assert_eq!(
            PriceOracle::compute_median(&values),
            Decimal::from_str_exact("2001.5").unwrap()
        );
    }

    #[test]
    fn test_price_oracle_new_creates_clients() {
        let oracle = PriceOracle::new("https://testnet.binance.vision".into());
        assert_eq!(oracle.binance_base_url, "https://testnet.binance.vision");
        assert_eq!(oracle.clients.len(), 4);
    }

    #[test]
    fn test_price_source_serialization_roundtrip() {
        let source = PriceSource {
            name: "binance_ticker".into(),
            price: Decimal::from_str_exact("2000.5").unwrap(),
        };
        let json = serde_json::to_string(&source).unwrap();
        let decoded: PriceSource = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.name, source.name);
        assert_eq!(decoded.price, source.price);
    }

    #[test]
    fn test_aggregated_price_serialization_roundtrip() {
        let agg = AggregatedPrice {
            pair: "ETH/USDT".into(),
            sources: vec![PriceSource {
                name: "coingecko".into(),
                price: Decimal::from(2000),
            }],
            median: Decimal::from(2000),
            mean: Decimal::from(2000),
            best_bid: Some(Decimal::from(1999)),
            best_ask: Some(Decimal::from(2001)),
            mid_price: Decimal::from(2000),
            timestamp: "2026-01-01T00:00:00Z".into(),
        };
        let json = serde_json::to_string(&agg).unwrap();
        let decoded: AggregatedPrice = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.pair, agg.pair);
        assert_eq!(decoded.median, agg.median);
        assert_eq!(decoded.best_bid, agg.best_bid);
    }
}
