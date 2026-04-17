//! # Peanut Internship Rust Project
//!
//! A high-level library for Ethereum blockchain interaction, transaction building,
//! and analysis. Designed for the Peanut Trade internship program.
//!
//! ## Features
//! - **Core**: Types for [Address], [TokenAmount], and [WalletManager] for signing.
//! - **Chain**: [ChainClient] for RPC interaction and [TransactionBuilder] for fluent transaction creation.
//! - **Analysis**: Transaction analysis and event log decoding (ERC-20, Uniswap).
//! - **Pricing**: Exact Uniswap V2 math, price impact analysis, and route finding.
//!
//! ## Example: Transferring ETH
//! ```no_run
//! use peanut_internship_rust::{ChainClient, WalletManager, TransactionBuilder, TokenAmount, Address};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let client = ChainClient::new(vec!["http://localhost:8545".to_string()], 30, 3).unwrap();
//!     let wallet = WalletManager::from_env("PRIVATE_KEY")?;
//!     let to = Address::new("0x...")?;
//!     let amount = TokenAmount::from_eth("0.1")?;
//!
//!     let tx_hash = TransactionBuilder::new(client, wallet)
//!         .to(to)
//!         .value(amount)
//!         .send()
//!         .await?;
//!
//!     println!("Transaction sent: {}", tx_hash);
//!     Ok(())
//! }
//! ```

pub mod chain;
pub mod core;
pub mod exchange;
pub mod integration;
pub mod inventory;
pub mod pricing;

pub use chain::{
    ChainClient, ChainError, ChainResult, InsufficientFunds, NonceTooLow, ReplacementUnderpriced,
    TransactionBuilder,
};
pub use core::serializer::CanonicalSerializer;
pub use core::types::{
    Address, BlockId, CoreError, DEFAULT_GAS_BUFFER_BPS, ETH_DECIMALS, ETH_SYMBOL, GasPrice,
    GasPriority, MAINNET_CHAIN_ID, MIN_GAS_LIMIT, RECEIPT_STATUS_FAILED, RECEIPT_STATUS_SUCCESS,
    SEPOLIA_CHAIN_ID, Token, TokenAmount, TransactionReceipt, TransactionRequest,
    TransactionStatus, WEI_PER_GWEI,
};
pub use core::wallet::{WalletError, WalletManager};
pub use exchange::{
    AggregatedPrice, BINANCE_TESTNET_BASE_URL, BINANCE_TESTNET_WS_URL, BinanceConfig,
    CanExecuteResult, ExchangeClient, ExchangeError, ExchangeResult, FeeStructure, FillLevel,
    HttpClient, LimitInterval, LimitKey, LimitType, MyTrade, NormalizedBalance, OrderBookAnalyzer,
    OrderBookSnapshot, OrderResult, PortfolioSnapshot, PriceOracle, PriceSource, RateLimiter,
    RetryConfig, SkewResult, VenueSkew, WalkResult,
};
pub use integration::{
    ArbCheckDetails, ArbCheckError, ArbCheckResult, ArbChecker, CrossDexOpportunity, DexPoolInfo,
    ForkSimInfo,
};
pub use inventory::{
    ArbRecord, Balance, CostEstimate, InventoryError, InventoryResult, InventoryTracker, PnLEngine,
    PnLSummary, RebalancePlanner, TradeLeg, TradeSummary, TransferFeeInfo, TransferPlan, Venue,
    WalletBalanceFetcher, min_operating_balance, transfer_fees,
};
pub use pricing::{
    AmountOutDecoder, ArbDetector, ArbKind, ArbOpportunity, ForkSimulation, ForkSimulator,
    HistoricalImpactAnalyzer, HistoricalImpactPoint, ImpactRow, ImpactSummary, MempoolMonitor,
    ParsedSwap, PoolRef, PriceFeed, PriceImpactAnalyzer, PriceTick, PricingEngine, PricingError,
    PricingResult, Quote, QuoteError, QuoteResult, RouteFinder, SimulationComparison,
    SimulationResult, SimulationVerdict, SizeImpact, SizeImpactAvg, SwapParams, TradeCost,
    UniswapV2Pair, UniswapV3Pool, V3SwapQuote,
};
