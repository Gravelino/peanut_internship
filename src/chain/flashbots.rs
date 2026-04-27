use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

use crate::core::wallet::WalletManager;

#[derive(Debug, Clone)]
pub struct FlashbotsConfig {
    pub relay_url: String,
    pub target_block_offset: u64,
    pub max_blocks_to_try: u64,
    pub simulation_timeout_secs: u64,
    pub inclusion_timeout_secs: u64,
}

impl Default for FlashbotsConfig {
    fn default() -> Self {
        Self {
            relay_url: "https://relay.flashbots.net".to_string(),
            target_block_offset: 1,
            max_blocks_to_try: 3,
            simulation_timeout_secs: 5,
            inclusion_timeout_secs: 30,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleTx {
    pub raw_tx: String,
    pub tx_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleRequest {
    pub txs: Vec<BundleTx>,
    pub target_block: u64,
    pub min_timestamp: Option<u64>,
    pub max_timestamp: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleSubmission {
    pub bundle_hash: String,
    pub tx_hashes: Vec<String>,
    pub target_block: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BundleStatus {
    Simulated,
    Submitted,
    Included { block_number: u64 },
    NotIncluded,
    SimulationFailed { reason: String },
    RelayRejected { reason: String },
    TimedOut,
}

#[derive(Debug, Error)]
pub enum BundleError {
    #[error("relay http error: {0}")]
    Http(String),
    #[error("relay rpc error: {0}")]
    Rpc(String),
    #[error("relay decode error: {0}")]
    Decode(String),
    #[error("bundle simulation failed: {0}")]
    Simulation(String),
    #[error("bundle rejected: {0}")]
    Rejected(String),
    #[error("bundle not included")]
    NotIncluded,
    #[error("bundle timed out")]
    TimedOut,
    #[error("auth signing failed: {0}")]
    Auth(String),
}

pub type BundleResult<T> = Result<T, BundleError>;

#[async_trait]
pub trait BundleRelay: Send + Sync + std::fmt::Debug {
    async fn simulate_bundle(&self, request: &BundleRequest) -> BundleResult<()>;
    async fn send_bundle(&self, request: &BundleRequest) -> BundleResult<BundleSubmission>;
}

#[derive(Clone)]
pub struct FlashbotsRelayClient {
    http: reqwest::Client,
    config: FlashbotsConfig,
    auth_wallet: WalletManager,
}

impl std::fmt::Debug for FlashbotsRelayClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlashbotsRelayClient")
            .field("relay_url", &self.config.relay_url)
            .finish_non_exhaustive()
    }
}

impl FlashbotsRelayClient {
    pub fn new(config: FlashbotsConfig, auth_wallet: WalletManager) -> Self {
        Self {
            http: reqwest::Client::new(),
            config,
            auth_wallet,
        }
    }

    pub fn config(&self) -> &FlashbotsConfig {
        &self.config
    }

    pub fn simulation_payload(request: &BundleRequest) -> Value {
        json!({
            "txs": request.txs.iter().map(|tx| tx.raw_tx.clone()).collect::<Vec<_>>(),
            "blockNumber": hex_block(request.target_block),
            "stateBlockNumber": "latest",
            "timestamp": request.max_timestamp,
        })
    }

    pub fn send_payload(request: &BundleRequest) -> Value {
        let mut payload = json!({
            "txs": request.txs.iter().map(|tx| tx.raw_tx.clone()).collect::<Vec<_>>(),
            "blockNumber": hex_block(request.target_block),
        });
        if let Some(min) = request.min_timestamp {
            payload["minTimestamp"] = json!(min);
        }
        if let Some(max) = request.max_timestamp {
            payload["maxTimestamp"] = json!(max);
        }
        payload
    }

    async fn post_rpc(&self, method: &str, params: Value) -> BundleResult<Value> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": [params],
        });
        let body_text =
            serde_json::to_string(&body).map_err(|e| BundleError::Decode(e.to_string()))?;
        let header = self.auth_header(&body_text).await?;
        let response = self
            .http
            .post(&self.config.relay_url)
            .header("content-type", "application/json")
            .header("X-Flashbots-Signature", header)
            .body(body_text)
            .send()
            .await
            .map_err(|e| BundleError::Http(e.to_string()))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| BundleError::Http(e.to_string()))?;
        if !status.is_success() {
            return Err(BundleError::Http(format!("status {status}: {text}")));
        }
        let value: Value =
            serde_json::from_str(&text).map_err(|e| BundleError::Decode(e.to_string()))?;
        if let Some(error) = value.get("error") {
            return Err(BundleError::Rpc(error.to_string()));
        }
        value
            .get("result")
            .cloned()
            .ok_or_else(|| BundleError::Decode("missing result".into()))
    }

    async fn auth_header(&self, body: &str) -> BundleResult<String> {
        let digest = ethers::utils::id(body);
        let signature = self
            .auth_wallet
            .sign_message_bytes(&digest)
            .await
            .map_err(|e| BundleError::Auth(e.to_string()))?;
        Ok(format!("{}:{}", self.auth_wallet.address(), signature))
    }
}

#[async_trait]
impl BundleRelay for FlashbotsRelayClient {
    async fn simulate_bundle(&self, request: &BundleRequest) -> BundleResult<()> {
        let result = self
            .post_rpc("eth_callBundle", Self::simulation_payload(request))
            .await?;
        if let Some(error) = result.get("error") {
            return Err(BundleError::Simulation(error.to_string()));
        }
        if let Some(results) = result.get("results").and_then(Value::as_array) {
            for tx in results {
                if let Some(error) = tx.get("error") {
                    return Err(BundleError::Simulation(error.to_string()));
                }
                if let Some(revert) = tx.get("revert")
                    && !revert.is_null()
                {
                    return Err(BundleError::Simulation(revert.to_string()));
                }
            }
        }
        Ok(())
    }

    async fn send_bundle(&self, request: &BundleRequest) -> BundleResult<BundleSubmission> {
        let result = self
            .post_rpc("eth_sendBundle", Self::send_payload(request))
            .await?;
        let bundle_hash = result
            .get("bundleHash")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if bundle_hash.is_empty() {
            return Err(BundleError::Rejected(format!(
                "missing bundleHash: {result}"
            )));
        }
        Ok(BundleSubmission {
            bundle_hash,
            tx_hashes: request.txs.iter().map(|tx| tx.tx_hash.clone()).collect(),
            target_block: request.target_block,
        })
    }
}

fn hex_block(block: u64) -> String {
    format!("0x{block:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_send_payload_with_hex_block() {
        let request = BundleRequest {
            txs: vec![BundleTx {
                raw_tx: "0xabc".into(),
                tx_hash: "0xdef".into(),
            }],
            target_block: 16,
            min_timestamp: Some(100),
            max_timestamp: Some(200),
        };
        let payload = FlashbotsRelayClient::send_payload(&request);
        assert_eq!(payload["blockNumber"], "0x10");
        assert_eq!(payload["txs"][0], "0xabc");
        assert_eq!(payload["minTimestamp"], 100);
        assert_eq!(payload["maxTimestamp"], 200);
    }

    #[test]
    fn builds_simulation_payload() {
        let request = BundleRequest {
            txs: vec![BundleTx {
                raw_tx: "0xabc".into(),
                tx_hash: "0xdef".into(),
            }],
            target_block: 255,
            min_timestamp: None,
            max_timestamp: None,
        };
        let payload = FlashbotsRelayClient::simulation_payload(&request);
        assert_eq!(payload["blockNumber"], "0xff");
        assert_eq!(payload["stateBlockNumber"], "latest");
        assert_eq!(payload["txs"][0], "0xabc");
    }
}
