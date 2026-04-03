use std::collections::HashMap;
use std::str::FromStr;

use ethers::providers::{Http, Middleware, Provider};
use ethers::types::{H256, U256};
use ethers::utils::format_ether;
use serde_json::Value;

use super::errors::{ChainError, ChainResult};

pub fn known_selectors() -> HashMap<String, String> {
    HashMap::from([
        // ERC-20
        ("0xa9059cbb".into(), "transfer(address,uint256)".into()),
        ("0x095ea7b3".into(), "approve(address,uint256)".into()),
        ("0x23b872dd".into(), "transferFrom(address,address,uint256)".into()),
        // Uniswap V2
        ("0x38ed1739".into(), "swapExactTokensForTokens(uint256,uint256,address[],address,uint256)".into()),
        ("0x7ff36ab5".into(), "swapExactETHForTokens(uint256,address[],address,uint256)".into()),
        ("0x18cbafe5".into(), "swapExactTokensForETH(uint256,uint256,address[],address,uint256)".into()),
        ("0xe8e33700".into(), "addLiquidity(address,address,uint256,uint256,uint256,uint256,address,uint256)".into()),
        ("0xf305d719".into(), "addLiquidityETH(address,uint256,uint256,uint256,address,uint256)".into()),
        ("0xbaa2abde".into(), "removeLiquidity(address,address,uint256,uint256,uint256,address,uint256)".into()),
        ("0x02751cec".into(), "removeLiquidityETH(address,uint256,uint256,uint256,address,uint256)".into()),
        // Uniswap V3
        ("0xac9650d8".into(), "multicall(bytes[])".into()),
        ("0x414bf389".into(), "exactInputSingle((address,address,uint24,address,uint256,uint256,uint160))".into()),
        ("0xc04b8d59".into(), "exactInput((bytes,address,uint256,uint256,uint256))".into()),
        ("0xdb3e2198".into(), "exactOutputSingle((address,address,uint24,address,uint256,uint256,uint160))".into()),
        ("0xf28c0498".into(), "exactOutput((bytes,address,uint256,uint256,uint256))".into()),
    ])
}

const TRANSFER_TOPIC: &str = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
const APPROVAL_TOPIC: &str = "0x8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925";
const SWAP_V2_TOPIC: &str = "0xd78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822";
const SYNC_TOPIC: &str = "0x1c411e9a96e071241c2f21f7726b17ae89e3cab4c78be50e062b03a9fffbbad1";
const SWAP_V3_TOPIC: &str = "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67";

pub fn decode_event_topic(topic: &str) -> &'static str {
    match topic {
        TRANSFER_TOPIC => "Transfer(address,address,uint256)",
        APPROVAL_TOPIC => "Approval(address,address,uint256)",
        SWAP_V2_TOPIC => "Swap(address,uint256,uint256,uint256,uint256,address) [Uniswap V2]",
        SYNC_TOPIC => "Sync(uint112,uint112)",
        SWAP_V3_TOPIC => "Swap(address,address,int256,int256,uint160,uint128,int24) [Uniswap V3]",
        _ => "Unknown",
    }
}

pub fn extract_selector(input: &[u8]) -> Option<String> {
    if input.len() < 4 {
        return None;
    }
    Some(format!("0x{}", hex::encode(&input[..4])))
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

    if log.topics.len() >= 3 {
        let topic0 = format!("0x{}", hex::encode(log.topics[0].as_bytes()));
        if topic0 == TRANSFER_TOPIC {
            let from = format!("0x{}", hex::encode(&log.topics[1].as_bytes()[12..]));
            let to = format!("0x{}", hex::encode(&log.topics[2].as_bytes()[12..]));
            out.push_str(&format!("    From:    {from}\n"));
            out.push_str(&format!("    To:      {to}\n"));
            if log.data.len() >= 32 {
                let value = U256::from_big_endian(&log.data.0[..32]);
                out.push_str(&format!("    Value:   {value}\n"));
            }
        }
    }

    out.push_str(&format!("    Address: {:?}\n", log.address));
    out
}

fn try_get_revert_reason(
    provider: &Provider<Http>,
    rt: &tokio::runtime::Runtime,
    tx: &ethers::types::Transaction,
) -> Option<String> {
    let mut call = ethers::types::transaction::eip2718::TypedTransaction::Legacy(Default::default());
    if let ethers::types::transaction::eip2718::TypedTransaction::Legacy(ref mut inner) = call {
        inner.from = Some(tx.from);
        inner.to = tx.to.map(Into::into);
        inner.value = Some(tx.value);
        inner.data = Some(tx.input.clone());
        inner.gas = Some(tx.gas);
        inner.gas_price = tx.gas_price;
    }

    let block = tx.block_number.map(|n| ethers::types::BlockId::Number(n.into()));

    match rt.block_on(provider.call(&call, block)) {
        Err(ethers::providers::ProviderError::JsonRpcClientError(e)) => {
            let msg = e.to_string();
            if msg.contains("revert") || msg.contains("execution reverted") {
                Some(msg)
            } else {
                Some(format!("call failed: {}", truncate(&msg, 200)))
            }
        }
        Err(e) => Some(format!("call failed: {}", truncate(&e.to_string(), 200))),
        Ok(_) => None,
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() > max { &s[..max] } else { s }
}

#[derive(Debug)]
pub struct AnalysisResult {
    pub hash: String,
    pub block: Option<u64>,
    pub timestamp: Option<u64>,
    pub status: String,
    pub from: String,
    pub to: String,
    pub value_eth: String,
    pub gas_limit: String,
    pub gas_used: Option<String>,
    pub effective_gas_price: Option<String>,
    pub tx_fee_eth: Option<String>,
    pub selector: Option<String>,
    pub function_name: String,
    pub events: Vec<String>,
    pub revert_reason: Option<String>,
}

impl AnalysisResult {
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        out.push_str("Transaction Analysis\n");
        out.push_str("====================\n");
        out.push_str(&format!("Hash:           {}\n", self.hash));
        out.push_str(&format!("Block:          {}\n", self.block.map(|v| v.to_string()).unwrap_or_else(|| "PENDING".into())));
        out.push_str(&format!("Timestamp:      {}\n", self.timestamp.map(|v| v.to_string()).unwrap_or_else(|| "unknown".into())));
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
        out.push_str(&format!("Selector:       {}\n", self.selector.as_deref().unwrap_or("none (plain ETH transfer)")));
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

pub fn analyze_transaction(rpc_url: &str, tx_hash: &str) -> ChainResult<AnalysisResult> {
    let hash = H256::from_str(tx_hash).map_err(|e| ChainError::Other(format!("invalid transaction hash: {e}")))?;
    let provider = Provider::<Http>::try_from(rpc_url).map_err(|e| ChainError::Rpc(e.to_string()))?;
    let rt = tokio::runtime::Runtime::new().map_err(|e| ChainError::Other(e.to_string()))?;

    let tx = rt
        .block_on(provider.get_transaction(hash))
        .map_err(|e| ChainError::Rpc(e.to_string()))?
        .ok_or_else(|| ChainError::Other("transaction not found".into()))?;

    let receipt = rt
        .block_on(provider.get_transaction_receipt(hash))
        .map_err(|e| ChainError::Rpc(e.to_string()))?;

    let block_timestamp = if let Some(block_number) = tx.block_number {
        rt.block_on(provider.get_block(block_number))
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

    let status_str = match receipt.as_ref().and_then(|r| r.status).map(|v| v.as_u64()) {
        Some(1) => "SUCCESS",
        Some(0) => "FAILED",
        _ => "PENDING",
    };

    let mut events = Vec::new();
    if let Some(ref r) = receipt {
        for (i, log) in r.logs.iter().enumerate() {
            events.push(format_log_entry(log, i));
        }
    }

    let revert_reason = if status_str == "FAILED" {
        try_get_revert_reason(&provider, &rt, &tx)
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
        status: status_str.into(),
        from: format!("{:?}", tx.from),
        to: tx.to.map(|v| format!("{:?}", v)).unwrap_or_else(|| "contract creation".into()),
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

pub fn analyze_transaction_text(rpc_url: &str, tx_hash: &str) -> ChainResult<String> {
    analyze_transaction(rpc_url, tx_hash).map(|r| r.to_text())
}
