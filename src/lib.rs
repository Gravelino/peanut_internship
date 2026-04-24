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
pub mod executor;
pub mod integration;
pub mod inventory;
pub mod observability;
pub mod pricing;
pub mod strategy;

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
    AggregatedPrice, BINANCE_TESTNET_BASE_URL, BINANCE_TESTNET_WS_URL, BYBIT_TESTNET_BASE_URL,
    BYBIT_TESTNET_WS_URL, BinanceConfig, BybitAdapter, BybitConfig, CanExecuteResult, DepthEvent,
    DepthSnapshot, DepthUpdate, ExchangeAdapter, ExchangeClient, ExchangeConfig, ExchangeError,
    ExchangeResult, FeeStructure, FillLevel, HttpClient, LimitInterval, LimitKey, LimitType,
    LocalOrderBook, MyTrade, NormalizedBalance, OrderBookAnalyzer, OrderBookSnapshot, OrderResult,
    PortfolioSnapshot, PriceOracle, PriceSource, RateLimiter, RetryConfig, SequenceStatus,
    SkewResult, VenueSkew, WalkResult,
};
pub use executor::{
    CircuitBreaker, CircuitBreakerConfig, ExecutionContext, Executor,
    ExecutorConfig as ArbExecutorConfig, ExecutorError, ExecutorState, LegBehaviour, LegExecutor,
    LegFill, LegOutcome, LiveLegs, ReplayProtection, SimulatedLegs,
};
pub use integration::{
    ArbCheckDetails, ArbCheckError, ArbCheckResult, ArbChecker, ArbLogger, CrossDexOpportunity,
    DexPoolInfo, ForkSimInfo,
};
pub use inventory::{
    ArbRecord, Balance, CostEstimate, ExecutorConfig, InventoryError, InventoryResult,
    InventoryTracker, PnLChartExporter, PnLEngine, PnLSummary, RebalanceExecutor, RebalancePlanner,
    RebalanceResult, RebalanceStatus, RebalanceStep, TradeLeg, TradeStep, TradeSummary,
    TransferFeeInfo, TransferPlan, Venue, WalletBalanceFetcher, WithdrawStep,
    min_operating_balance, transfer_fees,
};
pub use pricing::{
    AmountOutDecoder, ArbDetector, ArbKind, ArbOpportunity, ForkSimulation, ForkSimulator,
    HistoricalImpactAnalyzer, HistoricalImpactPoint, ImpactRow, ImpactSummary, MempoolMonitor,
    ParsedSwap, PoolRef, PriceFeed, PriceImpactAnalyzer, PriceTick, PricingEngine, PricingError,
    PricingResult, Quote, QuoteError, QuoteResult, RouteFinder, SimulationComparison,
    SimulationResult, SimulationVerdict, SizeImpact, SizeImpactAvg, SwapParams, TradeCost,
    UniswapV2Pair, UniswapV3Pool, V3SwapQuote,
};
pub use strategy::{
    Direction, FeeStructure as StrategyFees, GeneratorConfig, PriceSource as StrategyPriceSource,
    ScorerConfig, Signal, SignalGenerator, SignalParams, SignalScorer, StrategyError,
    StrategyResult, StubPriceSource, VenuePrices,
};
