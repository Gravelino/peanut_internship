use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::exchange::errors::{ExchangeError, ExchangeResult};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceSource {
    pub name: String,
    pub price: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregatedPrice {
    pub pair: String,
    pub sources: Vec<PriceSource>,
    pub median: Decimal,
    pub mean: Decimal,
    pub best_bid: Option<Decimal>,
    pub best_ask: Option<Decimal>,
    pub mid_price: Decimal,
    pub timestamp: String,
}

pub struct PriceOracle {
    http: reqwest::Client,
    binance_base_url: String,
}

impl std::fmt::Debug for PriceOracle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PriceOracle")
            .field("binance_base_url", &self.binance_base_url)
            .finish_non_exhaustive()
    }
}

impl PriceOracle {
    pub fn new(binance_base_url: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("http client build");

        Self {
            http,
            binance_base_url,
        }
    }

    pub async fn fetch_aggregated(
        &self,
        pair: &str,
        orderbook_mid: Option<Decimal>,
    ) -> ExchangeResult<AggregatedPrice> {
        let base_asset = pair.split('/').next().unwrap_or("ETH");
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
            return Err(ExchangeError::Network(
                "no price source available".into(),
            ));
        }

        let mut prices: Vec<Decimal> = sources.iter().map(|s| s.price).collect();
        prices.sort();

        let median = Self::compute_median(&prices);
        let mean = Self::compute_mean(&prices);

        let best_bid = orderbook_mid.map(|m| {
            let spread_est = m * Decimal::from_str_exact("0.0001").unwrap_or(Decimal::ZERO);
            m - spread_est / Decimal::TWO
        });
        let best_ask = orderbook_mid.map(|m| {
            let spread_est = m * Decimal::from_str_exact("0.0001").unwrap_or(Decimal::ZERO);
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

    pub async fn fetch_binance_ticker(&self, pair: &str) -> ExchangeResult<Decimal> {
        let symbol = pair.replace('/', "");
        let url = format!("{}/api/v3/ticker/price?symbol={}", self.binance_base_url, symbol);
        debug!(url = %url, "Fetching Binance ticker");

        let resp: serde_json::Value = self.http.get(&url).send().await?.json().await?;

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

        let resp: serde_json::Value = self.http.get(&url).send().await?.json().await?;

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

        let resp: serde_json::Value = self.http.get(&url).send().await?.json().await?;

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

        let resp: serde_json::Value = self.http.get(&url).send().await?.json().await?;

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

    fn compute_mean(values: &[Decimal]) -> Decimal {
        if values.is_empty() {
            return Decimal::ZERO;
        }
        let sum: Decimal = values.iter().sum();
        sum / Decimal::from(values.len() as i32)
    }
}
