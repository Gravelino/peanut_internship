//! # AMM Pricer
//! Exact Uniswap V2 math using integer arithmetic.
//! Display helpers produce Decimal values.

use ethers::types::U256;
use rust_decimal::Decimal;
use tracing::warn;

use super::errors::{PricingError, PricingResult};
use crate::chain::client::ChainClient;
use crate::core::types::{
    Address, BPS_SCALE, BlockId, DECIMAL_BASE, ETH_DECIMALS, MAINNET_CHAIN_ID, Token, TokenAmount,
    TransactionRequest, WEI_PER_GWEI,
};

/// Default fee in basis points (0.3%).
pub const DEFAULT_FEE_BPS: u32 = 30;

/// Size of an EVM word in bytes.
const EVM_WORD_LEN: usize = 32;

/// Number of bytes in an `address` type (20 bytes).
const ADDRESS_LEN: usize = 20;

/// Byte offset to skip the leading 12 bytes of a 32-byte slot when extracting an address.
const ADDRESS_SKIP_LEN: usize = EVM_WORD_LEN - ADDRESS_LEN;

/// Minimum byte length of a `getReserves()` ABI return (3 EVM words = 96 bytes).
const GET_RESERVES_RETURN_MIN: usize = EVM_WORD_LEN * 3;

/// Minimum byte length of an ABI-encoded string (offset + length + at least 1 word of data).
const ABI_STRING_MIN: usize = EVM_WORD_LEN * 3;

/// Byte offset of the string length field within an ABI-encoded string return.
const ABI_STRING_LEN_OFFSET: usize = EVM_WORD_LEN * 2 - 8;

/// Byte offset where string data starts in an ABI-encoded string return.
const ABI_STRING_DATA_START: usize = EVM_WORD_LEN * 2;

/// Upper bound divisor for binary search: never swap more than half the reserve.
const MAX_SWAP_RESERVE_DIVISOR: u128 = 2;

/// Selector for `getReserves()` on a Uniswap V2 pair contract.
/// keccak256("getReserves()")[..4] = 0x0902f1ac
const GET_RESERVES_SELECTOR: [u8; 4] = [0x09, 0x02, 0xf1, 0xac];

/// Selector for `token0()`.
/// keccak256("token0()")[..4] = 0x0dfe1681
const TOKEN0_SELECTOR: [u8; 4] = [0x0d, 0xfe, 0x16, 0x81];

/// Selector for `token1()`.
/// keccak256("token1()")[..4] = 0xd21220a7
const TOKEN1_SELECTOR: [u8; 4] = [0xd2, 0x12, 0x20, 0xa7];

/// Selector for `decimals()` on an ERC-20 token.
/// keccak256("decimals()")[..4] = 0x313ce567
const DECIMALS_SELECTOR: [u8; 4] = [0x31, 0x3c, 0xe5, 0x67];

/// Selector for `symbol()` on an ERC-20 token.
/// keccak256("symbol()")[..4] = 0x95d89b41
const SYMBOL_SELECTOR: [u8; 4] = [0x95, 0xd8, 0x9b, 0x41];

/// Uniswap V2 liquidity pair. Core calculations use exact integer arithmetic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UniswapV2Pair {
    /// On-chain address of the pair contract.
    pub address: Address,
    /// The lower-address token in the pair.
    pub token0: Token,
    /// The higher-address token in the pair.
    pub token1: Token,
    /// Current reserve for `token0` (raw integer units).
    pub reserve0: u128,
    /// Current reserve for `token1` (raw integer units).
    pub reserve1: u128,
    /// Trading fee in basis points (default: 30 = 0.30%).
    pub fee_bps: u32,
}

impl UniswapV2Pair {
    fn u256_mul_checked(a: U256, b: U256, context: &'static str) -> PricingResult<U256> {
        let (value, overflowed) = a.overflowing_mul(b);
        if overflowed {
            return Err(PricingError::ArithmeticOverflow(context));
        }
        Ok(value)
    }

    fn u256_add_checked(a: U256, b: U256, context: &'static str) -> PricingResult<U256> {
        let (value, overflowed) = a.overflowing_add(b);
        if overflowed {
            return Err(PricingError::ArithmeticOverflow(context));
        }
        Ok(value)
    }

    /// Creates a new `UniswapV2Pair`. Fails if `fee_bps >= 10_000`.
    pub fn new(
        address: Address,
        token0: Token,
        token1: Token,
        reserve0: u128,
        reserve1: u128,
        fee_bps: u32,
    ) -> PricingResult<Self> {
        if fee_bps >= (BPS_SCALE as u32) {
            return Err(PricingError::InvalidFeeBps(fee_bps));
        }
        Ok(Self {
            address,
            token0,
            token1,
            reserve0,
            reserve1,
            fee_bps,
        })
    }

    /// Returns `token0` or `token1` if its address matches the given hex string.
    pub fn token0_if_matches(&self, address: &str) -> Option<Token> {
        let target = Address::new(address).ok()?;
        if self.token0.address == target {
            Some(self.token0.clone())
        } else if self.token1.address == target {
            Some(self.token1.clone())
        } else {
            None
        }
    }

    /// Returns `(reserve_in, reserve_out)` for a swap where `token_in` is sold.
    fn reserves_for(&self, token_in: &Token) -> PricingResult<(u128, u128)> {
        if *token_in == self.token0 {
            Ok((self.reserve0, self.reserve1))
        } else if *token_in == self.token1 {
            Ok((self.reserve1, self.reserve0))
        } else {
            Err(PricingError::UnknownToken(token_in.symbol.clone()))
        }
    }

    fn token_out_for<'a>(&'a self, token_in: &Token) -> PricingResult<&'a Token> {
        if *token_in == self.token0 {
            Ok(&self.token1)
        } else if *token_in == self.token1 {
            Ok(&self.token0)
        } else {
            Err(PricingError::UnknownToken(token_in.symbol.clone()))
        }
    }

    /// Calculates output amount using exact Uniswap V2 integer math and `U256` intermediate products.
    pub fn get_amount_out(&self, amount_in: u128, token_in: &Token) -> PricingResult<u128> {
        if amount_in == 0 {
            return Err(PricingError::ZeroAmountIn);
        }
        let (reserve_in, reserve_out) = self.reserves_for(token_in)?;

        let ain = U256::from(amount_in);
        let r_in = U256::from(reserve_in);
        let r_out = U256::from(reserve_out);
        let bps = U256::from(BPS_SCALE);
        let fee = U256::from(self.fee_bps);

        let amount_in_with_fee = Self::u256_mul_checked(ain, bps - fee, "amount_in_with_fee")?;
        let numerator = Self::u256_mul_checked(amount_in_with_fee, r_out, "amount_out numerator")?;
        let reserve_term =
            Self::u256_mul_checked(r_in, bps, "amount_out denominator reserve term")?;
        let denominator =
            Self::u256_add_checked(reserve_term, amount_in_with_fee, "amount_out denominator")?;

        Ok((numerator / denominator).as_u128())
    }

    /// Calculates required input to receive exact `amount_out` using exact integer math and ceilings.
    pub fn get_amount_in(&self, amount_out: u128, token_out: &Token) -> PricingResult<u128> {
        if amount_out == 0 {
            return Err(PricingError::ZeroAmountOut);
        }

        let token_in = if *token_out == self.token1 {
            &self.token0
        } else if *token_out == self.token0 {
            &self.token1
        } else {
            return Err(PricingError::UnknownToken(token_out.symbol.clone()));
        };

        let (reserve_in, reserve_out) = self.reserves_for(token_in)?;

        if amount_out >= reserve_out {
            return Err(PricingError::InsufficientLiquidity {
                amount_out,
                reserve: reserve_out,
            });
        }

        let aout = U256::from(amount_out);
        let r_in = U256::from(reserve_in);
        let r_out = U256::from(reserve_out);
        let bps = U256::from(BPS_SCALE);
        let fee = U256::from(self.fee_bps);

        let reserve_times_out =
            Self::u256_mul_checked(r_in, aout, "amount_in numerator reserve*amount_out")?;
        let numerator = Self::u256_mul_checked(reserve_times_out, bps, "amount_in numerator")?;
        let denominator = Self::u256_mul_checked(r_out - aout, bps - fee, "amount_in denominator")?;

        Ok((numerator / denominator).as_u128() + 1)
    }

    /// Returns spot price of `token_in` in terms of `token_out` (display-only).
    pub fn get_spot_price(&self, token_in: &Token) -> PricingResult<Decimal> {
        let (reserve_in, reserve_out) = self.reserves_for(token_in)?;
        let token_out = self.token_out_for(token_in)?;

        let scale_in = Decimal::from(DECIMAL_BASE.pow(token_in.decimals as u32));
        let scale_out = Decimal::from(DECIMAL_BASE.pow(token_out.decimals as u32));

        let r_in = Decimal::from(reserve_in) / scale_in;
        let r_out = Decimal::from(reserve_out) / scale_out;

        Ok(r_out / r_in)
    }

    /// Returns execution price for a trade of `amount_in` (`amount_out / amount_in`).
    pub fn get_execution_price(&self, amount_in: u128, token_in: &Token) -> PricingResult<Decimal> {
        let amount_out = self.get_amount_out(amount_in, token_in)?;
        let token_out = self.token_out_for(token_in)?;

        let scale_in = Decimal::from(DECIMAL_BASE.pow(token_in.decimals as u32));
        let scale_out = Decimal::from(DECIMAL_BASE.pow(token_out.decimals as u32));

        let human_in = Decimal::from(amount_in) / scale_in;
        let human_out = Decimal::from(amount_out) / scale_out;

        if human_in.is_zero() {
            return Ok(Decimal::ZERO);
        }
        Ok(human_out / human_in)
    }

    /// Returns price impact measuring slippage from the midprice.
    pub fn get_price_impact(&self, amount_in: u128, token_in: &Token) -> PricingResult<Decimal> {
        let (reserve_in, reserve_out) = self.reserves_for(token_in)?;
        let amount_out = self.get_amount_out(amount_in, token_in)?;

        let ain = U256::from(amount_in);
        let r_in = U256::from(reserve_in);
        let r_out = U256::from(reserve_out);

        let expected = ain * r_out / r_in;
        if expected.is_zero() {
            return Ok(Decimal::ZERO);
        }

        let actual = U256::from(amount_out);
        if actual >= expected {
            return Ok(Decimal::ZERO);
        }

        let diff = Decimal::from((expected - actual).as_u128());
        let exp = Decimal::from(expected.as_u128());
        Ok((diff / exp).max(Decimal::ZERO))
    }

    /// Returns a new `UniswapV2Pair` with reserves updated after the swap.
    pub fn simulate_swap(&self, amount_in: u128, token_in: &Token) -> PricingResult<Self> {
        let amount_out = self.get_amount_out(amount_in, token_in)?;

        let (new_reserve0, new_reserve1) = if *token_in == self.token0 {
            (
                self.reserve0 + amount_in,
                self.reserve1.saturating_sub(amount_out),
            )
        } else {
            (
                self.reserve0.saturating_sub(amount_out),
                self.reserve1 + amount_in,
            )
        };

        Ok(Self {
            address: self.address.clone(),
            token0: self.token0.clone(),
            token1: self.token1.clone(),
            reserve0: new_reserve0,
            reserve1: new_reserve1,
            fee_bps: self.fee_bps,
        })
    }

    /// Fetches live pair data and token metadata from an on-chain Uniswap V2 pair contract.
    pub async fn from_chain(address: Address, client: &ChainClient) -> PricingResult<Self> {
        let call = |data: Vec<u8>| {
            TransactionRequest::contract_call(address.clone(), data, MAINNET_CHAIN_ID)
        };

        let reserves_raw = client
            .call(&call(GET_RESERVES_SELECTOR.to_vec()), BlockId::Latest)
            .await
            .map_err(|e| PricingError::ChainCall(e.to_string()))?;

        if reserves_raw.len() < GET_RESERVES_RETURN_MIN {
            return Err(PricingError::AbiDecode(format!(
                "getReserves returned {} bytes, expected {GET_RESERVES_RETURN_MIN}",
                reserves_raw.len()
            )));
        }
        let reserve0 = decode_u128_from_slot(&reserves_raw[0..EVM_WORD_LEN])?;
        let reserve1 = decode_u128_from_slot(&reserves_raw[EVM_WORD_LEN..EVM_WORD_LEN * 2])?;

        let token0_raw = client
            .call(&call(TOKEN0_SELECTOR.to_vec()), BlockId::Latest)
            .await
            .map_err(|e| PricingError::ChainCall(e.to_string()))?;
        let token0_addr = decode_address_from_slot(&token0_raw)?;

        let token1_raw = client
            .call(&call(TOKEN1_SELECTOR.to_vec()), BlockId::Latest)
            .await
            .map_err(|e| PricingError::ChainCall(e.to_string()))?;
        let token1_addr = decode_address_from_slot(&token1_raw)?;

        let zero_value = TokenAmount::eth(0u64);
        let token0 = fetch_token_metadata(&token0_addr, client, zero_value.clone()).await?;
        let token1 = fetch_token_metadata(&token1_addr, client, zero_value.clone()).await?;

        Self::new(address, token0, token1, reserve0, reserve1, DEFAULT_FEE_BPS)
    }

    /// Fetches only the current reserves from an on-chain Uniswap V2 pair contract.
    ///
    /// This is cheaper than [`from_chain`] when token metadata is already known
    /// (e.g., refreshing prices on a pool that was previously loaded).
    pub async fn fetch_reserves(
        address: &Address,
        client: &ChainClient,
    ) -> PricingResult<(u128, u128)> {
        let call = TransactionRequest::contract_call(
            address.clone(),
            GET_RESERVES_SELECTOR.to_vec(),
            MAINNET_CHAIN_ID,
        );

        let reserves_raw = client
            .call(&call, BlockId::Latest)
            .await
            .map_err(|e| PricingError::ChainCall(e.to_string()))?;

        if reserves_raw.len() < GET_RESERVES_RETURN_MIN {
            return Err(PricingError::AbiDecode(format!(
                "getReserves returned {} bytes, expected {GET_RESERVES_RETURN_MIN}",
                reserves_raw.len()
            )));
        }

        let reserve0 = decode_u128_from_slot(&reserves_raw[0..EVM_WORD_LEN])?;
        let reserve1 = decode_u128_from_slot(&reserves_raw[EVM_WORD_LEN..EVM_WORD_LEN * 2])?;

        Ok((reserve0, reserve1))
    }
}

/// Decodes a `uint256` (or smaller) from a 32-byte ABI slot into `u128`.
pub fn decode_u128_from_slot(slot: &[u8]) -> PricingResult<u128> {
    if slot.len() < EVM_WORD_LEN {
        return Err(PricingError::AbiDecode("slot too short".into()));
    }
    let bytes: [u8; 16] = slot[EVM_WORD_LEN / 2..EVM_WORD_LEN]
        .try_into()
        .map_err(|_| PricingError::AbiDecode("slice conversion failed".into()))?;
    Ok(u128::from_be_bytes(bytes))
}

/// Decodes a 20-byte Ethereum address from a 32-byte ABI slot.
pub fn decode_address_from_slot(slot: &[u8]) -> PricingResult<Address> {
    if slot.len() < EVM_WORD_LEN {
        return Err(PricingError::AbiDecode("slot too short".into()));
    }
    let hex = format!("0x{}", hex::encode(&slot[ADDRESS_SKIP_LEN..EVM_WORD_LEN]));
    Address::new(&hex).map_err(|e| PricingError::AbiDecode(e.to_string()))
}

/// Decodes a `uint8` from a 32-byte ABI slot.
pub fn decode_u8_from_slot(slot: &[u8]) -> Option<u8> {
    if slot.len() < EVM_WORD_LEN {
        return None;
    }
    let bytes: [u8; EVM_WORD_LEN] = slot.try_into().ok()?;
    let value = U256::from_big_endian(&bytes);
    Some(value.as_u64() as u8)
}

/// Fetches `symbol()` and `decimals()` from an ERC-20 token contract.
pub async fn fetch_token_metadata(
    addr: &Address,
    client: &ChainClient,
    _zero_value: TokenAmount,
) -> PricingResult<Token> {
    let call =
        |data: Vec<u8>| TransactionRequest::contract_call(addr.clone(), data, MAINNET_CHAIN_ID);

    let dec_raw = client
        .call(&call(DECIMALS_SELECTOR.to_vec()), BlockId::Latest)
        .await
        .map_err(|e| PricingError::ChainCall(e.to_string()))?;
    let decimals = decode_u8_from_slot(&dec_raw).ok_or_else(|| {
        PricingError::AbiDecode(format!("failed to decode decimals for token {}", addr))
    })?;

    let sym_raw = client
        .call(&call(SYMBOL_SELECTOR.to_vec()), BlockId::Latest)
        .await
        .map_err(|e| PricingError::ChainCall(e.to_string()))?;
    let symbol = decode_string_from_abi(&sym_raw).unwrap_or_else(|| {
        warn!(
            token = %addr,
            "failed to decode token symbol; using placeholder"
        );
        "???".into()
    });

    Ok(Token {
        address: addr.clone(),
        symbol,
        decimals,
    })
}

/// Decodes an ABI-encoded `string` (dynamic type) from a raw byte slice.
pub fn decode_string_from_abi(raw: &[u8]) -> Option<String> {
    if raw.len() < ABI_STRING_MIN {
        return None;
    }
    let len_bytes: [u8; 8] = raw[ABI_STRING_LEN_OFFSET..ABI_STRING_DATA_START]
        .try_into()
        .ok()?;
    let len = u64::from_be_bytes(len_bytes) as usize;
    if raw.len() < ABI_STRING_DATA_START + len {
        return None;
    }
    String::from_utf8(raw[ABI_STRING_DATA_START..ABI_STRING_DATA_START + len].to_vec()).ok()
}

/// A single row in the impact table.
#[derive(Debug, Clone)]
pub struct ImpactRow {
    /// Raw amount of `token_in` (integer units).
    pub amount_in: u128,
    /// Raw amount of `token_out` received.
    pub amount_out: u128,
    /// Spot price before the trade (display only).
    pub spot_price: Decimal,
    /// Actual execution price for this trade size (display only).
    pub execution_price: Decimal,
    /// Price impact as a percentage (`1.0` = 100%).
    pub price_impact_pct: Decimal,
}

/// Holds the result of [`PriceImpactAnalyzer::estimate_true_cost`].
#[derive(Debug, Clone)]
pub struct TradeCost {
    /// Raw `amount_out` before gas deduction.
    pub gross_output: u128,
    /// Gas cost denominated in ETH wei.
    pub gas_cost_eth: u128,
    /// Gas cost converted to `token_out` units (at spot price).
    pub gas_cost_in_output_token: u128,
    /// Net output after gas deduction (`gross_output - gas_cost_in_output_token`).
    pub net_output: u128,
    /// `net_output_human / amount_in_human` — the all-in effective price.
    pub effective_price: Decimal,
}

/// Analyzes price impact and total trade cost across different input sizes.
#[derive(Debug, Clone)]
pub struct PriceImpactAnalyzer {
    /// The pair to analyze.
    pub pair: UniswapV2Pair,
}

impl PriceImpactAnalyzer {
    /// Creates a new analyzer for the given pair.
    pub fn new(pair: UniswapV2Pair) -> Self {
        Self { pair }
    }

    /// Generates an impact table for the listed input sizes.
    pub fn generate_impact_table(
        &self,
        token_in: &Token,
        sizes: &[u128],
    ) -> PricingResult<Vec<ImpactRow>> {
        let spot = self.pair.get_spot_price(token_in)?;

        sizes
            .iter()
            .map(|&amount_in| {
                let amount_out = self.pair.get_amount_out(amount_in, token_in)?;
                let exec = self.pair.get_execution_price(amount_in, token_in)?;
                let impact = self.pair.get_price_impact(amount_in, token_in)?;
                Ok(ImpactRow {
                    amount_in,
                    amount_out,
                    spot_price: spot,
                    execution_price: exec,
                    price_impact_pct: impact * Decimal::ONE_HUNDRED,
                })
            })
            .collect()
    }

    /// Binary-searches for the largest trade whose price impact is at most `max_impact_pct`.
    pub fn find_max_size_for_impact(
        &self,
        token_in: &Token,
        max_impact_pct: Decimal,
    ) -> PricingResult<u128> {
        let (reserve_in, _) = self.pair.reserves_for(token_in)?;
        let max_impact = max_impact_pct / Decimal::ONE_HUNDRED;

        let smallest_impact = self.pair.get_price_impact(1, token_in)?;
        if smallest_impact > max_impact {
            return Err(PricingError::NoFeasibleSize);
        }

        let mut lo: u128 = 1;
        let mut hi: u128 = reserve_in / MAX_SWAP_RESERVE_DIVISOR;
        let mut best: u128 = 1;

        while lo <= hi {
            let mid = lo + (hi - lo) / 2;
            let impact = self.pair.get_price_impact(mid, token_in)?;
            if impact <= max_impact {
                best = mid;
                lo = mid + 1;
            } else {
                hi = mid - 1;
            }
        }

        Ok(best)
    }

    /// Estimates the all-in cost of a trade, including gas.
    pub fn estimate_true_cost(
        &self,
        amount_in: u128,
        token_in: &Token,
        gas_price_gwei: u128,
        gas_estimate: u128,
    ) -> PricingResult<TradeCost> {
        let gross_output = self.pair.get_amount_out(amount_in, token_in)?;
        let token_out = self.pair.token_out_for(token_in)?;

        let gas_price_wei = gas_price_gwei * WEI_PER_GWEI;
        let gas_cost_eth = gas_estimate * gas_price_wei;

        let scale_out = DECIMAL_BASE.pow(token_out.decimals as u32);
        let scale_eth: u128 = DECIMAL_BASE.pow(ETH_DECIMALS as u32);

        let gas_cost_in_output_token = gas_cost_eth * scale_out / scale_eth;
        let net_output = gross_output.saturating_sub(gas_cost_in_output_token);

        let scale_in = Decimal::from(DECIMAL_BASE.pow(token_in.decimals as u32));
        let human_in = Decimal::from(amount_in) / scale_in;
        let human_net = Decimal::from(net_output) / Decimal::from(scale_out);

        let effective_price = if human_in.is_zero() {
            Decimal::ZERO
        } else {
            human_net / human_in
        };

        Ok(TradeCost {
            gross_output,
            gas_cost_eth,
            gas_cost_in_output_token,
            net_output,
            effective_price,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ethers::types::U256;
    use proptest::prelude::*;
    use rust_decimal::Decimal;

    fn weth() -> Token {
        Token {
            address: Address::new("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
            symbol: "WETH".into(),
            decimals: 18,
        }
    }

    fn usdc() -> Token {
        Token {
            address: Address::new("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
            symbol: "USDC".into(),
            decimals: 6,
        }
    }

    fn eth_usdc_pair() -> UniswapV2Pair {
        let pair_addr = Address::new("0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc").unwrap();
        UniswapV2Pair::new(
            pair_addr,
            weth(),
            usdc(),
            1_000 * DECIMAL_BASE.pow(18),
            2_000_000 * DECIMAL_BASE.pow(6),
            30,
        )
        .unwrap()
    }

    #[test]
    fn test_get_amount_out_basic() {
        let pair = eth_usdc_pair();
        let usdc_in: u128 = 2_000 * DECIMAL_BASE.pow(6);
        let eth_out = pair.get_amount_out(usdc_in, &usdc()).unwrap();
        let one_eth: u128 = DECIMAL_BASE.pow(18);
        assert!(eth_out < one_eth);
        assert!(eth_out > (one_eth * 99) / 100);
    }

    #[test]
    fn test_get_amount_out_matches_solidity() {
        let pair = eth_usdc_pair();
        let usdc_in: u128 = 2_000 * DECIMAL_BASE.pow(6);
        let amount_in_with_fee: u128 = usdc_in * (10_000 - 30);
        let reserve_in: u128 = 2_000_000 * DECIMAL_BASE.pow(6);
        let reserve_out: u128 = 1_000 * DECIMAL_BASE.pow(18);
        let numerator = amount_in_with_fee * reserve_out;
        let denominator = reserve_in * 10_000 + amount_in_with_fee;
        let expected = numerator / denominator;
        let actual = pair.get_amount_out(usdc_in, &usdc()).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_integer_math_no_floats_large_numbers() {
        let pair_addr = Address::new("0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc").unwrap();
        let pair = UniswapV2Pair::new(
            pair_addr,
            weth(),
            usdc(),
            DECIMAL_BASE.pow(30),
            DECIMAL_BASE.pow(30),
            30,
        )
        .unwrap();
        let amount_in: u128 = DECIMAL_BASE.pow(25);
        let out = pair.get_amount_out(amount_in, &weth()).unwrap();
        assert!(out > 0);
        assert!(out < DECIMAL_BASE.pow(30));
    }

    #[test]
    fn test_get_amount_out_zero_input() {
        let pair = eth_usdc_pair();
        let err = pair.get_amount_out(0, &usdc()).unwrap_err();
        assert_eq!(err, PricingError::ZeroAmountIn);
    }

    #[test]
    fn test_get_amount_out_unknown_token() {
        let pair = eth_usdc_pair();
        let dai = Token {
            address: Address::new("0x6B175474E89094C44Da98b954EedeAC495271d0F").unwrap(),
            symbol: "DAI".into(),
            decimals: 18,
        };
        let err = pair.get_amount_out(1_000, &dai).unwrap_err();
        assert!(matches!(err, PricingError::UnknownToken(_)));
    }

    #[test]
    fn test_get_amount_in_inverse_of_out() {
        let pair = eth_usdc_pair();
        let desired_eth_out: u128 = DECIMAL_BASE.pow(17);
        let usdc_in_required = pair.get_amount_in(desired_eth_out, &weth()).unwrap();
        let actual_out = pair.get_amount_out(usdc_in_required, &usdc()).unwrap();
        assert!(actual_out >= desired_eth_out);
        assert!(actual_out - desired_eth_out < DECIMAL_BASE.pow(12));
    }

    #[test]
    fn test_get_amount_in_insufficient_liquidity() {
        let pair = eth_usdc_pair();
        let too_much: u128 = 1_001 * DECIMAL_BASE.pow(18);
        let err = pair.get_amount_in(too_much, &weth()).unwrap_err();
        assert!(matches!(err, PricingError::InsufficientLiquidity { .. }));
    }

    #[test]
    fn test_get_amount_in_zero_output() {
        let pair = eth_usdc_pair();
        let err = pair.get_amount_in(0, &weth()).unwrap_err();
        assert_eq!(err, PricingError::ZeroAmountOut);
    }

    #[test]
    fn test_simulate_swap_is_immutable() {
        let pair = eth_usdc_pair();
        let original_reserve0 = pair.reserve0;
        let original_reserve1 = pair.reserve1;
        let usdc_in: u128 = 2_000 * DECIMAL_BASE.pow(6);
        let new_pair = pair.simulate_swap(usdc_in, &usdc()).unwrap();
        assert_eq!(pair.reserve0, original_reserve0);
        assert_eq!(pair.reserve1, original_reserve1);
        assert!(new_pair.reserve1 > original_reserve1);
        assert!(new_pair.reserve0 < original_reserve0);
    }

    #[test]
    fn test_simulate_swap_updates_price() {
        let pair = eth_usdc_pair();
        let usdc_in: u128 = 100_000 * DECIMAL_BASE.pow(6);
        let spot_before = pair.get_spot_price(&usdc()).unwrap();
        let new_pair = pair.simulate_swap(usdc_in, &usdc()).unwrap();
        let spot_after = new_pair.get_spot_price(&usdc()).unwrap();
        assert!(spot_after < spot_before);
    }

    #[test]
    fn test_spot_price_direction() {
        let pair = eth_usdc_pair();
        let usdc_to_weth = pair.get_spot_price(&usdc()).unwrap();
        let expected = Decimal::new(5, 4);
        let tolerance = Decimal::new(1, 8);
        assert!((usdc_to_weth - expected).abs() < tolerance);
        let weth_to_usdc = pair.get_spot_price(&weth()).unwrap();
        let expected_rev = Decimal::from(2000u64);
        assert!((weth_to_usdc - expected_rev).abs() < Decimal::ONE);
    }

    #[test]
    fn test_price_impact_small_trade() {
        let pair = eth_usdc_pair();
        let tiny: u128 = 100 * DECIMAL_BASE.pow(6);
        let impact = pair.get_price_impact(tiny, &usdc()).unwrap();
        assert!(impact < Decimal::new(4, 3));
        assert!(impact > Decimal::ZERO);
    }

    #[test]
    fn test_price_impact_large_trade() {
        let pair = eth_usdc_pair();
        let large: u128 = 1_000_000 * DECIMAL_BASE.pow(6);
        let impact = pair.get_price_impact(large, &usdc()).unwrap();
        assert!(impact > Decimal::new(30, 2));
    }

    #[test]
    fn test_impact_table_row_count() {
        let analyzer = PriceImpactAnalyzer::new(eth_usdc_pair());
        let sizes: Vec<u128> = vec![
            1_000 * DECIMAL_BASE.pow(6),
            10_000 * DECIMAL_BASE.pow(6),
            100_000 * DECIMAL_BASE.pow(6),
        ];
        let table = analyzer.generate_impact_table(&usdc(), &sizes).unwrap();
        assert_eq!(table.len(), 3);
    }

    #[test]
    fn test_impact_table_impact_monotonically_increases() {
        let analyzer = PriceImpactAnalyzer::new(eth_usdc_pair());
        let sizes: Vec<u128> = (1..=5)
            .map(|i| i as u128 * 100_000 * DECIMAL_BASE.pow(6))
            .collect();
        let table = analyzer.generate_impact_table(&usdc(), &sizes).unwrap();
        for window in table.windows(2) {
            assert!(window[1].price_impact_pct >= window[0].price_impact_pct);
        }
    }

    #[test]
    fn test_find_max_size_for_impact_satisfies_constraint() {
        let analyzer = PriceImpactAnalyzer::new(eth_usdc_pair());
        let max_pct = Decimal::ONE;
        let max_size = analyzer.find_max_size_for_impact(&usdc(), max_pct).unwrap();
        let impact = eth_usdc_pair().get_price_impact(max_size, &usdc()).unwrap();
        let impact_pct = impact * Decimal::ONE_HUNDRED;
        assert!(impact_pct <= max_pct);
    }

    #[test]
    fn test_estimate_true_cost_net_lte_gross() {
        let analyzer = PriceImpactAnalyzer::new(eth_usdc_pair());
        let usdc_in: u128 = 2_000 * DECIMAL_BASE.pow(6);
        let cost = analyzer
            .estimate_true_cost(usdc_in, &usdc(), 20, 150_000)
            .unwrap();
        assert!(cost.net_output <= cost.gross_output);
        assert!(cost.gas_cost_eth > 0);
    }

    #[test]
    fn test_invalid_fee_bps_rejected() {
        let pair_addr = Address::new("0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc").unwrap();
        let err = UniswapV2Pair::new(pair_addr, weth(), usdc(), 1_000, 1_000, 10_000).unwrap_err();
        assert_eq!(err, PricingError::InvalidFeeBps(10_000));
    }

    #[test]
    fn test_fee_math_matches_reference_for_large_numbers() {
        let pair_addr = Address::new("0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc").unwrap();
        let reserve0 = 123_456_789_012_345_678_901_234_567_890u128;
        let reserve1 = 98_765_432_109_876_543_210_987_654_321u128;
        let amount_in = 9_876_543_210_987_654_321_000_000u128;

        for fee_bps in [0u32, 1u32, 30u32, 100u32, 500u32, 9_999u32] {
            let pair = UniswapV2Pair::new(
                pair_addr.clone(),
                weth(),
                usdc(),
                reserve0,
                reserve1,
                fee_bps,
            )
            .unwrap();

            let actual = pair.get_amount_out(amount_in, &weth()).unwrap();

            let ain = U256::from(amount_in);
            let r_in = U256::from(reserve0);
            let r_out = U256::from(reserve1);
            let bps = U256::from(10_000u64);
            let fee = U256::from(fee_bps);
            let amount_in_with_fee = ain * (bps - fee);
            let expected =
                (amount_in_with_fee * r_out / (r_in * bps + amount_in_with_fee)).as_u128();

            assert_eq!(actual, expected, "mismatch for fee_bps={fee_bps}");
        }
    }

    #[test]
    fn test_get_amount_in_is_minimum_required_input() {
        let pair = eth_usdc_pair();
        let desired_eth_out: u128 = 123_456_789_000_000_000;

        let required_usdc_in = pair.get_amount_in(desired_eth_out, &weth()).unwrap();
        let out_with_required = pair.get_amount_out(required_usdc_in, &usdc()).unwrap();
        assert!(out_with_required >= desired_eth_out);

        let out_with_less = pair.get_amount_out(required_usdc_in - 1, &usdc()).unwrap();
        assert!(out_with_less < desired_eth_out);
    }

    proptest! {
        #[test]
        fn prop_amount_out_is_monotonic_for_larger_input(
            reserve0 in 1_000_000u128..10_000_000_000_000_000_000_000_000u128,
            reserve1 in 1_000_000u128..10_000_000_000_000_000_000_000_000u128,
            a in 1u128..1_000_000_000_000_000_000_000u128,
            b in 1u128..1_000_000_000_000_000_000_000u128,
            fee_bps in 0u32..9_999u32,
        ) {
            let pair_addr = Address::new("0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc").unwrap();
            let pair = UniswapV2Pair::new(pair_addr, weth(), usdc(), reserve0, reserve1, fee_bps).unwrap();

            let lo = a.min(b);
            let hi = a.max(b);
            let out_lo = pair.get_amount_out(lo, &weth()).unwrap();
            let out_hi = pair.get_amount_out(hi, &weth()).unwrap();

            prop_assert!(out_lo <= out_hi);
            prop_assert!(out_hi < reserve1);
        }

        #[test]
        fn prop_get_amount_in_is_ceiling_of_requirement(
            reserve0 in 1_000_000u128..10_000_000_000_000_000_000_000_000u128,
            reserve1 in 1_000_000u128..10_000_000_000_000_000_000_000_000u128,
            desired_out in 1u128..1_000_000_000_000_000_000u128,
            fee_bps in 0u32..9_999u32,
        ) {
            prop_assume!(desired_out < reserve1);

            let pair_addr = Address::new("0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc").unwrap();
            let pair = UniswapV2Pair::new(pair_addr, weth(), usdc(), reserve0, reserve1, fee_bps).unwrap();

            let required_in = pair.get_amount_in(desired_out, &usdc()).unwrap();
            let out_at_required = pair.get_amount_out(required_in, &weth()).unwrap();
            prop_assert!(out_at_required >= desired_out);

            if required_in > 1 {
                let out_with_less = pair.get_amount_out(required_in - 1, &weth()).unwrap();
                prop_assert!(out_with_less < desired_out);
            }
        }
    }
}
