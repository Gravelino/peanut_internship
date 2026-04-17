use std::collections::HashMap;

use hmac::{Hmac, Mac};
use rust_decimal::Decimal;
use sha2::Sha256;
use tracing::{debug, info, warn};

use crate::core::types::{
    BINANCE_ERR_INSUFFICIENT_FUNDS, BINANCE_ERR_INVALID_QUANTITY, BINANCE_ERR_INVALID_SYMBOL,
    BINANCE_ERR_RATE_LIMIT, BINANCE_RECV_WINDOW_MS, BINANCE_WEIGHT_ACCOUNT,
    BINANCE_WEIGHT_DEPTH_100, BINANCE_WEIGHT_DEPTH_500, BINANCE_WEIGHT_DEPTH_1000,
    BINANCE_WEIGHT_DEPTH_5000, BINANCE_WEIGHT_EXCHANGE_INFO, BINANCE_WEIGHT_MY_TRADES,
    BINANCE_WEIGHT_ORDER, BINANCE_WEIGHT_ORDER_STATUS, BINANCE_WEIGHT_SERVER_TIME,
    BINANCE_WEIGHT_TRADE_FEE,
};
use crate::exchange::config::BinanceConfig;
use crate::exchange::errors::{ExchangeError, ExchangeResult};
use crate::exchange::http_client::{HttpClient, RetryConfig};
use crate::exchange::rate_limiter::{LimitInterval, LimitKey, LimitType};
use crate::exchange::types::{
    FeeStructure, MyTrade, NormalizedBalance, OrderBookSnapshot, OrderResult,
};

type HmacSha256 = Hmac<Sha256>;

fn missing_field_err(field: &str) -> ExchangeError {
    ExchangeError::JsonParse(
        serde_json::from_str::<serde_json::Value>(&format!("missing field: {field}")).unwrap_err(),
    )
}

fn expected_array_err(context: &str) -> ExchangeError {
    ExchangeError::JsonParse(
        serde_json::from_str::<serde_json::Value>(&format!("expected array in {context}"))
            .unwrap_err(),
    )
}

fn sign_query(query: &str, secret: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC can take key of any size");
    mac.update(query.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Client for interacting with the Binance exchange REST API.
///
/// Delegates all HTTP transport, rate-limit tracking, and retry logic
/// to an [`HttpClient`] instance.
#[derive(Debug)]
pub struct ExchangeClient {
    config: BinanceConfig,
    http: HttpClient,
}

impl ExchangeClient {
    /// Creates a new `ExchangeClient` from the given Binance configuration.
    pub fn new(config: BinanceConfig) -> ExchangeResult<Self> {
        let http = HttpClient::new(RetryConfig::default(), config.enable_rate_limit)?;

        Ok(Self { config, http })
    }

    /// Sends a GET request and deserializes the JSON body.
    async fn get_json(&self, url: &str, weight: u32) -> ExchangeResult<serde_json::Value> {
        let response = self
            .http
            .get(url, Some(&self.config.api_key), weight)
            .await?;
        let resp: serde_json::Value = response.json().await?;
        Ok(resp)
    }

    /// Sends a POST request and deserializes the JSON body.
    async fn post_json(&self, url: &str, weight: u32) -> ExchangeResult<serde_json::Value> {
        let response = self
            .http
            .post(url, Some(&self.config.api_key), weight)
            .await?;
        let resp: serde_json::Value = response.json().await?;
        Ok(resp)
    }

    /// Sends a DELETE request and deserializes the JSON body.
    async fn delete_json(&self, url: &str, weight: u32) -> ExchangeResult<serde_json::Value> {
        let response = self
            .http
            .delete(url, Some(&self.config.api_key), weight)
            .await?;
        let resp: serde_json::Value = response.json().await?;
        Ok(resp)
    }

    /// Checks connectivity by fetching the Binance server time.
    pub async fn health_check(&self) -> ExchangeResult<u64> {
        let url = format!("{}/api/v3/time", self.config.base_url);
        debug!(url = %url, "Health check: fetching server time");

        let resp = self.get_json(&url, BINANCE_WEIGHT_SERVER_TIME).await?;

        let server_time = resp["serverTime"]
            .as_u64()
            .ok_or_else(|| ExchangeError::ConnectionCheck("no serverTime in response".into()))?;

        info!(server_time, "Binance testnet connection OK");
        Ok(server_time)
    }

    /// Fetches and applies exchange rate-limit quotas from the exchangeInfo endpoint.
    ///
    /// Registers all three Binance bucket types (REQUEST_WEIGHT, ORDERS, RAW_REQUESTS)
    /// with their respective intervals and limits.
    pub async fn fetch_rate_limits(&self) -> ExchangeResult<()> {
        let url = format!("{}/api/v3/exchangeInfo", self.config.base_url);
        debug!(url = %url, "Fetching rate limits from exchangeInfo");

        let resp = self.get_json(&url, BINANCE_WEIGHT_EXCHANGE_INFO).await?;
        self.check_api_error(&resp)?;

        let rate_limits = resp["rateLimits"]
            .as_array()
            .ok_or_else(|| missing_field_err("rateLimits"))?;

        let mut limiter = self.http.rate_limiter().lock().await;

        for rl in rate_limits {
            let limit_type_str = rl["rateLimitType"].as_str().unwrap_or("");
            let interval_str = rl["interval"].as_str().unwrap_or("");
            let limit = rl["limit"].as_u64().unwrap_or(0) as u32;

            let Some(limit_type) = LimitType::from_binance_str(limit_type_str) else {
                continue;
            };
            let Some(interval) = LimitInterval::from_binance_str(interval_str) else {
                continue;
            };

            if limit > 0 {
                let key = LimitKey::new(limit_type, interval);
                limiter.register_bucket(key, limit);
            }
        }

        Ok(())
    }

    /// Fetches the order book snapshot for the given symbol and depth limit.
    pub async fn fetch_order_book(
        &self,
        symbol: &str,
        limit: u32,
    ) -> ExchangeResult<OrderBookSnapshot> {
        let url = format!(
            "{}/api/v3/depth?symbol={}&limit={}",
            self.config.base_url,
            symbol.replace('/', ""),
            limit,
        );

        debug!(url = %url, symbol, limit, "Fetching order book");

        let weight = match limit {
            ..=100 => BINANCE_WEIGHT_DEPTH_100,
            101..=500 => BINANCE_WEIGHT_DEPTH_500,
            501..=1000 => BINANCE_WEIGHT_DEPTH_1000,
            _ => BINANCE_WEIGHT_DEPTH_5000,
        };

        let resp = self.get_json(&url, weight).await?;
        self.check_api_error(&resp)?;

        let bids_raw = resp["bids"]
            .as_array()
            .ok_or_else(|| missing_field_err("bids"))?;
        let asks_raw = resp["asks"]
            .as_array()
            .ok_or_else(|| missing_field_err("asks"))?;

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
                    None
                } else {
                    Some(spread / mid * Decimal::from(10000))
                };
                (Some(mid), bps)
            }
            _ => (None, None),
        };

        let timestamp = resp["lastUpdateId"].as_u64().unwrap_or_else(|| {
            warn!(
                symbol,
                "Missing lastUpdateId in orderbook response, defaulting to 0"
            );
            0
        });

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

    /// Fetches the account balance for all non-zero assets.
    pub async fn fetch_balance(&self) -> ExchangeResult<HashMap<String, NormalizedBalance>> {
        let query = format!("recvWindow={}", BINANCE_RECV_WINDOW_MS);
        let signed = self.sign_request(&query)?;

        let url = format!(
            "{}/api/v3/account?{}&{}",
            self.config.base_url, query, signed
        );

        debug!(url = %url, "Fetching account balance");

        let resp = self.get_json(&url, BINANCE_WEIGHT_ACCOUNT).await?;
        self.check_api_error(&resp)?;

        let balances_raw = resp["balances"]
            .as_array()
            .ok_or_else(|| missing_field_err("balances"))?;

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

    /// Places a LIMIT GTC (good-til-cancelled) order.
    pub async fn create_limit_gtc_order(
        &self,
        symbol: &str,
        side: &str,
        amount: f64,
        price: f64,
    ) -> ExchangeResult<OrderResult> {
        let query = format!(
            "symbol={}&side={}&type=LIMIT&timeInForce=GTC&quantity={}&price={}&recvWindow={}",
            symbol.replace('/', ""),
            side.to_uppercase(),
            amount,
            price,
            BINANCE_RECV_WINDOW_MS,
        );

        let signed = self.sign_request(&query)?;
        let url = format!("{}/api/v3/order?{}&{}", self.config.base_url, query, signed);

        debug!(symbol, side, amount, price, "Placing LIMIT GTC order");

        let resp = self.post_json(&url, BINANCE_WEIGHT_ORDER).await?;
        self.check_api_error(&resp)?;

        Self::parse_order_result(&resp)
    }

    /// Places a LIMIT IOC (immediate-or-cancel) order.
    pub async fn create_limit_ioc_order(
        &self,
        symbol: &str,
        side: &str,
        amount: f64,
        price: f64,
    ) -> ExchangeResult<OrderResult> {
        let query = format!(
            "symbol={}&side={}&type=LIMIT&timeInForce=IOC&quantity={}&price={}&recvWindow={}",
            symbol.replace('/', ""),
            side.to_uppercase(),
            amount,
            price,
            BINANCE_RECV_WINDOW_MS,
        );

        let signed = self.sign_request(&query)?;
        let url = format!("{}/api/v3/order?{}&{}", self.config.base_url, query, signed);

        debug!(symbol, side, amount, price, "Placing LIMIT IOC order");

        let resp = self.post_json(&url, BINANCE_WEIGHT_ORDER).await?;
        self.check_api_error(&resp)?;

        Self::parse_order_result(&resp)
    }

    /// Places a MARKET order.
    pub async fn create_market_order(
        &self,
        symbol: &str,
        side: &str,
        amount: f64,
    ) -> ExchangeResult<OrderResult> {
        let query = format!(
            "symbol={}&side={}&type=MARKET&quantity={}&recvWindow={}",
            symbol.replace('/', ""),
            side.to_uppercase(),
            amount,
            BINANCE_RECV_WINDOW_MS,
        );

        let signed = self.sign_request(&query)?;
        let url = format!("{}/api/v3/order?{}&{}", self.config.base_url, query, signed);

        debug!(symbol, side, amount, "Placing MARKET order");

        let resp = self.post_json(&url, BINANCE_WEIGHT_ORDER).await?;
        self.check_api_error(&resp)?;

        Self::parse_order_result(&resp)
    }

    /// Cancels an existing order by its ID and symbol.
    pub async fn cancel_order(&self, order_id: &str, symbol: &str) -> ExchangeResult<OrderResult> {
        let query = format!(
            "symbol={}&orderId={}&recvWindow={}",
            symbol.replace('/', ""),
            order_id,
            BINANCE_RECV_WINDOW_MS,
        );

        let signed = self.sign_request(&query)?;
        let url = format!("{}/api/v3/order?{}&{}", self.config.base_url, query, signed);

        debug!(order_id, symbol, "Cancelling order");

        let resp = self.delete_json(&url, BINANCE_WEIGHT_ORDER).await?;
        self.check_api_error(&resp)?;

        Self::parse_order_result(&resp)
    }

    /// Fetches the current status of an order by its ID and symbol.
    pub async fn fetch_order_status(
        &self,
        order_id: &str,
        symbol: &str,
    ) -> ExchangeResult<OrderResult> {
        let query = format!(
            "symbol={}&orderId={}&recvWindow={}",
            symbol.replace('/', ""),
            order_id,
            BINANCE_RECV_WINDOW_MS,
        );

        let signed = self.sign_request(&query)?;
        let url = format!("{}/api/v3/order?{}&{}", self.config.base_url, query, signed);

        debug!(order_id, symbol, "Fetching order status");

        let resp = self.get_json(&url, BINANCE_WEIGHT_ORDER_STATUS).await?;
        self.check_api_error(&resp)?;

        Self::parse_order_result(&resp)
    }

    /// Fetches the maker and taker trading fees for a symbol.
    pub async fn get_trading_fees(&self, symbol: &str) -> ExchangeResult<FeeStructure> {
        let query = format!(
            "symbol={}&recvWindow={}",
            symbol.replace('/', ""),
            BINANCE_RECV_WINDOW_MS
        );
        let signed = self.sign_request(&query)?;
        let url = format!(
            "{}/sapi/v1/asset/tradeFee?{}&{}",
            self.config.base_url, query, signed
        );

        debug!(symbol, "Fetching trading fees");

        let resp = self.get_json(&url, BINANCE_WEIGHT_TRADE_FEE).await?;
        self.check_api_error(&resp)?;

        let arr = resp
            .as_array()
            .ok_or_else(|| expected_array_err("tradeFee"))?;

        let first = arr.first().ok_or_else(|| ExchangeError::Api {
            code: -1,
            message: "no fee data".into(),
        })?;

        let maker = Self::parse_decimal(&first["makerCommission"])?;
        let taker = Self::parse_decimal(&first["takerCommission"])?;

        Ok(FeeStructure { maker, taker })
    }

    /// Fetches recent trades for the given symbol.
    pub async fn fetch_my_trades(&self, symbol: &str, limit: u32) -> ExchangeResult<Vec<MyTrade>> {
        let query = format!(
            "symbol={}&limit={}&recvWindow={}",
            symbol.replace('/', ""),
            limit,
            BINANCE_RECV_WINDOW_MS,
        );
        let signed = self.sign_request(&query)?;
        let url = format!(
            "{}/api/v3/myTrades?{}&{}",
            self.config.base_url, query, signed
        );

        debug!(symbol, limit, "Fetching my trades");

        let resp = self.get_json(&url, BINANCE_WEIGHT_MY_TRADES).await?;
        self.check_api_error(&resp)?;

        let arr = resp
            .as_array()
            .ok_or_else(|| expected_array_err("myTrades"))?;

        let mut trades = Vec::new();
        for t in arr {
            let id = t["id"]
                .as_u64()
                .ok_or_else(|| missing_field_err("trade id"))?
                .to_string();
            let order_id = t["orderId"]
                .as_u64()
                .ok_or_else(|| missing_field_err("orderId in trade"))?
                .to_string();
            let side = t["isBuyer"]
                .as_bool()
                .map(|b| if b { "buy" } else { "sell" })
                .unwrap_or("unknown");
            let price = Self::parse_decimal(&t["price"])?;
            let qty = Self::parse_decimal(&t["qty"])?;
            let fee = Self::parse_decimal(&t["commission"])?;
            let fee_asset = t["commissionAsset"].as_str().unwrap_or("").to_string();
            let timestamp = t["time"].as_u64().unwrap_or_else(|| {
                warn!(id, "Missing timestamp in trade, defaulting to 0");
                0
            });

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

    fn sign_request(&self, query: &str) -> ExchangeResult<String> {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| ExchangeError::Config("system clock unavailable".into()))?
            .as_millis() as u64;

        let query_with_ts = format!("{}&timestamp={}", query, timestamp);
        let signature = sign_query(&query_with_ts, &self.config.secret);
        Ok(format!("timestamp={}&signature={}", timestamp, signature))
    }

    fn check_api_error(&self, resp: &serde_json::Value) -> ExchangeResult<()> {
        if let Some(code) = resp["code"].as_i64()
            && code < 0
        {
            let msg = resp["msg"].as_str().unwrap_or("unknown error");
            match code {
                BINANCE_ERR_RATE_LIMIT => return Err(ExchangeError::RateLimit(msg.to_string())),
                BINANCE_ERR_INSUFFICIENT_FUNDS => {
                    return Err(ExchangeError::InsufficientFunds(msg.to_string()));
                }
                BINANCE_ERR_INVALID_SYMBOL | BINANCE_ERR_INVALID_QUANTITY => {
                    return Err(ExchangeError::InvalidSymbol(msg.to_string()));
                }
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
        let id = resp["orderId"]
            .as_u64()
            .ok_or_else(|| missing_field_err("orderId"))?
            .to_string();
        let symbol = resp["symbol"]
            .as_str()
            .ok_or_else(|| missing_field_err("symbol"))?
            .to_string();
        let side = resp["side"]
            .as_str()
            .ok_or_else(|| missing_field_err("side"))?
            .to_string();
        let order_type = resp["type"]
            .as_str()
            .ok_or_else(|| missing_field_err("type"))?
            .to_string();
        let time_in_force = resp["timeInForce"]
            .as_str()
            .ok_or_else(|| missing_field_err("timeInForce"))?
            .to_string();

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
                    .filter_map(|f| {
                        Self::parse_decimal(&f["commission"])
                            .map_err(|e| {
                                warn!(error = %e, "Failed to parse fill commission, skipping");
                                e
                            })
                            .ok()
                    })
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
            .unwrap_or_else(|| {
                warn!(symbol = %symbol, "Missing transactTime/updateTime in order response, defaulting to 0");
                0
            });

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

    /// Returns a reference to the underlying Binance configuration.
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

    #[test]
    fn test_parse_decimal_non_string_type() {
        let val = serde_json::Value::Number(123.into());
        assert!(ExchangeClient::parse_decimal(&val).is_err());
    }

    #[test]
    fn test_parse_decimal_null() {
        let val = serde_json::Value::Null;
        assert!(ExchangeClient::parse_decimal(&val).is_err());
    }

    fn make_config() -> BinanceConfig {
        BinanceConfig::with_custom_url(
            "test_key".into(),
            "test_secret".into(),
            "https://testnet.binance.vision".into(),
        )
    }

    #[test]
    fn test_check_api_error_rate_limit() {
        let client = ExchangeClient::new(make_config()).unwrap();
        let resp = serde_json::json!({"code": BINANCE_ERR_RATE_LIMIT, "msg": "Too many requests"});
        let err = client.check_api_error(&resp).unwrap_err();
        assert!(matches!(err, ExchangeError::RateLimit(_)));
    }

    #[test]
    fn test_check_api_error_insufficient_funds() {
        let client = ExchangeClient::new(make_config()).unwrap();
        let resp = serde_json::json!({"code": BINANCE_ERR_INSUFFICIENT_FUNDS, "msg": "Not enough balance"});
        let err = client.check_api_error(&resp).unwrap_err();
        assert!(matches!(err, ExchangeError::InsufficientFunds(_)));
    }

    #[test]
    fn test_check_api_error_invalid_symbol() {
        let client = ExchangeClient::new(make_config()).unwrap();
        let resp = serde_json::json!({"code": BINANCE_ERR_INVALID_SYMBOL, "msg": "Invalid symbol"});
        let err = client.check_api_error(&resp).unwrap_err();
        assert!(matches!(err, ExchangeError::InvalidSymbol(_)));
    }

    #[test]
    fn test_check_api_error_invalid_quantity() {
        let client = ExchangeClient::new(make_config()).unwrap();
        let resp =
            serde_json::json!({"code": BINANCE_ERR_INVALID_QUANTITY, "msg": "Invalid quantity"});
        let err = client.check_api_error(&resp).unwrap_err();
        assert!(matches!(err, ExchangeError::InvalidSymbol(_)));
    }

    #[test]
    fn test_check_api_error_generic_api_error() {
        let client = ExchangeClient::new(make_config()).unwrap();
        let resp = serde_json::json!({"code": -9999, "msg": "Something went wrong"});
        let err = client.check_api_error(&resp).unwrap_err();
        assert!(matches!(err, ExchangeError::Api { code, .. } if code == -9999));
    }

    #[test]
    fn test_check_api_error_positive_code_is_ok() {
        let client = ExchangeClient::new(make_config()).unwrap();
        let resp = serde_json::json!({"code": 200, "msg": "OK"});
        assert!(client.check_api_error(&resp).is_ok());
    }

    #[test]
    fn test_check_api_error_no_code_field_is_ok() {
        let client = ExchangeClient::new(make_config()).unwrap();
        let resp = serde_json::json!({"symbol": "ETHUSDT", "price": "2000"});
        assert!(client.check_api_error(&resp).is_ok());
    }

    #[test]
    fn test_parse_order_result_missing_order_id() {
        let resp = serde_json::json!({
            "symbol": "ETHUSDT",
            "side": "BUY",
            "type": "LIMIT",
            "timeInForce": "GTC",
            "origQty": "1.0",
            "executedQty": "0.0"
        });
        assert!(ExchangeClient::parse_order_result(&resp).is_err());
    }

    #[test]
    fn test_parse_order_result_missing_symbol() {
        let resp = serde_json::json!({
            "orderId": 123,
            "side": "BUY",
            "type": "LIMIT",
            "timeInForce": "GTC",
            "origQty": "1.0",
            "executedQty": "0.0"
        });
        assert!(ExchangeClient::parse_order_result(&resp).is_err());
    }

    #[test]
    fn test_parse_order_result_missing_side() {
        let resp = serde_json::json!({
            "orderId": 123,
            "symbol": "ETHUSDT",
            "type": "LIMIT",
            "timeInForce": "GTC",
            "origQty": "1.0",
            "executedQty": "0.0"
        });
        assert!(ExchangeClient::parse_order_result(&resp).is_err());
    }

    #[test]
    fn test_parse_order_result_missing_type() {
        let resp = serde_json::json!({
            "orderId": 123,
            "symbol": "ETHUSDT",
            "side": "BUY",
            "timeInForce": "GTC",
            "origQty": "1.0",
            "executedQty": "0.0"
        });
        assert!(ExchangeClient::parse_order_result(&resp).is_err());
    }

    #[test]
    fn test_parse_order_result_missing_time_in_force() {
        let resp = serde_json::json!({
            "orderId": 123,
            "symbol": "ETHUSDT",
            "side": "BUY",
            "type": "LIMIT",
            "origQty": "1.0",
            "executedQty": "0.0"
        });
        assert!(ExchangeClient::parse_order_result(&resp).is_err());
    }

    #[test]
    fn test_parse_order_result_valid_no_fills() {
        let resp = serde_json::json!({
            "orderId": 123,
            "symbol": "ETHUSDT",
            "side": "BUY",
            "type": "LIMIT",
            "timeInForce": "GTC",
            "origQty": "1.00000000",
            "executedQty": "0.00000000",
            "status": "NEW",
            "transactTime": 1700000000000u64
        });
        let result = ExchangeClient::parse_order_result(&resp).unwrap();
        assert_eq!(result.id, "123");
        assert_eq!(result.symbol, "ETHUSDT");
        assert_eq!(result.side, "BUY");
        assert_eq!(result.order_type, "LIMIT");
        assert_eq!(result.time_in_force, "GTC");
        assert_eq!(result.amount_requested, Decimal::ONE);
        assert_eq!(result.amount_filled, Decimal::ZERO);
        assert_eq!(result.avg_fill_price, Decimal::ZERO);
        assert_eq!(result.fee, Decimal::ZERO);
    }

    #[test]
    fn test_parse_order_result_with_fills() {
        let resp = serde_json::json!({
            "orderId": 456,
            "symbol": "ETHUSDT",
            "side": "SELL",
            "type": "MARKET",
            "timeInForce": "GTC",
            "origQty": "1.00000000",
            "executedQty": "1.00000000",
            "cummulativeQuoteQty": "2000.50000000",
            "status": "FILLED",
            "transactTime": 1700000000000u64,
            "fills": [
                {"commission": "2.00000000", "commissionAsset": "USDT"},
                {"commission": "0.50000000", "commissionAsset": "BNB"}
            ]
        });
        let result = ExchangeClient::parse_order_result(&resp).unwrap();
        assert_eq!(
            result.avg_fill_price,
            Decimal::from_str_exact("2000.5").unwrap()
        );
        assert_eq!(result.fee, Decimal::from_str_exact("2.5").unwrap());
        assert_eq!(result.fee_asset, "USDT");
    }

    #[test]
    fn test_parse_order_result_bad_decimal_in_orig_qty() {
        let resp = serde_json::json!({
            "orderId": 123,
            "symbol": "ETHUSDT",
            "side": "BUY",
            "type": "LIMIT",
            "timeInForce": "GTC",
            "origQty": "not_a_number",
            "executedQty": "0.00000000"
        });
        assert!(ExchangeClient::parse_order_result(&resp).is_err());
    }

    #[test]
    fn test_exchange_client_new_builds_http() {
        let config = make_config();
        let client = ExchangeClient::new(config).unwrap();
        assert_eq!(client.config.api_key, "test_key");
    }
}
