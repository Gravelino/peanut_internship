use std::str::FromStr;

use ethers::providers::{Http, Middleware, Provider};
use ethers::types::{H256, U256};
use ethers::utils::format_ether;
use serde_json::Value;

use super::errors::{ChainError, ChainResult};
pub use super::selectors::{
    SWAP_V2_TOPIC, SYNC_TOPIC, TRANSFER_TOPIC, decode_event_topic, known_selectors,
};
use crate::core::types::{RECEIPT_STATUS_FAILED, RECEIPT_STATUS_SUCCESS, TransactionStatus};

/// Maximum length for truncated revert reasons in reports.
const REVERT_REASON_MAX_LEN: usize = 200;

/// Length of the Ethereum function selector (4 bytes).
const SELECTOR_LEN: usize = 4;

/// Standard size of a word in the EVM (32 bytes).
const EVM_WORD_LEN: usize = 32;

/// Number of bytes to skip in a 32-byte word to extract a 20-byte address.
const ADDRESS_SKIP_LEN: usize = 12;

/// Minimum topics required for ERC-20 Transfer decoding.
const MIN_TRANSFER_TOPICS: usize = 3;

/// Extracts the 4-byte function selector from transaction input data.
pub fn extract_selector(input: &[u8]) -> Option<String> {
    if input.len() < SELECTOR_LEN {
        return None;
    }
    Some(format!("0x{}", hex::encode(&input[..SELECTOR_LEN])))
}

fn format_log_entry(log: &ethers::types::Log, index: usize) -> String {
    let mut out = String::new();
    out.push_str(&format!("  Log #{index}\n"));

    if let Some(topic0) = log.topics.first() {
        let topic_hex = format!("0x{}", hex::encode(topic0.as_bytes()));
        let event_name = decode_event_topic(&topic_hex);
        out.push_str(&format!("    Event:   {event_name}\n"));
        out.push_str(&format!("    Topic0:  {topic_hex}\n"));
    }

    if log.topics.len() >= MIN_TRANSFER_TOPICS {
        let topic0 = format!("0x{}", hex::encode(log.topics[0].as_bytes()));
        if topic0 == TRANSFER_TOPIC {
            let from = format!(
                "0x{}",
                hex::encode(&log.topics[1].as_bytes()[ADDRESS_SKIP_LEN..])
            );
            let to = format!(
                "0x{}",
                hex::encode(&log.topics[2].as_bytes()[ADDRESS_SKIP_LEN..])
            );
            out.push_str(&format!("    From:    {from}\n"));
            out.push_str(&format!("    To:      {to}\n"));
            if log.data.len() >= EVM_WORD_LEN {
                let value = U256::from_big_endian(&log.data.0[..EVM_WORD_LEN]);
                out.push_str(&format!("    Value:   {value}\n"));
            }
        }
    }

    out.push_str(&format!("    Address: {:?}\n", log.address));
    out
}

async fn try_get_revert_reason(
    provider: &Provider<Http>,
    tx: &ethers::types::Transaction,
) -> Option<String> {
    let mut call =
        ethers::types::transaction::eip2718::TypedTransaction::Legacy(Default::default());
    if let ethers::types::transaction::eip2718::TypedTransaction::Legacy(ref mut inner) = call {
        inner.from = Some(tx.from);
        inner.to = tx.to.map(Into::into);
        inner.value = Some(tx.value);
        inner.data = Some(tx.input.clone());
        inner.gas = Some(tx.gas);
        inner.gas_price = tx.gas_price;
    }

    let block = tx
        .block_number
        .map(|n| ethers::types::BlockId::Number(n.into()));

    match provider.call(&call, block).await {
        Err(ethers::providers::ProviderError::JsonRpcClientError(e)) => {
            let msg = e.to_string();
            if msg.contains("revert") || msg.contains("execution reverted") {
                Some(msg)
            } else {
                Some(format!(
                    "call failed: {}",
                    truncate(&msg, REVERT_REASON_MAX_LEN)
                ))
            }
        }
        Err(e) => Some(format!(
            "call failed: {}",
            truncate(&e.to_string(), REVERT_REASON_MAX_LEN)
        )),
        Ok(_) => None,
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        let mut boundary = max;
        while boundary > 0 && !s.is_char_boundary(boundary) {
            boundary -= 1;
        }
        &s[..boundary]
    }
}

/// Detailed results of a transaction analysis.
#[derive(Debug)]
pub struct AnalysisResult {
    /// Transaction hash.
    pub hash: String,
    /// Block number (None if pending).
    pub block: Option<u64>,
    /// Block timestamp (None if pending/unknown).
    pub timestamp: Option<u64>,
    /// Status (Success, Failed, Pending).
    pub status: TransactionStatus,
    /// Transaction sender.
    pub from: String,
    /// Transaction recipient (or "contract creation").
    pub to: String,
    /// Value transferred in ETH.
    pub value_eth: String,
    /// Gas limit.
    pub gas_limit: String,
    /// Gas used (None if pending).
    pub gas_used: Option<String>,
    /// Effective gas price (None if pending).
    pub effective_gas_price: Option<String>,
    /// Transaction fee in ETH (None if pending).
    pub tx_fee_eth: Option<String>,
    /// Function selector (4 bytes prefix).
    pub selector: Option<String>,
    /// Human-readable function name (e.g., "transfer(address,uint256)").
    pub function_name: String,
    /// List of formatted events/logs.
    pub events: Vec<String>,
    /// Optional revert reason if the transaction failed.
    pub revert_reason: Option<String>,
}

impl AnalysisResult {
    /// Formats the analysis results as a human-readable text report.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        out.push_str("Transaction Analysis\n");
        out.push_str("====================\n");
        out.push_str(&format!("Hash:           {}\n", self.hash));
        out.push_str(&format!(
            "Block:          {}\n",
            self.block
                .map(|v| v.to_string())
                .unwrap_or_else(|| "PENDING".into())
        ));
        out.push_str(&format!(
            "Timestamp:      {}\n",
            self.timestamp
                .map(|v| v.to_string())
                .unwrap_or_else(|| "unknown".into())
        ));
        out.push_str(&format!("Status:         {}\n\n", self.status));

        out.push_str(&format!("From:           {}\n", self.from));
        out.push_str(&format!("To:             {}\n", self.to));
        out.push_str(&format!("Value:          {} ETH\n\n", self.value_eth));

        out.push_str("Gas Analysis\n");
        out.push_str("------------\n");
        out.push_str(&format!("Gas Limit:      {}\n", self.gas_limit));
        if let Some(ref used) = self.gas_used {
            out.push_str(&format!("Gas Used:       {used}\n"));
        }
        if let Some(ref price) = self.effective_gas_price {
            out.push_str(&format!("Effective Price:{price} wei\n"));
        }
        if let Some(ref fee) = self.tx_fee_eth {
            out.push_str(&format!("Transaction Fee:{fee} ETH\n"));
        }
        out.push('\n');

        out.push_str("Function Called\n");
        out.push_str("---------------\n");
        out.push_str(&format!(
            "Selector:       {}\n",
            self.selector
                .as_deref()
                .unwrap_or("none (plain ETH transfer)")
        ));
        out.push_str(&format!("Function:       {}\n\n", self.function_name));

        if !self.events.is_empty() {
            out.push_str("Events / Logs\n");
            out.push_str("-------------\n");
            for event in &self.events {
                out.push_str(event);
            }
            out.push('\n');
        }

        if let Some(ref reason) = self.revert_reason {
            out.push_str("Revert Reason\n");
            out.push_str("-------------\n");
            out.push_str(&format!("{reason}\n"));
        }

        out
    }

    /// Serializes the analysis results to a JSON value.
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "hash": self.hash,
            "block": self.block,
            "timestamp": self.timestamp,
            "status": self.status,
            "from": self.from,
            "to": self.to,
            "value_eth": self.value_eth,
            "gas_limit": self.gas_limit,
            "gas_used": self.gas_used,
            "effective_gas_price": self.effective_gas_price,
            "tx_fee_eth": self.tx_fee_eth,
            "selector": self.selector,
            "function_name": self.function_name,
            "events_count": self.events.len(),
            "revert_reason": self.revert_reason,
        })
    }
}

/// Orchestrates the analysis of an Ethereum transaction.
///
/// Fetches transaction data, receipt, and block details, then decodes
/// the function called and its events.
pub async fn analyze_transaction(
    client: &crate::chain::client::ChainClient,
    tx_hash: &str,
) -> ChainResult<AnalysisResult> {
    let hash = H256::from_str(tx_hash)
        .map_err(|e| ChainError::Other(format!("invalid transaction hash: {e}")))?;

    let provider = client
        .provider()
        .ok_or_else(|| ChainError::Rpc("no provider available".into()))?;

    let tx = provider
        .get_transaction(hash)
        .await
        .map_err(|e| ChainError::Rpc(e.to_string()))?
        .ok_or_else(|| ChainError::Other("transaction not found".into()))?;

    let receipt = provider
        .get_transaction_receipt(hash)
        .await
        .map_err(|e| ChainError::Rpc(e.to_string()))?;

    let block_timestamp = if let Some(block_number) = tx.block_number {
        provider
            .get_block(block_number)
            .await
            .map_err(|e| ChainError::Rpc(e.to_string()))?
            .map(|block| block.timestamp.as_u64())
    } else {
        None
    };

    let selector = extract_selector(&tx.input.0);
    let selectors = known_selectors();
    let function_name = selector
        .as_ref()
        .and_then(|s| selectors.get(s.as_str()).cloned())
        .unwrap_or_else(|| {
            if tx.input.0.is_empty() {
                "ETH transfer (no input data)".into()
            } else {
                format!("unknown ({})", selector.as_deref().unwrap_or("???"))
            }
        });

    let status = match receipt.as_ref().and_then(|r| r.status).map(|v| v.as_u64()) {
        Some(RECEIPT_STATUS_SUCCESS) => TransactionStatus::Success,
        Some(RECEIPT_STATUS_FAILED) => TransactionStatus::Failed,
        _ => TransactionStatus::Pending,
    };

    let mut events = Vec::new();
    if let Some(ref r) = receipt {
        for (i, log) in r.logs.iter().enumerate() {
            events.push(format_log_entry(log, i));
        }
    }

    let revert_reason = if status == TransactionStatus::Failed {
        try_get_revert_reason(&provider, &tx).await
    } else {
        None
    };

    let gas_used = receipt.as_ref().map(|r| r.gas_used.unwrap_or_default());
    let effective_price = receipt.as_ref().and_then(|r| r.effective_gas_price);
    let tx_fee = gas_used.zip(effective_price).map(|(u, p)| u * p);

    Ok(AnalysisResult {
        hash: format!("{:?}", tx.hash),
        block: tx.block_number.map(|n| n.as_u64()),
        timestamp: block_timestamp,
        status,
        from: format!("{:?}", tx.from),
        to: tx
            .to
            .map(|v| format!("{:?}", v))
            .unwrap_or_else(|| "contract creation".into()),
        value_eth: format_ether(tx.value).to_string(),
        gas_limit: tx.gas.to_string(),
        gas_used: gas_used.map(|v| v.to_string()),
        effective_gas_price: effective_price.map(|v| v.to_string()),
        tx_fee_eth: tx_fee.map(|v| format_ether(v).to_string()),
        selector,
        function_name,
        events,
        revert_reason,
    })
}

/// Analyzes a transaction and returns a human-readable text report.
pub async fn analyze_transaction_text(
    client: &crate::chain::client::ChainClient,
    tx_hash: &str,
) -> ChainResult<String> {
    analyze_transaction(client, tx_hash)
        .await
        .map(|r| r.to_text())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_selector_from_transfer_calldata() {
        let data =
            hex::decode("a9059cbb0000000000000000000000001234567890abcdef1234567890abcdef12345678")
                .unwrap();
        assert_eq!(extract_selector(&data), Some("0xa9059cbb".to_string()));
    }

    #[test]
    fn extract_selector_returns_none_for_short_input() {
        assert_eq!(extract_selector(&[0x01, 0x02, 0x03]), None);
        assert_eq!(extract_selector(&[]), None);
    }

    #[test]
    fn extract_selector_works_with_exactly_4_bytes() {
        let data = vec![0x09, 0x5e, 0xa7, 0xb3];
        assert_eq!(extract_selector(&data), Some("0x095ea7b3".to_string()));
    }

    #[test]
    fn known_selectors_contain_erc20_functions() {
        let map = known_selectors();
        assert!(map.get("0xa9059cbb").unwrap().starts_with("transfer"));
        assert!(map.get("0x095ea7b3").unwrap().starts_with("approve"));
        assert!(map.get("0x23b872dd").unwrap().starts_with("transferFrom"));
    }

    #[test]
    fn known_selectors_contain_uniswap_v2_functions() {
        let map = known_selectors();
        assert!(map.contains_key("0x38ed1739"));
        assert!(map.contains_key("0x7ff36ab5"));
        assert!(map.contains_key("0x18cbafe5"));
        assert!(map.contains_key("0xe8e33700"));
        assert!(map.contains_key("0xbaa2abde"));
    }

    #[test]
    fn known_selectors_contain_uniswap_v3_functions() {
        let map = known_selectors();
        assert!(map.contains_key("0xac9650d8"));
        assert!(map.contains_key("0x414bf389"));
        assert!(map.contains_key("0xc04b8d59"));
        assert!(map.contains_key("0xdb3e2198"));
        assert!(map.contains_key("0xf28c0498"));
    }

    #[test]
    fn unknown_selector_is_not_in_map() {
        let map = known_selectors();
        assert!(!map.contains_key("0xdeadbeef"));
    }

    #[test]
    fn decode_transfer_event_topic() {
        let topic = TRANSFER_TOPIC;
        let name = decode_event_topic(topic);
        assert!(name.contains("Transfer"));
    }

    #[test]
    fn decode_swap_v2_event_topic() {
        let topic = SWAP_V2_TOPIC;
        let name = decode_event_topic(topic);
        assert!(name.contains("Swap"));
        assert!(name.contains("V2"));
    }

    #[test]
    fn decode_sync_event_topic() {
        let topic = SYNC_TOPIC;
        let name = decode_event_topic(topic);
        assert!(name.contains("Sync"));
    }

    #[test]
    fn unknown_event_topic_returns_unknown() {
        assert_eq!(
            decode_event_topic(
                "0x0000000000000000000000000000000000000000000000000000000000000000"
            ),
            "Unknown"
        );
    }

    #[tokio::test]
    async fn invalid_tx_hash_returns_clear_error() {
        let client =
            crate::chain::client::ChainClient::new(vec!["http://localhost:1".to_string()], 5, 0)
                .unwrap();
        let result = analyze_transaction(&client, "not-a-hash").await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("invalid") || msg.contains("hash"));
    }
}
