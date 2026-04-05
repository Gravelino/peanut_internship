use std::collections::HashMap;
use std::sync::OnceLock;

/// Returns a map of 4-byte function selectors to their human-readable signatures.
pub fn known_selectors() -> &'static HashMap<String, String> {
    static SELECTORS: OnceLock<HashMap<String, String>> = OnceLock::new();

    SELECTORS.get_or_init(|| {
        HashMap::from([
            // ERC-20
            ("0xa9059cbb".into(), "transfer(address,uint256)".into()),
            ("0x095ea7b3".into(), "approve(address,uint256)".into()),
            ("0x23b872dd".into(), "transferFrom(address,address,uint256)".into()),
            ("0x70a08231".into(), "balanceOf(address)".into()),
            ("0xdd62ed3e".into(), "allowance(address,address)".into()),
            
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
            
            // Common
            ("0x1249c58b".into(), "mint(address,uint256)".into()),
            ("0x42966910".into(), "burn(uint256)".into()),
        ])
    })
}

/// Topic hash for the ERC-20 `Transfer(address,address,uint256)` event.
pub const TRANSFER_TOPIC: &str = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
/// Topic hash for the ERC-20 `Approval(address,address,uint256)` event.
pub const APPROVAL_TOPIC: &str = "0x8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925";
/// Topic hash for the Uniswap V2 `Swap(address,uint256,uint256,uint256,uint256,address)` event.
pub const SWAP_V2_TOPIC: &str = "0xd78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822";
/// Topic hash for the Uniswap V2 `Sync(uint112,uint112)` event.
pub const SYNC_TOPIC: &str = "0x1c411e9a96e071241c2f21f7726b17ae89e3cab4c78be50e062b03a9fffbbad1";
/// Topic hash for the Uniswap V3 `Swap(address,address,int256,int256,uint160,uint128,int24)` event.
pub const SWAP_V3_TOPIC: &str = "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67";
/// Topic hash for the WETH `Deposit(address,uint256)` event.
pub const WETH_DEPOSIT_TOPIC: &str = "0xe1fffcc4923d04b559f4d29a8bfc6cda04eb5b0d3c460751c2402c5c5cc9109c";
/// Topic hash for the WETH `Withdrawal(address,uint256)` event.
pub const WETH_WITHDRAWAL_TOPIC: &str = "0x7fcf532c15f0a6dbed9992d9921443650d7e6235f29174924f928cc2ac818eb";
/// Alternative topic hash for the WETH `Withdrawal(address,uint256)` event.
pub const WETH_WITHDRAWAL_ALT_TOPIC: &str = "0x27f12abfe35860a9a927b465bb3d4a9c23c8428174b83f278fe45ed7b4da2662";

/// Decodes an Ethereum event topic hash into a human-readable event name.
pub fn decode_event_topic(topic: &str) -> &'static str {
    match topic {
        TRANSFER_TOPIC => "Transfer(address,address,uint256)",
        APPROVAL_TOPIC => "Approval(address,address,uint256)",
        SWAP_V2_TOPIC => "Swap(address,uint256,uint256,uint256,uint256,address) [Uniswap V2]",
        SYNC_TOPIC => "Sync(uint112,uint112)",
        SWAP_V3_TOPIC => "Swap(address,address,int256,int256,uint160,uint128,int24) [Uniswap V3]",
        WETH_DEPOSIT_TOPIC => "Deposit(address,uint256) [WETH]",
        WETH_WITHDRAWAL_TOPIC => "Withdrawal(address,uint256) [WETH]",
        WETH_WITHDRAWAL_ALT_TOPIC => "Withdrawal(address,uint256) [Wrapped ETH/Alternative]",
        _ => "Unknown",
    }
}
