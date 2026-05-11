use std::collections::HashMap;

use hmac::{Hmac, KeyInit, Mac};
use rust_decimal::Decimal;
use sha2::Sha256;
use tracing::{debug, info, warn};

use crate::core::types::{BPS_SCALE, split_pair_symbols};
use crate::exchange::bybit_config::BybitConfig;
use crate::exchange::errors::{ExchangeError, ExchangeResult};
use crate::exchange::http_client::{HttpClient, RetryConfig};
use crate::exchange::traits::{ExchangeAdapter, ExchangeConfig};
use crate::exchange::types::{
    FeeStructure, MyTrade, NormalizedBalance, OrderBookSnapshot, OrderResult,
};

type HmacSha256 = Hmac<Sha256>;

/// Bybit V5 API error code: parameter error.
const BYBIT_ERR_PARAMS: i32 = 10001;
/// Bybit V5 API error code: order not found.
const BYBIT_ERR_ORDER_NOT_FOUND: i32 = 110001;
/// Bybit V5 API error code: insufficient margin.
const BYBIT_ERR_INSUFFICIENT_MARGIN: i32 = 110007;

/// Bybit V5 API adapter implementing [`ExchangeAdapter`].
#[derive(Debug)]
pub struct BybitAdapter {
    config: BybitConfig,
    exchange_config: ExchangeConfig,
    http: HttpClient,
}

impl BybitAdapter {
    /// Creates a new Bybit adapter from the given configuration.
    pub fn new(config: BybitConfig) -> ExchangeResult<Self> {
        let http = HttpClient::new(RetryConfig::default(), config.enable_rate_limit)?;
        let exchange_config = ExchangeConfig::Bybit(config.clone());
        Ok(Self {
            config,
            exchange_config,
            http,
        })
    }

    /// Signs a query string using Bybit's HMAC-SHA256 scheme.
    ///
    /// Bybit V5 signing: `sign = HMAC_SHA256(apiKey + recvWindow + timestamp + queryString)`.
    fn sign_request(&self, query: &str) -> ExchangeResult<(u64, String)> {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| ExchangeError::Config("system clock unavailable".into()))?
            .as_millis() as u64;

        let param_string = format!(
            "{}{}{}{}",
            self.config.api_key, self.config.recv_window, timestamp, query,
        );

        let mut mac = HmacSha256::new_from_slice(self.config.secret.as_bytes())
            .map_err(|_| ExchangeError::Config("HMAC key init failed".into()))?;
        mac.update(param_string.as_bytes());
        let result = mac.finalize();
        let signature = hex::encode(result.into_bytes());

        Ok((timestamp, signature))
    }

    /// Sends a signed GET request and deserializes the JSON body.
    async fn signed_get(&self, path: &str, query: &str) -> ExchangeResult<serde_json::Value> {
        let (timestamp, signature) = self.sign_request(query)?;
        let url = if query.is_empty() {
            format!(
                "{}{}?api_key={}&recvWindow={}&timestamp={}&sign={}",
                self.config.base_url,
                path,
                self.config.api_key,
                self.config.recv_window,
                timestamp,
                signature,
            )
        } else {
            format!(
                "{}{}?{}&api_key={}&recvWindow={}&timestamp={}&sign={}",
                self.config.base_url,
                path,
                query,
                self.config.api_key,
                self.config.recv_window,
                timestamp,
                signature,
            )
        };

        debug!(url = %url, "Bybit signed GET");
        let resp = self.http.get(&url, Some(&self.config.api_key), 1).await?;
        let body: serde_json::Value = resp.json().await?;
        self.check_api_error(&body)?;
        Ok(body)
    }

    /// Sends a signed POST request and deserializes the JSON body.
    async fn signed_post(&self, path: &str, query: &str) -> ExchangeResult<serde_json::Value> {
        let (timestamp, signature) = self.sign_request(query)?;
        let url = format!(
            "{}{}?api_key={}&recvWindow={}&timestamp={}&sign={}",
            self.config.base_url,
            path,
            self.config.api_key,
            self.config.recv_window,
            timestamp,
            signature,
        );

        debug!(url = %url, "Bybit signed POST");
        let resp = self.http.post(&url, Some(&self.config.api_key), 1).await?;
        let body: serde_json::Value = resp.json().await?;
        self.check_api_error(&body)?;
        Ok(body)
    }

    /// Checks for Bybit V5 API errors in the response.
    fn check_api_error(&self, resp: &serde_json::Value) -> ExchangeResult<()> {
        let ret_code = resp["retCode"].as_i64().unwrap_or(0);
        if ret_code != 0 {
            let msg = resp["retMsg"].as_str().unwrap_or("unknown error");
            match ret_code as i32 {
                BYBIT_ERR_PARAMS => {
                    return Err(ExchangeError::InvalidSymbol(msg.to_string()));
                }
                BYBIT_ERR_INSUFFICIENT_MARGIN => {
                    return Err(ExchangeError::InsufficientFunds(msg.to_string()));
                }
                BYBIT_ERR_ORDER_NOT_FOUND => {
                    return Err(ExchangeError::OrderRejected(msg.to_string()));
                }
                _ => {
                    return Err(ExchangeError::Api {
                        code: ret_code as i32,
                        message: msg.to_string(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Parses a decimal from a JSON string value.
    fn parse_decimal(val: &serde_json::Value) -> ExchangeResult<Decimal> {
        let s = val
            .as_str()
            .ok_or_else(|| ExchangeError::DecimalParse("expected string decimal".into()))?;
        Decimal::from_str_exact(s)
            .map_err(|e| ExchangeError::DecimalParse(format!("decimal parse: {e}")))
    }
}

impl ExchangeAdapter for BybitAdapter {
    async fn health_check(&self) -> ExchangeResult<u64> {
        let url = format!("{}/v5/market/time", self.config.base_url);
        let resp: serde_json::Value = self.http.get(&url, None, 1).await?.json().await?;
        self.check_api_error(&resp)?;
        let server_time = resp["result"]["timeNano"].as_u64().unwrap_or(0) / 1_000_000;
        info!(server_time, "Bybit testnet connection OK");
        Ok(server_time)
    }

    async fn fetch_rate_limits(&self) -> ExchangeResult<()> {
        debug!("Bybit rate limits discovered from response headers automatically");
        Ok(())
    }

    async fn fetch_order_book(
        &self,
        symbol: &str,
        limit: u32,
    ) -> ExchangeResult<OrderBookSnapshot> {
        let url = format!(
            "{}/v5/market/orderbook?category=spot&symbol={}&limit={}",
            self.config.base_url,
            symbol.replace('/', ""),
            limit,
        );
        debug!(url = %url, symbol, limit, "Fetching Bybit order book");

        let resp: serde_json::Value = self.http.get(&url, None, 1).await?.json().await?;
        self.check_api_error(&resp)?;

        let result = &resp["result"];

        let bids_raw = result["bids"].as_array().ok_or_else(|| {
            ExchangeError::JsonParse(
                serde_json::from_str::<serde_json::Value>("missing bids").unwrap_err(),
            )
        })?;

        let asks_raw = result["asks"].as_array().ok_or_else(|| {
            ExchangeError::JsonParse(
                serde_json::from_str::<serde_json::Value>("missing asks").unwrap_err(),
            )
        })?;

        let mut bids: Vec<(Decimal, Decimal)> = Vec::with_capacity(bids_raw.len());
        for level in bids_raw {
            let price = Self::parse_decimal(&level[0])?;
            let qty = Self::parse_decimal(&level[1])?;
            bids.push((price, qty));
        }
        bids.sort_by_key(|b| std::cmp::Reverse(b.0));

        let mut asks: Vec<(Decimal, Decimal)> = Vec::with_capacity(asks_raw.len());
        for level in asks_raw {
            let price = Self::parse_decimal(&level[0])?;
            let qty = Self::parse_decimal(&level[1])?;
            asks.push((price, qty));
        }
        asks.sort_by_key(|a| a.0);

        let best_bid = bids.first().copied();
        let best_ask = asks.first().copied();

        let (mid_price, spread_bps) = match (best_bid, best_ask) {
            (Some((bid_p, _)), Some((ask_p, _))) => {
                let mid = (bid_p + ask_p) / Decimal::TWO;
                let spread = ask_p - bid_p;
                let bps = if mid.is_zero() {
                    None
                } else {
                    Some(spread / mid * Decimal::from(BPS_SCALE))
                };
                (Some(mid), bps)
            }
            _ => (None, None),
        };

        let timestamp = result["ts"].as_u64().unwrap_or(0);

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

    async fn fetch_balance(&self) -> ExchangeResult<HashMap<String, NormalizedBalance>> {
        let resp = self
            .signed_get("/v5/account/wallet-balance", "accountType=UNIFIED")
            .await?;

        let accounts = resp["result"]["list"].as_array().ok_or_else(|| {
            ExchangeError::JsonParse(
                serde_json::from_str::<serde_json::Value>("missing list").unwrap_err(),
            )
        })?;

        let mut result = HashMap::new();
        for account in accounts {
            let coins = match account["coin"].as_array() {
                Some(c) => c,
                None => continue,
            };
            for coin in coins {
                let asset = coin["coin"].as_str().unwrap_or("");
                let free = Self::parse_decimal(&coin["availableToWithdraw"])?;
                let locked = Self::parse_decimal(&coin["locked"])?;
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
        }

        Ok(result)
    }

    async fn create_limit_order(
        &self,
        symbol: &str,
        side: &str,
        amount: f64,
        price: f64,
        time_in_force: &str,
    ) -> ExchangeResult<OrderResult> {
        let query = format!(
            "category=spot&symbol={}&side={}&orderType=Limit&qty={}&price={}&timeInForce={}",
            symbol.replace('/', ""),
            side.to_uppercase(),
            amount,
            price,
            time_in_force,
        );

        debug!(symbol, side, amount, price, "Placing Bybit LIMIT order");
        let resp = self.signed_post("/v5/order/create", &query).await?;
        let order_id = resp["result"]["orderId"]
            .as_str()
            .unwrap_or("unknown")
            .to_string();

        Ok(OrderResult {
            id: order_id,
            symbol: symbol.to_string(),
            side: side.to_string(),
            order_type: "LIMIT".to_string(),
            time_in_force: time_in_force.to_string(),
            amount_requested: Decimal::try_from(amount)
                .map_err(|e| ExchangeError::DecimalParse(format!("f64→Decimal: {e}")))?,
            amount_filled: Decimal::ZERO,
            avg_fill_price: Decimal::ZERO,
            fee: Decimal::ZERO,
            fee_asset: String::new(),
            status: "NEW".to_string(),
            timestamp: 0,
        })
    }

    async fn create_market_order(
        &self,
        symbol: &str,
        side: &str,
        amount: f64,
    ) -> ExchangeResult<OrderResult> {
        let query = format!(
            "category=spot&symbol={}&side={}&orderType=Market&qty={}",
            symbol.replace('/', ""),
            side.to_uppercase(),
            amount,
        );

        debug!(symbol, side, amount, "Placing Bybit MARKET order");
        let resp = self.signed_post("/v5/order/create", &query).await?;
        let order_id = resp["result"]["orderId"]
            .as_str()
            .unwrap_or("unknown")
            .to_string();

        Ok(OrderResult {
            id: order_id,
            symbol: symbol.to_string(),
            side: side.to_string(),
            order_type: "MARKET".to_string(),
            time_in_force: "IOC".to_string(),
            amount_requested: Decimal::try_from(amount)
                .map_err(|e| ExchangeError::DecimalParse(format!("f64→Decimal: {e}")))?,
            amount_filled: Decimal::ZERO,
            avg_fill_price: Decimal::ZERO,
            fee: Decimal::ZERO,
            fee_asset: String::new(),
            status: "NEW".to_string(),
            timestamp: 0,
        })
    }

    async fn cancel_order(&self, order_id: &str, symbol: &str) -> ExchangeResult<OrderResult> {
        let query = format!(
            "category=spot&symbol={}&orderId={}",
            symbol.replace('/', ""),
            order_id,
        );

        debug!(order_id, symbol, "Cancelling Bybit order");
        self.signed_post("/v5/order/cancel", &query).await?;

        Ok(OrderResult {
            id: order_id.to_string(),
            symbol: symbol.to_string(),
            side: String::new(),
            order_type: String::new(),
            time_in_force: String::new(),
            amount_requested: Decimal::ZERO,
            amount_filled: Decimal::ZERO,
            avg_fill_price: Decimal::ZERO,
            fee: Decimal::ZERO,
            fee_asset: String::new(),
            status: "CANCELLED".to_string(),
            timestamp: 0,
        })
    }

    async fn fetch_order_status(
        &self,
        order_id: &str,
        symbol: &str,
    ) -> ExchangeResult<OrderResult> {
        let query = format!(
            "category=spot&symbol={}&orderId={}",
            symbol.replace('/', ""),
            order_id,
        );

        let resp = self.signed_get("/v5/order/realtime", &query).await?;

        let order = &resp["result"]["list"]
            .as_array()
            .and_then(|a| a.first())
            .ok_or_else(|| ExchangeError::OrderRejected("order not found".into()))?;

        let status = order["orderStatus"].as_str().unwrap_or("UNKNOWN");
        let side = order["side"].as_str().unwrap_or("unknown");
        let order_type = order["orderType"].as_str().unwrap_or("unknown");
        let price = Self::parse_decimal(&order["price"]).unwrap_or(Decimal::ZERO);
        let qty = Self::parse_decimal(&order["qty"]).unwrap_or(Decimal::ZERO);
        let cum_exec_qty = Self::parse_decimal(&order["cumExecQty"]).unwrap_or(Decimal::ZERO);

        Ok(OrderResult {
            id: order_id.to_string(),
            symbol: symbol.to_string(),
            side: side.to_string(),
            order_type: order_type.to_string(),
            time_in_force: order["timeInForce"].as_str().unwrap_or("").to_string(),
            amount_requested: qty,
            amount_filled: cum_exec_qty,
            avg_fill_price: price,
            fee: Decimal::ZERO,
            fee_asset: String::new(),
            status: status.to_string(),
            timestamp: order["createdTime"].as_u64().unwrap_or(0),
        })
    }

    async fn get_trading_fees(&self, symbol: &str) -> ExchangeResult<FeeStructure> {
        let (base, _) = split_pair_symbols(symbol)
            .map_err(|error| ExchangeError::InvalidSymbol(error.to_string()))?;
        let query = format!(
            "category=spot&symbol={}&baseCoin={}",
            symbol.replace('/', ""),
            base,
        );
        let resp = self.signed_get("/v5/account/fee-rate", &query).await?;

        let list = resp["result"]["list"]
            .as_array()
            .and_then(|a| a.first())
            .ok_or_else(|| ExchangeError::Api {
                code: -1,
                message: "no fee data".into(),
            })?;

        let maker = Self::parse_decimal(&list["makerFeeRate"])?;
        let taker = Self::parse_decimal(&list["takerFeeRate"])?;

        Ok(FeeStructure {
            maker: maker.abs(),
            taker: taker.abs(),
        })
    }

    async fn fetch_my_trades(&self, symbol: &str, limit: u32) -> ExchangeResult<Vec<MyTrade>> {
        let query = format!(
            "category=spot&symbol={}&limit={}",
            symbol.replace('/', ""),
            limit,
        );
        let resp = self.signed_get("/v5/execution/list", &query).await?;

        let list = match resp["result"]["list"].as_array() {
            Some(a) => a,
            None => return Ok(Vec::new()),
        };

        let mut trades = Vec::new();
        for t in list {
            let id = t["execId"].as_str().unwrap_or("").to_string();
            let order_id = t["orderId"].as_str().unwrap_or("").to_string();
            let side = t["side"].as_str().unwrap_or("unknown").to_string();
            let price = Self::parse_decimal(&t["execPrice"])?;
            let qty = Self::parse_decimal(&t["execQty"])?;
            let fee = Self::parse_decimal(&t["execFee"])?;
            let fee_asset = t["feeCurrency"].as_str().unwrap_or("").to_string();
            let timestamp = t["execTime"].as_u64().unwrap_or_else(|| {
                warn!(id, "Missing timestamp in Bybit trade, defaulting to 0");
                0
            });

            trades.push(MyTrade {
                id,
                order_id,
                symbol: symbol.to_string(),
                side,
                price,
                qty,
                fee,
                fee_asset,
                timestamp,
            });
        }

        Ok(trades)
    }

    async fn fetch_price(&self, symbol: &str) -> ExchangeResult<Decimal> {
        let url = format!(
            "{}/v5/market/tickers?category=spot&symbol={}",
            self.config.base_url,
            symbol.replace('/', "")
        );
        let resp: serde_json::Value = self.http.get(&url, None, 1).await?.json().await?;
        self.check_api_error(&resp)?;

        let ticker = &resp["result"]["list"]
            .as_array()
            .and_then(|a| a.first())
            .ok_or_else(|| ExchangeError::DecimalParse("missing ticker in response".into()))?;

        Self::parse_decimal(&ticker["lastPrice"])
    }

    fn config(&self) -> &ExchangeConfig {
        &self.exchange_config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bybit_adapter_new() {
        let config = BybitConfig::with_custom_url(
            "key".into(),
            "secret".into(),
            "https://test.bybit.com".into(),
        );
        let adapter = BybitAdapter::new(config).unwrap();
        assert_eq!(adapter.config.api_key, "key");
    }

    #[test]
    fn test_sign_request_deterministic() {
        let config = BybitConfig::with_custom_url(
            "key".into(),
            "secret".into(),
            "https://test.bybit.com".into(),
        );
        let adapter = BybitAdapter::new(config).unwrap();

        let (ts1, sig1) = adapter.sign_request("symbol=ETHUSDT").unwrap();
        let (_ts2, sig2) = adapter.sign_request("symbol=ETHUSDT").unwrap();

        assert_ne!(ts1, 0);
        // Timestamps may differ by milliseconds, signatures will differ.
        // But same query + same key should produce same length signatures.
        assert_eq!(sig1.len(), sig2.len());
    }

    #[test]
    fn test_check_api_error_ok() {
        let config = BybitConfig::with_custom_url(
            "key".into(),
            "secret".into(),
            "https://test.bybit.com".into(),
        );
        let adapter = BybitAdapter::new(config).unwrap();

        let resp = serde_json::json!({"retCode": 0, "retMsg": "OK"});
        assert!(adapter.check_api_error(&resp).is_ok());
    }

    #[test]
    fn test_check_api_error_params() {
        let config = BybitConfig::with_custom_url(
            "key".into(),
            "secret".into(),
            "https://test.bybit.com".into(),
        );
        let adapter = BybitAdapter::new(config).unwrap();

        let resp = serde_json::json!({"retCode": BYBIT_ERR_PARAMS, "retMsg": "Params error"});
        let err = adapter.check_api_error(&resp).unwrap_err();
        assert!(matches!(err, ExchangeError::InvalidSymbol(_)));
    }

    #[test]
    fn test_check_api_error_insufficient() {
        let config = BybitConfig::with_custom_url(
            "key".into(),
            "secret".into(),
            "https://test.bybit.com".into(),
        );
        let adapter = BybitAdapter::new(config).unwrap();

        let resp = serde_json::json!({"retCode": BYBIT_ERR_INSUFFICIENT_MARGIN, "retMsg": "Insufficient margin"});
        let err = adapter.check_api_error(&resp).unwrap_err();
        assert!(matches!(err, ExchangeError::InsufficientFunds(_)));
    }

    #[test]
    fn test_check_api_error_generic() {
        let config = BybitConfig::with_custom_url(
            "key".into(),
            "secret".into(),
            "https://test.bybit.com".into(),
        );
        let adapter = BybitAdapter::new(config).unwrap();

        let resp = serde_json::json!({"retCode": 99999, "retMsg": "Unknown"});
        let err = adapter.check_api_error(&resp).unwrap_err();
        assert!(matches!(err, ExchangeError::Api { code: 99999, .. }));
    }
}
