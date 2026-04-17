use std::collections::HashMap;
use std::sync::Arc;

use hmac::{Hmac, Mac};
use rust_decimal::Decimal;
use sha2::Sha256;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::exchange::config::BinanceConfig;
use crate::exchange::errors::{ExchangeError, ExchangeResult};
use crate::exchange::rate_limiter::RateLimiter;
use crate::exchange::types::{FeeStructure, MyTrade, NormalizedBalance, OrderBookSnapshot, OrderResult};

type HmacSha256 = Hmac<Sha256>;

fn sign_query(query: &str, secret: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC can take key of any size");
    mac.update(query.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

#[derive(Debug)]
pub struct ExchangeClient {
    config: BinanceConfig,
    http: reqwest::Client,
    rate_limiter: Arc<Mutex<RateLimiter>>,
}

impl ExchangeClient {
    pub fn new(config: BinanceConfig) -> ExchangeResult<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(ExchangeError::Http)?;

        Ok(Self {
            config,
            http,
            rate_limiter: Arc::new(Mutex::new(RateLimiter::default())),
        })
    }

    pub async fn health_check(&self) -> ExchangeResult<u64> {
        let url = format!("{}/api/v3/time", self.config.base_url);
        debug!(url = %url, "Health check: fetching server time");

        let resp: serde_json::Value = self
            .http
            .get(&url)
            .header("X-MBX-APIKEY", &self.config.api_key)
            .send()
            .await?
            .json()
            .await?;

        let server_time = resp["serverTime"]
            .as_u64()
            .ok_or_else(|| ExchangeError::ConnectionCheck("no serverTime in response".into()))?;

        info!(server_time, "Binance testnet connection OK");
        Ok(server_time)
    }

    pub async fn fetch_order_book(
        &self,
        symbol: &str,
        limit: u32,
    ) -> ExchangeResult<OrderBookSnapshot> {
        let weight = if limit <= 100 {
            5
        } else if limit <= 500 {
            10
        } else {
            50
        };
        self.check_rate_limit(weight).await;

        let url = format!(
            "{}/api/v3/depth?symbol={}&limit={}",
            self.config.base_url,
            symbol.replace('/', ""),
            limit,
        );

        debug!(url = %url, symbol, limit, "Fetching order book");

        let resp: serde_json::Value = self
            .http
            .get(&url)
            .header("X-MBX-APIKEY", &self.config.api_key)
            .send()
            .await?
            .json()
            .await?;

        self.check_api_error(&resp)?;

        let bids_raw = resp["bids"].as_array().ok_or_else(|| {
            ExchangeError::JsonParse(
                serde_json::from_str::<serde_json::Value>("\"missing bids\"").unwrap_err(),
            )
        })?;
        let asks_raw = resp["asks"].as_array().ok_or_else(|| {
            ExchangeError::JsonParse(
                serde_json::from_str::<serde_json::Value>("\"missing asks\"").unwrap_err(),
            )
        })?;

        let mut bids: Vec<(Decimal, Decimal)> = Vec::with_capacity(bids_raw.len());
        for level in bids_raw {
            let price = Self::parse_decimal(&level[0])?;
            let qty = Self::parse_decimal(&level[1])?;
            bids.push((price, qty));
        }
        bids.sort_by(|a, b| b.0.cmp(&a.0));

        let mut asks: Vec<(Decimal, Decimal)> = Vec::with_capacity(asks_raw.len());
        for level in asks_raw {
            let price = Self::parse_decimal(&level[0])?;
            let qty = Self::parse_decimal(&level[1])?;
            asks.push((price, qty));
        }
        asks.sort_by(|a, b| a.0.cmp(&b.0));

        let best_bid = bids.first().copied();
        let best_ask = asks.first().copied();

        let (mid_price, spread_bps) = match (best_bid, best_ask) {
            (Some((bid_p, _)), Some((ask_p, _))) => {
                let mid = (bid_p + ask_p) / Decimal::TWO;
                let spread = ask_p - bid_p;
                let bps = if mid.is_zero() {
                    Decimal::ZERO
                } else {
                    spread / mid * Decimal::from(10000)
                };
                (mid, bps)
            }
            _ => (Decimal::ZERO, Decimal::ZERO),
        };

        let timestamp = resp["lastUpdateId"].as_u64().unwrap_or(0);

        Ok(OrderBookSnapshot {
            symbol: symbol.to_string(),
            timestamp,
            bids,
            asks,
            best_bid,
            best_ask,
            mid_price,
            spread_bps,
        })
    }

    pub async fn fetch_balance(&self) -> ExchangeResult<HashMap<String, NormalizedBalance>> {
        self.check_rate_limit(10).await;

        let query = "recvWindow=60000";
        let signed = self.sign_request(query);

        let url = format!(
            "{}/api/v3/account?{}&{}",
            self.config.base_url, query, signed
        );

        debug!(url = %url, "Fetching account balance");

        let resp: serde_json::Value = self
            .http
            .get(&url)
            .header("X-MBX-APIKEY", &self.config.api_key)
            .send()
            .await?
            .json()
            .await?;

        self.check_api_error(&resp)?;

        let balances_raw = resp["balances"].as_array().ok_or_else(|| {
            ExchangeError::JsonParse(
                serde_json::from_str::<serde_json::Value>("\"missing balances\"").unwrap_err(),
            )
        })?;

        let mut result = HashMap::new();
        for b in balances_raw {
            let asset = b["asset"].as_str().unwrap_or("");
            let free = Self::parse_decimal(&b["free"])?;
            let locked = Self::parse_decimal(&b["locked"])?;
            let total = free + locked;
            if total > Decimal::ZERO {
                result.insert(
                    asset.to_string(),
                    NormalizedBalance {
                        free,
                        locked,
                        total,
                    },
                );
            }
        }

        Ok(result)
    }

    pub async fn create_limit_gtc_order(
        &self,
        symbol: &str,
        side: &str,
        amount: f64,
        price: f64,
    ) -> ExchangeResult<OrderResult> {
        self.check_rate_limit(1).await;

        let query = format!(
            "symbol={}&side={}&type=LIMIT&timeInForce=GTC&quantity={}&price={}&recvWindow=60000",
            symbol.replace('/', ""),
            side.to_uppercase(),
            amount,
            price,
        );

        let signed = self.sign_request(&query);
        let url = format!("{}/api/v3/order?{}&{}", self.config.base_url, query, signed);

        debug!(symbol, side, amount, price, "Placing LIMIT GTC order");

        let resp: serde_json::Value = self
            .http
            .post(&url)
            .header("X-MBX-APIKEY", &self.config.api_key)
            .send()
            .await?
            .json()
            .await?;

        self.check_api_error(&resp)?;

        Self::parse_order_result(&resp)
    }

    pub async fn create_limit_ioc_order(
        &self,
        symbol: &str,
        side: &str,
        amount: f64,
        price: f64,
    ) -> ExchangeResult<OrderResult> {
        self.check_rate_limit(1).await;

        let query = format!(
            "symbol={}&side={}&type=LIMIT&timeInForce=IOC&quantity={}&price={}&recvWindow=60000",
            symbol.replace('/', ""),
            side.to_uppercase(),
            amount,
            price,
        );

        let signed = self.sign_request(&query);
        let url = format!("{}/api/v3/order?{}&{}", self.config.base_url, query, signed);

        debug!(symbol, side, amount, price, "Placing LIMIT IOC order");

        let resp: serde_json::Value = self
            .http
            .post(&url)
            .header("X-MBX-APIKEY", &self.config.api_key)
            .send()
            .await?
            .json()
            .await?;

        self.check_api_error(&resp)?;

        Self::parse_order_result(&resp)
    }

    pub async fn create_market_order(
        &self,
        symbol: &str,
        side: &str,
        amount: f64,
    ) -> ExchangeResult<OrderResult> {
        self.check_rate_limit(1).await;

        let query = format!(
            "symbol={}&side={}&type=MARKET&quantity={}&recvWindow=60000",
            symbol.replace('/', ""),
            side.to_uppercase(),
            amount,
        );

        let signed = self.sign_request(&query);
        let url = format!("{}/api/v3/order?{}&{}", self.config.base_url, query, signed);

        debug!(symbol, side, amount, "Placing MARKET order");

        let resp: serde_json::Value = self
            .http
            .post(&url)
            .header("X-MBX-APIKEY", &self.config.api_key)
            .send()
            .await?
            .json()
            .await?;

        self.check_api_error(&resp)?;

        Self::parse_order_result(&resp)
    }

    pub async fn cancel_order(&self, order_id: &str, symbol: &str) -> ExchangeResult<OrderResult> {
        self.check_rate_limit(1).await;

        let query = format!(
            "symbol={}&orderId={}&recvWindow=60000",
            symbol.replace('/', ""),
            order_id,
        );

        let signed = self.sign_request(&query);
        let url = format!("{}/api/v3/order?{}&{}", self.config.base_url, query, signed);

        debug!(order_id, symbol, "Cancelling order");

        let resp: serde_json::Value = self
            .http
            .delete(&url)
            .header("X-MBX-APIKEY", &self.config.api_key)
            .send()
            .await?
            .json()
            .await?;

        self.check_api_error(&resp)?;

        Self::parse_order_result(&resp)
    }

    pub async fn fetch_order_status(
        &self,
        order_id: &str,
        symbol: &str,
    ) -> ExchangeResult<OrderResult> {
        self.check_rate_limit(2).await;

        let query = format!(
            "symbol={}&orderId={}&recvWindow=60000",
            symbol.replace('/', ""),
            order_id,
        );

        let signed = self.sign_request(&query);
        let url = format!("{}/api/v3/order?{}&{}", self.config.base_url, query, signed);

        debug!(order_id, symbol, "Fetching order status");

        let resp: serde_json::Value = self
            .http
            .get(&url)
            .header("X-MBX-APIKEY", &self.config.api_key)
            .send()
            .await?
            .json()
            .await?;

        self.check_api_error(&resp)?;

        Self::parse_order_result(&resp)
    }

    pub async fn get_trading_fees(&self, symbol: &str) -> ExchangeResult<FeeStructure> {
        self.check_rate_limit(1).await;

        let query = format!("symbol={}&recvWindow=60000", symbol.replace('/', ""));
        let signed = self.sign_request(&query);
        let url = format!(
            "{}/sapi/v1/asset/tradeFee?{}&{}",
            self.config.base_url, query, signed
        );

        debug!(symbol, "Fetching trading fees");

        let resp: serde_json::Value = self
            .http
            .get(&url)
            .header("X-MBX-APIKEY", &self.config.api_key)
            .send()
            .await?
            .json()
            .await?;

        self.check_api_error(&resp)?;

        let arr = resp.as_array().ok_or_else(|| {
            ExchangeError::JsonParse(
                serde_json::from_str::<serde_json::Value>("\"expected array\"").unwrap_err(),
            )
        })?;

        let first = arr.first().ok_or_else(|| ExchangeError::Api {
            code: -1,
            message: "no fee data".into(),
        })?;

        let maker = Self::parse_decimal(&first["makerCommission"])?;
        let taker = Self::parse_decimal(&first["takerCommission"])?;

        Ok(FeeStructure { maker, taker })
    }

    pub async fn fetch_my_trades(
        &self,
        symbol: &str,
        limit: u32,
    ) -> ExchangeResult<Vec<MyTrade>> {
        self.check_rate_limit(5).await;

        let query = format!(
            "symbol={}&limit={}&recvWindow=60000",
            symbol.replace('/', ""),
            limit,
        );
        let signed = self.sign_request(&query);
        let url = format!(
            "{}/api/v3/myTrades?{}&{}",
            self.config.base_url, query, signed
        );

        debug!(symbol, limit, "Fetching my trades");

        let resp: serde_json::Value = self
            .http
            .get(&url)
            .header("X-MBX-APIKEY", &self.config.api_key)
            .send()
            .await?
            .json()
            .await?;

        self.check_api_error(&resp)?;

        let arr = resp.as_array().ok_or_else(|| {
            ExchangeError::JsonParse(
                serde_json::from_str::<serde_json::Value>("\"expected array\"").unwrap_err(),
            )
        })?;

        let mut trades = Vec::new();
        for t in arr {
            let id = t["id"].as_u64().unwrap_or(0).to_string();
            let order_id = t["orderId"].as_u64().unwrap_or(0).to_string();
            let side = t["isBuyer"].as_bool().map(|b| if b { "buy" } else { "sell" }).unwrap_or("unknown");
            let price = Self::parse_decimal(&t["price"])?;
            let qty = Self::parse_decimal(&t["qty"])?;
            let fee = Self::parse_decimal(&t["commission"])?;
            let fee_asset = t["commissionAsset"].as_str().unwrap_or("").to_string();
            let timestamp = t["time"].as_u64().unwrap_or(0);

            trades.push(MyTrade {
                id,
                order_id,
                symbol: symbol.to_string(),
                side: side.to_string(),
                price,
                qty,
                fee,
                fee_asset,
                timestamp,
            });
        }

        Ok(trades)
    }

    fn sign_request(&self, query: &str) -> String {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let query_with_ts = format!("{}&timestamp={}", query, timestamp);
        let signature = sign_query(&query_with_ts, &self.config.secret);
        format!("timestamp={}&signature={}", timestamp, signature)
    }

    fn check_api_error(&self, resp: &serde_json::Value) -> ExchangeResult<()> {
        if let Some(code) = resp["code"].as_i64()
            && code < 0
        {
            let msg = resp["msg"].as_str().unwrap_or("unknown error");
            match code {
                -1015 => return Err(ExchangeError::RateLimit(msg.to_string())),
                -2010 => return Err(ExchangeError::InsufficientFunds(msg.to_string())),
                -1121 | -1013 => return Err(ExchangeError::InvalidSymbol(msg.to_string())),
                _ => {
                    return Err(ExchangeError::Api {
                        code: code as i32,
                        message: msg.to_string(),
                    });
                }
            }
        }
        Ok(())
    }

    fn parse_decimal(val: &serde_json::Value) -> ExchangeResult<Decimal> {
        let s = val
            .as_str()
            .ok_or_else(|| ExchangeError::DecimalParse("expected string decimal".into()))?;
        Decimal::from_str_exact(s)
            .map_err(|e| ExchangeError::DecimalParse(format!("decimal parse: {e}")))
    }

    fn parse_order_result(resp: &serde_json::Value) -> ExchangeResult<OrderResult> {
        let id = resp["orderId"].as_u64().unwrap_or(0).to_string();
        let symbol = resp["symbol"].as_str().unwrap_or("").to_string();
        let side = resp["side"].as_str().unwrap_or("").to_string();
        let order_type = resp["type"].as_str().unwrap_or("").to_string();
        let time_in_force = resp["timeInForce"].as_str().unwrap_or("").to_string();

        let amount_requested = Self::parse_decimal(&resp["origQty"])?;
        let amount_filled = Self::parse_decimal(&resp["executedQty"])?;

        let avg_fill_price = if amount_filled > Decimal::ZERO {
            let cummulative_quote = Self::parse_decimal(&resp["cummulativeQuoteQty"])?;
            cummulative_quote / amount_filled
        } else {
            Decimal::ZERO
        };

        let fee_val = resp
            .get("fills")
            .and_then(|f| f.as_array())
            .map(|fills| {
                fills
                    .iter()
                    .filter_map(|f| Self::parse_decimal(&f["commission"]).ok())
                    .fold(Decimal::ZERO, |acc, x| acc + x)
            })
            .unwrap_or(Decimal::ZERO);

        let fee_asset = resp
            .get("fills")
            .and_then(|f| f.as_array())
            .and_then(|fills| fills.first())
            .and_then(|f| f["commissionAsset"].as_str())
            .unwrap_or("")
            .to_string();

        let status = resp["status"].as_str().unwrap_or("UNKNOWN").to_string();

        let timestamp = resp["transactTime"]
            .as_u64()
            .or(resp["updateTime"].as_u64())
            .unwrap_or(0);

        Ok(OrderResult {
            id,
            symbol,
            side,
            order_type,
            time_in_force,
            amount_requested,
            amount_filled,
            avg_fill_price,
            fee: fee_val,
            fee_asset,
            status,
            timestamp,
        })
    }

    async fn check_rate_limit(&self, weight: u32) {
        let limiter = self.rate_limiter.lock().await;
        if !limiter.acquire(weight) {
            warn!(weight, "Rate limit reached, waiting");
            drop(limiter);
            let limiter = self.rate_limiter.lock().await;
            limiter.wait_and_acquire(weight);
        }
    }

    pub fn config(&self) -> &BinanceConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sign_query_deterministic() {
        let sig1 = sign_query("symbol=ETHUSDT&timestamp=1234", "testsecret");
        let sig2 = sign_query("symbol=ETHUSDT&timestamp=1234", "testsecret");
        assert_eq!(sig1, sig2);
        assert!(!sig1.is_empty());
    }

    #[test]
    fn test_sign_query_different_keys() {
        let sig1 = sign_query("test", "key1");
        let sig2 = sign_query("test", "key2");
        assert_ne!(sig1, sig2);
    }

    #[test]
    fn test_parse_decimal_valid() {
        let val = serde_json::Value::String("123.456".to_string());
        let d = ExchangeClient::parse_decimal(&val).unwrap();
        assert_eq!(d, Decimal::from_str_exact("123.456").unwrap());
    }

    #[test]
    fn test_parse_decimal_zero() {
        let val = serde_json::Value::String("0.00000000".to_string());
        let d = ExchangeClient::parse_decimal(&val).unwrap();
        assert_eq!(d, Decimal::ZERO);
    }

    #[test]
    fn test_parse_decimal_invalid() {
        let val = serde_json::Value::String("not_a_number".to_string());
        assert!(ExchangeClient::parse_decimal(&val).is_err());
    }
}
