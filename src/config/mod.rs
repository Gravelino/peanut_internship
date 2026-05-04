pub const ARBITRUM_CHAIN_ID: u64 = 42_161;
pub const ARBITRUM_PUBLIC_RPC_URL: &str = "https://arb1.arbitrum.io/rpc";
pub const ARBITRUM_UNISWAP_V2_ROUTER: &str = "0x4752ba5dbc23f44d87826276bf6fd6b1c372ad24";
pub const ARBITRUM_UNISWAP_V2_FACTORY: &str = "0xf1D7CC64Fb4452F05c498126312eBE29f30Fbcf9";
pub const ARBITRUM_SUSHISWAP_V2_ROUTER: &str = "0x1b02dA8Cb0d097eB8D57A175b88c7D8b47997506";
pub const ARBITRUM_SUSHISWAP_V2_FACTORY: &str = "0xc35DADB65012eC5796536bD9864eD8773aBc74C4";
pub const ARBITRUM_WETH: &str = "0x82aF49447D8a07e3bd95BD0d56f35241523fBab1";
pub const ARBITRUM_USDC: &str = "0xaf88d065e77c8cC2239327C5EDb3A432268e5831";

pub fn env_production_enabled() -> bool {
    std::env::var("PRODUCTION")
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arbitrum_constants_are_set() {
        assert_eq!(ARBITRUM_CHAIN_ID, 42_161);
        assert!(ARBITRUM_PUBLIC_RPC_URL.starts_with("https://"));
        assert!(ARBITRUM_WETH.starts_with("0x"));
        assert!(ARBITRUM_USDC.starts_with("0x"));
    }
}
