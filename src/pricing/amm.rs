//! # AMM Pricer
//! Exact Uniswap V2 math using integer arithmetic.
//! Display helpers produce Decimal values.

use ethers::types::U256;
use rust_decimal::Decimal;
use ethers::types::Bytes;

use crate::chain::client::ChainClient;
use crate::core::types::{Address, BlockId, Token, TokenAmount, TransactionRequest, ETH_DECIMALS};
use super::errors::{PricingError, PricingResult};

/// Basis-point scale used by Uniswap V2 fee math.
const BPS: u128 = 10_000;

/// Default fee in basis points (0.3%).
const DEFAULT_FEE_BPS: u32 = 30;

/// Base for decimal scaling.
const DECIMAL_RADIX: u128 = 10;

/// Chain ID for Ethereum Mainnet.
const MAINNET_CHAIN_ID: u64 = 1;

/// Multiplier to convert Gwei to Wei.
const WEI_PER_GWEI: u128 = 1_000_000_000;

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
    /// Creates a new `UniswapV2Pair`. Fails if `fee_bps >= 10_000`.
    pub fn new(
        address: Address,
        token0: Token,
        token1: Token,
        reserve0: u128,
        reserve1: u128,
        fee_bps: u32,
    ) -> PricingResult<Self> {
        if fee_bps >= (BPS as u32) {
            return Err(PricingError::InvalidFeeBps(fee_bps));
        }
        Ok(Self { address, token0, token1, reserve0, reserve1, fee_bps })
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

        let ain      = U256::from(amount_in);
        let r_in     = U256::from(reserve_in);
        let r_out    = U256::from(reserve_out);
        let bps      = U256::from(BPS);
        let fee      = U256::from(self.fee_bps);

        let amount_in_with_fee = ain * (bps - fee);
        let numerator          = amount_in_with_fee * r_out;
        let denominator        = r_in * bps + amount_in_with_fee;

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

        let aout  = U256::from(amount_out);
        let r_in  = U256::from(reserve_in);
        let r_out = U256::from(reserve_out);
        let bps   = U256::from(BPS);
        let fee   = U256::from(self.fee_bps);

        let numerator   = r_in * aout * bps;
        let denominator = (r_out - aout) * (bps - fee);

        Ok((numerator / denominator).as_u128() + 1)
    }

    /// Returns spot price of `token_in` in terms of `token_out` (display-only).
    pub fn get_spot_price(&self, token_in: &Token) -> PricingResult<Decimal> {
        let (reserve_in, reserve_out) = self.reserves_for(token_in)?;
        let token_out = self.token_out_for(token_in)?;

        let scale_in = Decimal::from(DECIMAL_RADIX.pow(token_in.decimals as u32));
        let scale_out = Decimal::from(DECIMAL_RADIX.pow(token_out.decimals as u32));

        let r_in = Decimal::from(reserve_in) / scale_in;
        let r_out = Decimal::from(reserve_out) / scale_out;

        Ok(r_out / r_in)
    }

    /// Returns execution price for a trade of `amount_in` (`amount_out / amount_in`).
    pub fn get_execution_price(&self, amount_in: u128, token_in: &Token) -> PricingResult<Decimal> {
        let amount_out = self.get_amount_out(amount_in, token_in)?;
        let token_out = self.token_out_for(token_in)?;

        let scale_in = Decimal::from(DECIMAL_RADIX.pow(token_in.decimals as u32));
        let scale_out = Decimal::from(DECIMAL_RADIX.pow(token_out.decimals as u32));

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

        let ain   = U256::from(amount_in);
        let r_in  = U256::from(reserve_in);
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
        let exp  = Decimal::from(expected.as_u128());
        Ok((diff / exp).max(Decimal::ZERO))
    }

    /// Returns a new `UniswapV2Pair` with reserves updated after the swap.
    pub fn simulate_swap(&self, amount_in: u128, token_in: &Token) -> PricingResult<Self> {
        let amount_out = self.get_amount_out(amount_in, token_in)?;

        let (new_reserve0, new_reserve1) = if *token_in == self.token0 {
            (self.reserve0 + amount_in, self.reserve1 - amount_out)
        } else {
            (self.reserve0 - amount_out, self.reserve1 + amount_in)
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
        let dummy_value = TokenAmount::eth(0u64);
        let dummy_data_tx = |data: Vec<u8>| TransactionRequest {
            to: address.clone(),
            value: dummy_value.clone(),
            data: Bytes::from(data),
            nonce: None,
            gas_limit: None,
            max_fee_per_gas: None,
            max_priority_fee: None,
            chain_id: MAINNET_CHAIN_ID,
        };

        let reserves_raw = client
            .call(&dummy_data_tx(GET_RESERVES_SELECTOR.to_vec()), BlockId::Latest)
            .await
            .map_err(|e| PricingError::ChainCall(e.to_string()))?;

        if reserves_raw.len() < 96 {
            return Err(PricingError::AbiDecode(format!(
                "getReserves returned {} bytes, expected 96",
                reserves_raw.len()
            )));
        }
        let reserve0 = decode_u128_from_slot(&reserves_raw[0..32])?;
        let reserve1 = decode_u128_from_slot(&reserves_raw[32..64])?;

        let token0_raw = client
            .call(&dummy_data_tx(TOKEN0_SELECTOR.to_vec()), BlockId::Latest)
            .await
            .map_err(|e| PricingError::ChainCall(e.to_string()))?;
        let token0_addr = decode_address_from_slot(&token0_raw)?;

        let token1_raw = client
            .call(&dummy_data_tx(TOKEN1_SELECTOR.to_vec()), BlockId::Latest)
            .await
            .map_err(|e| PricingError::ChainCall(e.to_string()))?;
        let token1_addr = decode_address_from_slot(&token1_raw)?;

        let token0 = fetch_token_metadata(&token0_addr, client, dummy_value.clone()).await?;
        let token1 = fetch_token_metadata(&token1_addr, client, dummy_value.clone()).await?;

        Self::new(address, token0, token1, reserve0, reserve1, DEFAULT_FEE_BPS)
    }
}

/// Decodes a `uint256` (or smaller) from a 32-byte ABI slot into `u128`.
fn decode_u128_from_slot(slot: &[u8]) -> PricingResult<u128> {
    if slot.len() < 32 {
        return Err(PricingError::AbiDecode("slot too short".into()));
    }
    let bytes: [u8; 16] = slot[16..32]
        .try_into()
        .map_err(|_| PricingError::AbiDecode("slice conversion failed".into()))?;
    Ok(u128::from_be_bytes(bytes))
}

/// Decodes an `address` (20 bytes) from a 32-byte ABI slot.
fn decode_address_from_slot(slot: &[u8]) -> PricingResult<Address> {
    if slot.len() < 32 {
        return Err(PricingError::AbiDecode("slot too short".into()));
    }
    let hex = format!("0x{}", hex::encode(&slot[12..32]));
    Address::new(&hex).map_err(|e| PricingError::AbiDecode(e.to_string()))
}

/// Fetches `symbol()` and `decimals()` from an ERC-20 token contract.
async fn fetch_token_metadata(
    addr: &Address,
    client: &ChainClient,
    dummy_value: TokenAmount,
) -> PricingResult<Token> {
    let call = |selector: Vec<u8>| TransactionRequest {
        to: addr.clone(),
        value: dummy_value.clone(),
        data: Bytes::from(selector),
        nonce: None,
        gas_limit: None,
        max_fee_per_gas: None,
        max_priority_fee: None,
        chain_id: MAINNET_CHAIN_ID,
    };

    let dec_raw = client
        .call(&call(DECIMALS_SELECTOR.to_vec()), BlockId::Latest)
        .await
        .map_err(|e| PricingError::ChainCall(e.to_string()))?;
    let decimals = dec_raw.last().copied().unwrap_or(18);

    let sym_raw = client
        .call(&call(SYMBOL_SELECTOR.to_vec()), BlockId::Latest)
        .await
        .map_err(|e| PricingError::ChainCall(e.to_string()))?;
    let symbol = decode_string_from_abi(&sym_raw).unwrap_or_else(|| "???".into());

    Ok(Token { address: addr.clone(), symbol, decimals })
}

/// Decodes an ABI-encoded `string` (dynamic type) from a raw byte slice.
fn decode_string_from_abi(raw: &[u8]) -> Option<String> {
    if raw.len() < 96 {
        return None;
    }
    let len_bytes: [u8; 8] = raw[56..64].try_into().ok()?;
    let len = u64::from_be_bytes(len_bytes) as usize;
    if raw.len() < 64 + len {
        return None;
    }
    String::from_utf8(raw[64..64 + len].to_vec()).ok()
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
        let mut hi: u128 = reserve_in / 2;
        let mut best: u128 = 1;

        while lo <= hi {
            let mid = lo + (hi - lo) / 2;
            let impact = self.pair.get_price_impact(mid, token_in)?;
            if impact <= max_impact {
                best = mid;
                lo = mid + 1;
            } else {
                if mid == 0 { break; }
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

        let scale_out = DECIMAL_RADIX.pow(token_out.decimals as u32);
        let scale_eth: u128 = DECIMAL_RADIX.pow(ETH_DECIMALS as u32);

        let gas_cost_in_output_token = gas_cost_eth * scale_out / scale_eth;
        let net_output = gross_output.saturating_sub(gas_cost_in_output_token);

        let scale_in = Decimal::from(DECIMAL_RADIX.pow(token_in.decimals as u32));
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
