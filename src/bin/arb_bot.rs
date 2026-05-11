//! End-to-end arbitrage bot: detects, scores, and executes opportunities.
//!
//! This binary wires Week 1-4 modules together:
//! - [`ExchangeClient`] for CEX market data
//! - [`StubPriceSource`] for DEX prices (falls back without a fork URL)
//! - [`InventoryTracker`] / [`PnLEngine`] for state and accounting
//! - [`SignalGenerator`] + [`SignalScorer`] for opportunity detection
//! - [`Executor`] for CEX/DEX leg coordination (simulation_mode by default)

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use clap::Parser;
use ethers::types::{Bytes, U256};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::Serialize;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use peanut_internship_rust::assets::{self};
use peanut_internship_rust::chain::{
    ChainClient, FlashbotsConfig, FlashbotsRelayClient, NonceManager,
};
use peanut_internship_rust::config::address_book::{
    AddressBookPoolKind, AddressBookTokenConfig, dex_pool_fee_bps_map, load_parsed_address_book,
    rebalance_token_book, selected_pairs, selected_pool_kinds,
    validate_arbitrum_known_token_symbols, validate_asset_symbol_uniqueness,
    validate_cex_pair_symbols, validate_selected_pool_compatibility,
};
use peanut_internship_rust::core::types::{
    ARBITRUM_CHAIN_ID, Address, BPS_SCALE, BlockId, DEFAULT_ALERT_LARGE_LOSS_USD,
    DEFAULT_ARB_GAS_UNITS, DEFAULT_BALANCE_SYNC_INTERVAL_SECS,
    DEFAULT_BALANCE_VERIFY_TOLERANCE_PCT, DEFAULT_CEX_FEE_BPS, DEFAULT_DEX_DEADLINE_SECS,
    DEFAULT_DEX_FEE_BPS, DEFAULT_DEX_SLIPPAGE_BPS, DEFAULT_FLASHBOTS_MAX_BLOCKS_TO_TRY,
    DEFAULT_FLASHBOTS_RELAY_URL, DEFAULT_FLASHBOTS_TARGET_BLOCK_OFFSET, DEFAULT_GAS_BUFFER_BPS,
    DEFAULT_GAS_COST_USD, DEFAULT_INITIAL_CAPITAL_USD, DEFAULT_KILL_SWITCH_FILE, DEFAULT_LOG_DIR,
    DEFAULT_MAX_CONCURRENT, DEFAULT_MAX_DAILY_LOSS_USD, DEFAULT_MAX_GAS_GWEI_CAP,
    DEFAULT_MIN_PROFIT_USD, DEFAULT_MIN_SCORE, DEFAULT_MIN_SPREAD_BPS, DEFAULT_ORDERBOOK_DEPTH,
    DEFAULT_QUEUE_MAX_SIZE, DEFAULT_REBALANCE_ALLOWED_ASSETS, DEFAULT_REBALANCE_ALLOWED_VENUES,
    DEFAULT_REBALANCE_INTERVAL_SECS, DEFAULT_REBALANCE_JOURNAL_PATH,
    DEFAULT_REBALANCE_MAX_SLIPPAGE_BPS, DEFAULT_REBALANCE_MAX_STEP_USD,
    DEFAULT_REBALANCE_MIN_FILL_PCT, DEFAULT_REBALANCE_QUOTE_ASSET, DEFAULT_RECONCILE_MAX_AGE_SECS,
    DEFAULT_RECONCILE_POLL_SECS, DEFAULT_RECONNECT_DELAY_SECS, DEFAULT_REPLAY_TTL_SECS,
    DEFAULT_RETRY_DELAY_SECS, DEFAULT_RISK_CONSECUTIVE_LOSS_LIMIT, DEFAULT_RISK_MAX_DAILY_LOSS_USD,
    DEFAULT_RISK_MAX_TRADE_USD, DEFAULT_RISK_MAX_TRADES_PER_HOUR, DEFAULT_SHUTDOWN_DRAIN_SECS,
    DEFAULT_TICK_MS, DEFAULT_WATCHDOG_INTERVAL_MS, ETH_DECIMALS, GasPriority, MIN_GAS_LIMIT,
    REBALANCE_DEVIATION_THRESHOLD_PCT, RPC_RETRIES, RPC_TIMEOUT_SECS, TokenAmount,
    TransactionRequest, WEI_PER_ETH, WEI_PER_GWEI,
};
use peanut_internship_rust::core::wallet::WalletManager;
use peanut_internship_rust::exchange::client::ExchangeClient;
use peanut_internship_rust::exchange::config::BinanceConfig;
use peanut_internship_rust::exchange::types::NormalizedBalance;
use peanut_internship_rust::exchange::{
    DepthEvent, DepthSnapshot, LocalOrderBook, OrderBookAnalyzer, OrderBookSnapshot,
    SequenceStatus, subscribe_book_ticker_stream, subscribe_depth_stream,
};
use peanut_internship_rust::executor::dex_swapper::WETH_DEPOSIT_SELECTOR;
use peanut_internship_rust::executor::engine::{
    Executor, ExecutorConfig, LegExecutor, LiveLegs, SimulatedLegs,
};
use peanut_internship_rust::executor::queue::{QueueConfig, QueueWorker, SignalQueue};
use peanut_internship_rust::executor::{
    ChainReceiptProvider, CompositeDexSwapper, DexPoolKind, DexSwapper, DexSwapperConfig,
    FlashbotsSwapper, PairAddressBook, PairTokens, PendingReconcile, ReconcileConfig,
    ReconcileStore, ReconcileWorker, TickOutcome, UniswapV2Swapper, UniswapV3Swapper,
    apply_slippage, build_allowance_calldata, build_approve_calldata, build_swap_calldata,
    v3_swap_calldata_for_pair,
};
use peanut_internship_rust::format::{self, *};
use peanut_internship_rust::inventory::pnl::{ArbRecord, PnLEngine, TradeJsonlLogger, TradeLeg};
use peanut_internship_rust::inventory::rebalancer::RebalancePlanner;
use peanut_internship_rust::inventory::tracker::InventoryTracker;
use peanut_internship_rust::inventory::types::{RebalanceStep, Venue};
use peanut_internship_rust::inventory::wallet::WalletBalanceFetcher;
use peanut_internship_rust::observability::{
    AlertEvent, AlertProvider, AlertRules, AlertSink, HaltCoordinator, NoopSink, WebhookSink,
    emit_best_effort, evaluate_execution, mask_webhook_url,
};
use peanut_internship_rust::pricing::{
    AmountOutDecoder, ForkSimulator, SwapParams, V3QuoterConfig, V3QuoterKind,
};
use peanut_internship_rust::safety::{PreTradeValidator, RiskLimits, RiskManager};
use peanut_internship_rust::strategy::fees::{FeeBreakdown, FeeStructure};
use peanut_internship_rust::strategy::generator::{
    CexOrderBookSource, GeneratorConfig, MarketState, PriceSource, SignalGenerator,
    StubPriceSource, split_pair,
};
use peanut_internship_rust::strategy::live_price_source::{
    AnyPriceSource, LivePoolConfig, LivePoolKind, LivePriceSource,
};
use peanut_internship_rust::strategy::scorer::{ScorerConfig, SignalScorer};
use peanut_internship_rust::strategy::signal::{Direction, Signal};
use serde_json::json;
use tokio::sync::{Mutex, mpsc, watch};

const ARBITRUM_UNISWAP_V3_SWAP_ROUTER: &str = "0xE592427A0AEce92De3Edee1F18E0157C05861564";
const ARBITRUM_UNISWAP_V3_QUOTER_V2: &str = "0x61fFE014bA17989E743c5F6cB21bF9697530B21e";
const ARBITRUM_NATIVE_USDC: &str = "0xaf88d065e77c8cC2239327C5EDb3A432268e5831";
const UNISWAP_V3_POOL_FEE_SELECTOR: [u8; 4] = [0xdd, 0xca, 0x3f, 0x43];
const DEFAULT_ARB_SIZE: &str = "0.1";
const DEFAULT_METRICS_PORT: u16 = 9090;
const DEFAULT_ALERT_PROVIDER: &str = "generic";
const DEFAULT_WALLET_KEY_ENV: &str = "WALLET_PRIVATE_KEY";
const DEFAULT_FLASHBOTS_AUTH_KEY_ENV: &str = "FLASHBOTS_AUTH_PRIVATE_KEY";
const DEFAULT_FEE_GAS_MODE: &str = "fixed";
const DEFAULT_TRADE_LOG_PATH: &str = "trades.jsonl";
const DEFAULT_EVENT_LOG_PATH: &str = "events.jsonl";
const DEFAULT_REBALANCE_CEX_WITHDRAW_NETWORK: &str = "ARBITRUM";
const DEFAULT_REBALANCE_TRANSFER_CONFIRM_TIMEOUT_SECS: u64 = 900;
const DEFAULT_REBALANCE_TRANSFER_CONFIRM_POLL_SECS: u64 = 15;
const ETH_SYMBOL: &str = "ETH";
const WETH_SYMBOL: &str = "WETH";
const USDC_SYMBOL: &str = "USDC";
const USDT_SYMBOL: &str = "USDT";
const USD_SYMBOL: &str = "USD";
#[cfg(test)]
const LINK_SYMBOL: &str = "LINK";
const ETH_USDC_PAIR: &str = "ETH/USDC";
const ARBITRUM_WETH_ADDRESS: &str = "0x82aF49447D8a07e3bd95BD0d56f35241523fBab1";
const REBALANCE_NATIVE_TRANSFER_GAS_LIMIT: u64 = 21_000;
const REBALANCE_TOKEN_TRANSFER_GAS_LIMIT: u64 = 100_000;
const REBALANCE_WETH_WRAP_GAS_LIMIT: u64 = 100_000;
const REBALANCE_WETH_WRAP_CONTEXT: &str = "rebalance WETH wrap";

/// CLI arguments.
#[derive(Debug, Parser)]
#[command(name = "arb_bot", about = "Cross-venue arbitrage bot")]
struct Cli {
    /// Trading pairs to watch (repeatable, or comma-separated via env).
    #[arg(long, default_values_t = vec![ETH_USDC_PAIR.to_string()], env = "PAIR", value_delimiter = ',')]
    pair: Vec<String>,

    /// Use Binance production credentials/endpoints. Env `PRODUCTION=true`
    /// also enables this mode.
    #[arg(long, default_value_t = false, env = "PRODUCTION")]
    production: bool,

    #[arg(long, default_value_t = false)]
    check_config: bool,

    /// Base-asset size per leg.
    #[arg(long, default_value = DEFAULT_ARB_SIZE, env = "ARB_SIZE")]
    size: String,

    /// Minimum score (0..=100) required to execute a signal.
    #[arg(long, default_value_t = DEFAULT_MIN_SCORE, env = "MIN_SCORE")]
    min_score: u32,

    /// Verbose logging: show all market probes even if no signal is found.
    #[arg(long, default_value_t = false, env = "VERBOSE")]
    verbose: bool,

    /// Loop interval in milliseconds.
    #[arg(long, default_value_t = DEFAULT_TICK_MS, env = "TICK_MS")]
    tick_ms: u64,

    /// Use the simulated leg backend instead of live exchange calls.
    #[arg(long, default_value_t = true, env = "SIMULATION", action = clap::ArgAction::Set)]
    simulation: bool,

    /// Port for the Prometheus `/metrics` endpoint. Set to 0 to disable.
    #[arg(long, default_value_t = DEFAULT_METRICS_PORT)]
    metrics_port: u16,

    /// Maximum concurrent executions. Default = 1 (safe; matches pre-queue
    /// behaviour). Safely raising above 1 requires inventory locking (see
    /// stretch goal S6 in `docs/STRETCH_GOALS.md`).
    #[arg(long, default_value_t = DEFAULT_MAX_CONCURRENT, env = "MAX_CONCURRENT")]
    max_concurrent_executions: usize,

    /// Maximum queue depth. When full, the weakest-score signal is evicted.
    #[arg(long, default_value_t = DEFAULT_QUEUE_MAX_SIZE)]
    queue_max_size: usize,

    /// SQLite path for persistent replay protection. Leave empty for
    /// in-memory only (default, matches pre-S8 behaviour).
    #[arg(long, default_value = "")]
    replay_db: String,

    /// Replay protection TTL in seconds. Ignored when `--replay-db` is empty.
    #[arg(long, default_value_t = DEFAULT_REPLAY_TTL_SECS)]
    replay_ttl_secs: u64,

    /// Ethereum RPC URL for on-chain wallet balance sync. Leave empty to
    /// skip (CEX-only inventory). Required when `--wallet-address` is set.
    /// Falls back to the `ETH_RPC_URL` environment variable if empty.
    #[arg(long, default_value = "", env = "ETH_RPC_URL")]
    eth_rpc_url: String,

    /// Wallet address to monitor for on-chain ERC-20 + ETH balances. Leave
    /// empty to skip (matches pre-S6 behaviour).
    /// Falls back to the `WALLET_ADDRESS` environment variable if empty.
    #[arg(long, default_value = "", env = "WALLET_ADDRESS")]
    wallet_address: String,

    /// Minimum interval (seconds) between full balance re-syncs. Prevents
    /// RPC / CEX hammering on tight tick intervals.
    #[arg(long, default_value_t = DEFAULT_BALANCE_SYNC_INTERVAL_SECS, env = "BALANCE_SYNC_INTERVAL")]
    pub balance_sync_interval_secs: u64,

    /// Tolerance (%) for post-trade balance verification. If the absolute
    /// difference between tracked and actual CEX balance exceeds this
    /// percentage, the bot emits a `BalanceMismatch` alert and halts.
    /// Set to 0 to disable verification.
    #[arg(long, default_value_t = DEFAULT_BALANCE_VERIFY_TOLERANCE_PCT)]
    balance_verify_tolerance_pct: f64,

    /// Webhook URL for alerting. Empty = alerts disabled (uses NoopSink).
    /// For Telegram, use `https://api.telegram.org/bot<TOKEN>/sendMessage`.
    #[arg(long, default_value = "", env = "TELEGRAM_WEBHOOK")]
    alert_webhook_url: String,

    /// Webhook payload format: `telegram` or `generic` (default).
    #[arg(long, default_value = DEFAULT_ALERT_PROVIDER, env = "ALERT_PROVIDER")]
    alert_provider: String,

    /// Telegram chat ID for alerts. Required when `--alert-provider telegram`.
    #[arg(long, default_value = "", env = "TELEGRAM_CHAT_ID")]
    alert_telegram_chat_id: String,

    /// Absolute-value loss (quote-asset units) that triggers a
    /// `LargeLoss` alert. Default 100 — tune to portfolio size.
    #[arg(long, default_value_t = DEFAULT_ALERT_LARGE_LOSS_USD)]
    alert_large_loss: u64,

    /// SQLite path for the reconcile store. When set, LEG2_TIMEOUT events
    /// with a known tx_hash are pushed into this database, and a background
    /// worker polls for receipts. Leave empty to disable (pre-S3 behaviour).
    #[arg(long, default_value = "")]
    reconcile_db: String,

    /// Reconcile worker polling interval in seconds.
    #[arg(long, default_value_t = DEFAULT_RECONCILE_POLL_SECS)]
    reconcile_poll_secs: u64,

    /// Maximum age (seconds) of a pending reconcile entry before it expires
    /// and is flagged for manual review.
    #[arg(long, default_value_t = DEFAULT_RECONCILE_MAX_AGE_SECS)]
    reconcile_max_age_secs: u64,

    /// Path to a JSON file mapping pair symbols to on-chain token addresses.
    /// Enables live DEX execution via UniswapV2Swapper. When absent in live
    /// mode, the DEX leg returns NotImplemented. Shape:
    ///   `{"ETH/USDC": {"base": "0x...", "base_decimals": 18, "quote": "0x...", "quote_decimals": 6}}`
    #[arg(long, default_value = "", env = "DEX_ADDRESS_BOOK")]
    dex_address_book: String,

    /// Slippage tolerance in basis points for DEX swaps.
    #[arg(long, default_value_t = DEFAULT_DEX_SLIPPAGE_BPS)]
    dex_slippage_bps: u64,

    /// DEX tx deadline in seconds from submission.
    #[arg(long, default_value_t = DEFAULT_DEX_DEADLINE_SECS)]
    dex_deadline_secs: u64,

    #[arg(long, default_value = "", env = "DEX_ROUTER")]
    dex_router: String,

    #[arg(long, default_value_t = ARBITRUM_CHAIN_ID, env = "DEX_CHAIN_ID")]
    dex_chain_id: u64,

    /// Maximum EIP-1559 maxFeePerGas for live DEX transactions, in gwei.
    /// Set to 0 to disable the cap.
    #[arg(long, default_value_t = DEFAULT_MAX_GAS_GWEI_CAP)]
    max_gas_gwei: u64,

    /// Environment variable name for the wallet private key used to sign
    /// DEX transactions. Only read when `--dex-address-book` is set.
    #[arg(long, default_value = DEFAULT_WALLET_KEY_ENV)]
    wallet_key_env: String,

    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    use_flashbots: bool,

    #[arg(long, default_value = DEFAULT_FLASHBOTS_RELAY_URL)]
    flashbots_relay_url: String,

    #[arg(long, default_value = DEFAULT_FLASHBOTS_AUTH_KEY_ENV)]
    flashbots_auth_key_env: String,

    #[arg(long, default_value_t = DEFAULT_FLASHBOTS_TARGET_BLOCK_OFFSET)]
    flashbots_target_block_offset: u64,

    #[arg(long, default_value_t = DEFAULT_FLASHBOTS_MAX_BLOCKS_TO_TRY)]
    flashbots_max_blocks_to_try: u64,

    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    require_private_dex: bool,

    /// Seed the inventory tracker with synthetic balances before the first
    /// tick. Intended for demos / integration tests running in
    /// `--simulation` mode, where `sync_cex_balance` would otherwise
    /// require Binance testnet credentials.
    ///
    /// Format: `venue:ASSET=AMOUNT[,ASSET=AMOUNT]...` where venue is
    /// `binance` or `wallet`. Repeatable. Example:
    ///   `--seed-inventory binance:USDT=10000,ETH=5 --seed-inventory wallet:ETH=2`
    #[arg(long, env = "SEED_INVENTORY")]
    seed_inventory: Vec<String>,

    /// Minimum net profit in quote-asset units required for the generator
    /// to emit a signal. Overrides `GeneratorConfig::default().min_profit_usd`
    /// (which is 5). Lower this for small-notional demos where default
    /// fees consume more than the achievable spread.
    #[arg(long, default_value_t = DEFAULT_MIN_PROFIT_USD.to_string(), env = "MIN_PROFIT_USD")]
    min_profit_usd: String,

    /// Minimum spread in basis points required to consider an opportunity.
    /// Overrides `GeneratorConfig::default().min_spread_bps` (50).
    #[arg(long, default_value_t = DEFAULT_MIN_SPREAD_BPS, env = "MIN_SPREAD_BPS")]
    min_spread_bps: u64,

    /// CEX taker fee in basis points. Drives both pre-trade profitability
    /// gating and post-trade realised PnL accounting (shared
    /// [`FeeStructure`]). Binance spot default = 10 bps.
    #[arg(long, default_value_t = DEFAULT_CEX_FEE_BPS, env = "FEE_CEX_TAKER_BPS")]
    fee_cex_taker_bps: u64,

    /// DEX swap fee in basis points. Uniswap V2 = 30 bps; Uniswap V3 tiers
    /// vary (5 / 30 / 100 / 1000 bps). Used in both generator and executor.
    #[arg(long, default_value_t = DEFAULT_DEX_FEE_BPS, env = "FEE_DEX_SWAP_BPS")]
    fee_dex_swap_bps: u64,

    /// Flat on-chain gas cost in USD per execution. Amortised per trade
    /// inside `FeeStructure::total_fee_bps` — small notionals pay a
    /// disproportionately higher %-ge. Default $5.
    #[arg(long, default_value_t = DEFAULT_GAS_COST_USD.to_string(), env = "FEE_GAS_USD")]
    fee_gas_usd: String,

    #[arg(long, default_value = DEFAULT_FEE_GAS_MODE, env = "FEE_GAS_MODE")]
    fee_gas_mode: String,

    #[arg(long, default_value_t = DEFAULT_ARB_GAS_UNITS, env = "FEE_GAS_UNITS")]
    fee_gas_units: u64,

    #[arg(long, default_value_t = DEFAULT_GAS_BUFFER_BPS, env = "FEE_GAS_BUFFER_BPS")]
    fee_gas_buffer_bps: u64,

    #[arg(long, default_value = "", env = "ANVIL_FORK_URL")]
    anvil_fork_url: String,

    /// Append-only structured trade log path. Completed executions are
    /// written as one JSON object per line. Leave empty to disable.
    #[arg(long, default_value = DEFAULT_TRADE_LOG_PATH, env = "TRADE_LOG_PATH")]
    trade_log_path: String,

    #[arg(long, default_value = DEFAULT_EVENT_LOG_PATH, env = "EVENT_LOG_PATH")]
    event_log_path: String,

    /// Maximum time to wait for queued/in-flight executions to finish during
    /// graceful shutdown.
    #[arg(long, default_value_t = DEFAULT_SHUTDOWN_DRAIN_SECS)]
    shutdown_drain_secs: u64,

    /// Maximum cumulative daily loss (USD) before the bot auto-halts.
    /// Set to 0 to disable. Default $100.
    #[arg(long, default_value_t = DEFAULT_MAX_DAILY_LOSS_USD.to_string(), env = "MAX_DAILY_LOSS")]
    max_daily_loss_usd: String,

    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    dry_run: bool,

    #[arg(long, default_value = DEFAULT_INITIAL_CAPITAL_USD, env = "INITIAL_CAPITAL_USD")]
    initial_capital_usd: String,

    #[arg(long, default_value = DEFAULT_RISK_MAX_TRADE_USD, env = "RISK_MAX_TRADE_USD")]
    risk_max_trade_usd: String,

    #[arg(long, default_value = DEFAULT_RISK_MAX_DAILY_LOSS_USD, env = "RISK_MAX_DAILY_LOSS_USD")]
    risk_max_daily_loss_usd: String,

    #[arg(long, default_value_t = DEFAULT_RISK_MAX_TRADES_PER_HOUR, env = "RISK_MAX_TRADES_PER_HOUR")]
    risk_max_trades_per_hour: u32,

    #[arg(long, default_value_t = DEFAULT_RISK_CONSECUTIVE_LOSS_LIMIT, env = "RISK_CONSECUTIVE_LOSS_LIMIT")]
    risk_consecutive_loss_limit: u32,

    #[arg(long, default_value = DEFAULT_LOG_DIR, env = "LOG_DIR")]
    log_dir: String,

    /// Path to a watchdog halt file. When this file exists, the bot halts
    /// immediately. Useful for emergency stops via `touch STOP`. Leave
    /// empty to disable.
    #[arg(long, default_value = DEFAULT_KILL_SWITCH_FILE, env = "HALT_FILE")]
    halt_file_path: String,

    /// Enable periodic inventory rebalance planning.
    #[arg(long, default_value_t = false, env = "REBALANCE_ENABLED")]
    rebalance_enabled: bool,

    /// Keep auto-rebalance in plan-only mode.
    #[arg(long, default_value_t = true, env = "REBALANCE_DRY_RUN")]
    rebalance_dry_run: bool,

    /// Seconds between rebalance checks.
    #[arg(long, default_value_t = DEFAULT_REBALANCE_INTERVAL_SECS, env = "REBALANCE_INTERVAL_SECS")]
    rebalance_interval_secs: u64,

    /// Maximum allowed inventory skew before a rebalance plan is generated.
    #[arg(long, default_value_t = REBALANCE_DEVIATION_THRESHOLD_PCT, env = "REBALANCE_THRESHOLD_PCT")]
    rebalance_threshold_pct: f64,

    /// Quote asset used for executable rebalance trade symbols.
    #[arg(long, default_value = DEFAULT_REBALANCE_QUOTE_ASSET, env = "REBALANCE_QUOTE_ASSET")]
    rebalance_quote_asset: String,

    /// Maximum slippage allowed for executable rebalance trade steps.
    #[arg(long, default_value_t = DEFAULT_REBALANCE_MAX_SLIPPAGE_BPS, env = "REBALANCE_MAX_SLIPPAGE_BPS")]
    rebalance_max_slippage_bps: u32,

    /// Destination wallet address for CEX→Wallet withdrawals. Defaults to signer/wallet address.
    #[arg(long, default_value = "", env = "REBALANCE_CEX_WITHDRAW_ADDRESS")]
    rebalance_cex_withdraw_address: String,

    /// Binance withdrawal network for CEX→Wallet withdrawals.
    #[arg(
        long,
        default_value = DEFAULT_REBALANCE_CEX_WITHDRAW_NETWORK,
        env = "REBALANCE_CEX_WITHDRAW_NETWORK"
    )]
    rebalance_cex_withdraw_network: String,

    /// CEX deposit address for Wallet→CEX transfers.
    #[arg(long, default_value = "", env = "REBALANCE_CEX_DEPOSIT_ADDRESS")]
    rebalance_cex_deposit_address: String,

    /// Chain ID used for Wallet→CEX transfer transactions.
    #[arg(long, default_value_t = ARBITRUM_CHAIN_ID, env = "REBALANCE_CHAIN_ID")]
    rebalance_chain_id: u64,

    #[arg(long, default_value = DEFAULT_REBALANCE_ALLOWED_ASSETS, env = "REBALANCE_ALLOWED_ASSETS")]
    rebalance_allowed_assets: String,

    #[arg(
        long,
        default_value = DEFAULT_REBALANCE_ALLOWED_VENUES,
        env = "REBALANCE_ALLOWED_VENUES"
    )]
    rebalance_allowed_venues: String,

    #[arg(long, default_value = DEFAULT_REBALANCE_MAX_STEP_USD, env = "REBALANCE_MAX_STEP_USD")]
    rebalance_max_step_usd: String,

    #[arg(long, default_value_t = DEFAULT_REBALANCE_MIN_FILL_PCT, env = "REBALANCE_MIN_FILL_PCT")]
    rebalance_min_fill_pct: f64,

    #[arg(long, default_value_t = true, env = "REBALANCE_PAUSE_TRADING")]
    rebalance_pause_trading: bool,

    #[arg(
        long,
        default_value = DEFAULT_REBALANCE_JOURNAL_PATH,
        env = "REBALANCE_JOURNAL_PATH"
    )]
    rebalance_journal_path: String,

    #[arg(
        long,
        default_value_t = DEFAULT_REBALANCE_TRANSFER_CONFIRM_TIMEOUT_SECS,
        env = "REBALANCE_TRANSFER_CONFIRM_TIMEOUT_SECS"
    )]
    rebalance_transfer_confirm_timeout_secs: u64,

    #[arg(
        long,
        default_value_t = DEFAULT_REBALANCE_TRANSFER_CONFIRM_POLL_SECS,
        env = "REBALANCE_TRANSFER_CONFIRM_POLL_SECS"
    )]
    rebalance_transfer_confirm_poll_secs: u64,
}

#[derive(Clone)]
struct RebalanceLoopConfig {
    dry_run: bool,
    interval: Duration,
    threshold_pct: f64,
    quote_asset: String,
    max_slippage_bps: Decimal,
    allowed_assets: Vec<String>,
    allowed_venues: Vec<String>,
    max_step_usd: Decimal,
    min_fill_pct: f64,
    pause_trading: bool,
    balance_verify_tolerance_pct: Decimal,
    journal_path: String,
}

struct RebalanceRuntime<'a> {
    inventory: &'a Arc<RwLock<InventoryTracker>>,
    transfer: Option<&'a RebalanceTransferContext>,
    alert_sink: &'a Arc<dyn AlertSink>,
    rebalance_pause: &'a Arc<Mutex<Option<String>>>,
    halt: &'a Arc<HaltCoordinator>,
    price_source: &'a Arc<AnyPriceSource>,
}

#[derive(Clone)]
struct RebalanceTransferContext {
    exchange: Arc<ExchangeClient>,
    chain: ChainClient,
    wallet: WalletManager,
    nonce_manager: NonceManager,
    cex_withdraw_address: String,
    cex_withdraw_network: String,
    cex_deposit_address: Address,
    token_book: HashMap<String, RebalanceTokenConfig>,
    chain_id: u64,
    max_gas_gwei: Option<u64>,
    transfer_confirm_timeout: Duration,
    transfer_confirm_poll: Duration,
}

type RebalanceTokenConfig = AddressBookTokenConfig;

struct WsCexOrderBookSource {
    exchange: Arc<ExchangeClient>,
    depth_snapshots: Arc<RwLock<HashMap<String, OrderBookSnapshot>>>,
    top_snapshots: Arc<RwLock<HashMap<String, OrderBookSnapshot>>>,
    max_age: Duration,
}

impl WsCexOrderBookSource {
    async fn new(exchange: Arc<ExchangeClient>, ws_url: String, pairs: &[String]) -> Self {
        let depth_snapshots = Arc::new(RwLock::new(HashMap::new()));
        let top_snapshots = Arc::new(RwLock::new(HashMap::new()));
        for pair in pairs {
            match exchange
                .fetch_order_book(pair, DEFAULT_ORDERBOOK_DEPTH)
                .await
            {
                Ok(snapshot) => {
                    depth_snapshots.write().await.insert(pair.clone(), snapshot);
                    info!(pair, "CEX order book seeded from REST snapshot");
                }
                Err(error) => {
                    warn!(pair, error = %error, "failed to seed CEX order book from REST");
                }
            }

            spawn_book_ticker_cache(ws_url.clone(), pair.clone(), Arc::clone(&top_snapshots));
            spawn_depth_cache(ws_url.clone(), pair.clone(), Arc::clone(&depth_snapshots));
        }

        Self {
            exchange,
            depth_snapshots,
            top_snapshots,
            max_age: Duration::from_secs(DEFAULT_BALANCE_SYNC_INTERVAL_SECS),
        }
    }
}

#[async_trait]
impl CexOrderBookSource for WsCexOrderBookSource {
    async fn fetch_order_book(
        &self,
        pair: &str,
        limit: u32,
    ) -> peanut_internship_rust::strategy::StrategyResult<OrderBookSnapshot> {
        let now = unix_millis();
        if let Some(snapshot) = self.depth_snapshots.read().await.get(pair).cloned() {
            if now.saturating_sub(snapshot.timestamp) <= self.max_age.as_millis() as u64 {
                return Ok(snapshot);
            }
            warn!(pair, "CEX depth snapshot stale; falling back to REST");
        }

        match self.exchange.fetch_order_book(pair, limit).await {
            Ok(snapshot) => Ok(snapshot),
            Err(error) => {
                if let Some(snapshot) = self.top_snapshots.read().await.get(pair).cloned()
                    && now.saturating_sub(snapshot.timestamp) <= self.max_age.as_millis() as u64
                {
                    warn!(pair, error = %error, "CEX REST depth failed; falling back to bookTicker top-of-book");
                    return Ok(snapshot);
                }
                Err(error.into())
            }
        }
    }
}

fn spawn_depth_cache(
    ws_url: String,
    pair: String,
    snapshots: Arc<RwLock<HashMap<String, OrderBookSnapshot>>>,
) {
    tokio::spawn(async move {
        loop {
            let mut book = {
                let existing = snapshots.read().await.get(&pair).cloned();
                existing
                    .as_ref()
                    .map(local_book_from_snapshot)
                    .unwrap_or_else(|| LocalOrderBook::new(&pair))
            };

            match subscribe_depth_stream(&ws_url, &pair).await {
                Ok(mut rx) => {
                    info!(pair, "CEX depth stream connected");
                    while let Some(event) = rx.recv().await {
                        match event {
                            DepthEvent::Snapshot(snapshot) => {
                                book.apply_snapshot(snapshot);
                                snapshots
                                    .write()
                                    .await
                                    .insert(pair.clone(), book.snapshot());
                            }
                            DepthEvent::Update(update) => match book.apply_update(update) {
                                SequenceStatus::Applied => {
                                    snapshots
                                        .write()
                                        .await
                                        .insert(pair.clone(), book.snapshot());
                                }
                                SequenceStatus::Stale => {}
                                SequenceStatus::NeedsReconnect => {
                                    warn!(pair, "CEX depth sequence gap; reconnecting");
                                    break;
                                }
                            },
                        }
                    }
                    warn!(pair, "CEX depth stream ended; reconnecting");
                }
                Err(error) => {
                    warn!(pair, error = %error, "CEX depth stream connect failed");
                }
            }
            tokio::time::sleep(Duration::from_secs(DEFAULT_RECONNECT_DELAY_SECS)).await;
        }
    });
}

fn local_book_from_snapshot(snapshot: &OrderBookSnapshot) -> LocalOrderBook {
    let mut book = LocalOrderBook::new(&snapshot.symbol);
    book.apply_snapshot(DepthSnapshot {
        last_update_id: 0,
        bids: snapshot
            .bids
            .iter()
            .map(|(price, qty)| [price.to_string(), qty.to_string()])
            .collect(),
        asks: snapshot
            .asks
            .iter()
            .map(|(price, qty)| [price.to_string(), qty.to_string()])
            .collect(),
    });
    book
}

fn spawn_rebalance_loop(
    inventory: Arc<RwLock<InventoryTracker>>,
    halt: Arc<HaltCoordinator>,
    alert_sink: Arc<dyn AlertSink>,
    rebalance_pause: Arc<Mutex<Option<String>>>,
    price_source: Arc<AnyPriceSource>,
    config: RebalanceLoopConfig,
    transfer: Option<RebalanceTransferContext>,
) {
    tokio::spawn(async move {
        let mut active_rebalance_key = None::<String>;
        info!(
            dry_run = config.dry_run,
            interval_s = config.interval.as_secs(),
            threshold_pct = config.threshold_pct,
            quote_asset = %config.quote_asset,
            max_slippage_bps = %config.max_slippage_bps,
            "auto-rebalance loop enabled"
        );
        loop {
            if halt.is_halted() {
                info!("auto-rebalance loop stopped because bot is halted");
                break;
            }

            run_rebalance_check(
                RebalanceRuntime {
                    inventory: &inventory,
                    transfer: transfer.as_ref(),
                    alert_sink: &alert_sink,
                    rebalance_pause: &rebalance_pause,
                    halt: &halt,
                    price_source: &price_source,
                },
                &config,
                &mut active_rebalance_key,
            )
            .await;
            tokio::time::sleep(config.interval).await;
        }
    });
}

async fn run_rebalance_check(
    runtime: RebalanceRuntime<'_>,
    config: &RebalanceLoopConfig,
    active_rebalance_key: &mut Option<String>,
) {
    let tracker_snapshot = runtime.inventory.read().await.clone();
    let planner = RebalancePlanner::new(tracker_snapshot, config.threshold_pct);
    let plans = planner.plan_executable_all(&config.quote_asset, config.max_slippage_bps);
    if plans.is_empty() {
        *active_rebalance_key = None;
        debug!("auto-rebalance check complete: no rebalance needed");
        return;
    }

    let total_steps: usize = plans.values().map(Vec::len).sum();
    info!(
        assets = plans.len(),
        steps = total_steps,
        dry_run = config.dry_run,
        "auto-rebalance plan generated"
    );
    let plan_key = rebalance_plan_key(&plans);
    if active_rebalance_key.as_deref() != Some(plan_key.as_str()) {
        *active_rebalance_key = Some(plan_key.clone());
        append_rebalance_journal(
            &config.journal_path,
            json!({
                "event": "plan_generated",
                "plan_key": plan_key,
                "assets": plans.len(),
                "steps": total_steps,
                "dry_run": config.dry_run,
                "quote_asset": config.quote_asset,
            }),
        );
        emit_best_effort(
            runtime.alert_sink,
            &AlertEvent::RebalanceTriggered {
                assets: plans.len(),
                steps: total_steps,
                dry_run: config.dry_run,
                quote_asset: config.quote_asset.clone(),
            },
        )
        .await;
    }

    let mut trading_paused = false;
    if !config.dry_run && config.pause_trading {
        let reason = format!("rebalance active plan={plan_key}");
        *runtime.rebalance_pause.lock().await = Some(reason.clone());
        append_rebalance_journal(
            &config.journal_path,
            json!({
                "event": "trading_paused",
                "plan_key": plan_key,
                "reason": reason,
            }),
        );
        trading_paused = true;
    }

    for (asset, steps) in plans {
        for step in steps {
            let validation = validate_rebalance_step(&step, config, runtime.price_source).await;
            if let Err(error) = validation {
                warn!(error = %error, "auto-rebalance step rejected by safety guard");
                append_rebalance_journal(
                    &config.journal_path,
                    json!({
                        "event": "step_rejected",
                        "plan_key": plan_key,
                        "asset": asset,
                        "step": rebalance_step_label(&step),
                        "reason": error,
                    }),
                );
                emit_best_effort(
                    runtime.alert_sink,
                    &AlertEvent::RebalanceStepFailed {
                        asset: asset.clone(),
                        step: rebalance_step_label(&step),
                        reason: error,
                    },
                )
                .await;
                if !config.dry_run {
                    if trading_paused {
                        clear_rebalance_pause(
                            runtime.rebalance_pause,
                            &config.journal_path,
                            &plan_key,
                        )
                        .await;
                    }
                    return;
                }
                continue;
            }
            match &step {
                RebalanceStep::Trade(trade) => {
                    info!(
                        asset,
                        venue = %trade.venue,
                        symbol = %trade.symbol,
                        side = %trade.side,
                        amount = %trade.amount,
                        max_slippage_bps = %trade.max_slippage_bps,
                        "auto-rebalance trade step planned"
                    );
                }
                RebalanceStep::Withdraw(withdraw) => {
                    info!(
                        asset,
                        from = %withdraw.from_venue,
                        to = %withdraw.to_venue,
                        amount = %withdraw.amount,
                        fee = %withdraw.fee,
                        "auto-rebalance withdraw step planned"
                    );
                }
            }
            if !config.dry_run {
                append_rebalance_journal(
                    &config.journal_path,
                    json!({
                        "event": "step_started",
                        "plan_key": plan_key,
                        "asset": asset,
                        "step": rebalance_step_label(&step),
                    }),
                );
                match execute_rebalance_step(
                    &step,
                    runtime.transfer,
                    runtime.inventory,
                    config.min_fill_pct,
                )
                .await
                {
                    Ok(reference) => {
                        info!(reference, "auto-rebalance step submitted");
                        append_rebalance_journal(
                            &config.journal_path,
                            json!({
                                "event": "step_completed",
                                "plan_key": plan_key,
                                "asset": asset,
                                "step": rebalance_step_label(&step),
                                "reference": reference,
                            }),
                        );
                        emit_best_effort(
                            runtime.alert_sink,
                            &AlertEvent::RebalanceStepCompleted {
                                asset: asset.clone(),
                                step: rebalance_step_label(&step),
                                reference,
                            },
                        )
                        .await;
                        if let Err(error) = verify_rebalance_after_step(
                            &step,
                            runtime.transfer,
                            runtime.inventory,
                            runtime.alert_sink,
                            runtime.halt,
                            &config.journal_path,
                            config.balance_verify_tolerance_pct,
                        )
                        .await
                        {
                            warn!(error = %error, "auto-rebalance post-step verification failed");
                            append_rebalance_journal(
                                &config.journal_path,
                                json!({
                                    "event": "post_step_verify_failed",
                                    "plan_key": plan_key,
                                    "asset": asset,
                                    "step": rebalance_step_label(&step),
                                    "reason": error,
                                }),
                            );
                            if trading_paused {
                                clear_rebalance_pause(
                                    runtime.rebalance_pause,
                                    &config.journal_path,
                                    &plan_key,
                                )
                                .await;
                            }
                            return;
                        }
                    }
                    Err(error) => {
                        warn!(error = %error, "auto-rebalance step execution failed");
                        append_rebalance_journal(
                            &config.journal_path,
                            json!({
                                "event": "step_failed",
                                "plan_key": plan_key,
                                "asset": asset,
                                "step": rebalance_step_label(&step),
                                "reason": error,
                            }),
                        );
                        emit_best_effort(
                            runtime.alert_sink,
                            &AlertEvent::RebalanceStepFailed {
                                asset: asset.clone(),
                                step: rebalance_step_label(&step),
                                reason: error,
                            },
                        )
                        .await;
                        if trading_paused {
                            clear_rebalance_pause(
                                runtime.rebalance_pause,
                                &config.journal_path,
                                &plan_key,
                            )
                            .await;
                        }
                        return;
                    }
                }
            }
        }
    }
    if trading_paused {
        clear_rebalance_pause(runtime.rebalance_pause, &config.journal_path, &plan_key).await;
    }
}

async fn execute_rebalance_step(
    step: &RebalanceStep,
    transfer: Option<&RebalanceTransferContext>,
    inventory: &Arc<RwLock<InventoryTracker>>,
    min_fill_pct: f64,
) -> Result<String, String> {
    let Some(transfer) = transfer else {
        return Err("real rebalance transfer context is not configured".into());
    };
    match step {
        RebalanceStep::Trade(trade) => {
            execute_rebalance_trade(trade, transfer, inventory, min_fill_pct).await
        }
        RebalanceStep::Withdraw(withdraw) => {
            if withdraw.from_venue.is_cex() && withdraw.to_venue == Venue::Wallet {
                execute_cex_to_wallet_withdraw(withdraw, transfer).await
            } else if withdraw.from_venue == Venue::Wallet && withdraw.to_venue.is_cex() {
                execute_wallet_to_cex_transfer(withdraw, transfer).await
            } else {
                Err(format!(
                    "unsupported rebalance withdraw route {} -> {}",
                    withdraw.from_venue, withdraw.to_venue
                ))
            }
        }
    }
}

fn rebalance_plan_key(plans: &HashMap<String, Vec<RebalanceStep>>) -> String {
    let mut parts = Vec::new();
    let mut assets: Vec<_> = plans.keys().collect();
    assets.sort();
    for asset in assets {
        if let Some(steps) = plans.get(asset) {
            let mut step_labels: Vec<_> = steps.iter().map(rebalance_step_label).collect();
            step_labels.sort();
            parts.push(format!("{asset}:{}", step_labels.join(",")));
        }
    }
    parts.join("|")
}

fn rebalance_step_label(step: &RebalanceStep) -> String {
    match step {
        RebalanceStep::Trade(trade) => format!(
            "trade:{}:{}:{}:{}",
            trade.venue, trade.symbol, trade.side, trade.amount
        ),
        RebalanceStep::Withdraw(withdraw) => format!(
            "withdraw:{}->{}:{}:{}",
            withdraw.from_venue, withdraw.to_venue, withdraw.asset, withdraw.amount
        ),
    }
}

async fn clear_rebalance_pause(
    rebalance_pause: &Arc<Mutex<Option<String>>>,
    journal_path: &str,
    plan_key: &str,
) {
    let reason = rebalance_pause.lock().await.take();
    append_rebalance_journal(
        journal_path,
        json!({
            "event": "trading_resumed",
            "plan_key": plan_key,
            "reason": reason,
        }),
    );
}

fn append_rebalance_journal<T: Serialize>(path: &str, payload: T) {
    if path.trim().is_empty() {
        return;
    }
    let mut value = serde_json::to_value(payload).unwrap_or_else(|_| json!({}));
    if let Some(object) = value.as_object_mut() {
        object.insert("ts".into(), json!(Utc::now().to_rfc3339()));
    }
    if let Some(parent) = Path::new(path).parent()
        && let Err(error) = fs::create_dir_all(parent)
    {
        warn!(path, error = %error, "failed to create rebalance journal directory");
        return;
    }
    match OpenOptions::new().create(true).append(true).open(path) {
        Ok(mut file) => {
            if let Err(error) = writeln!(file, "{value}") {
                warn!(path, error = %error, "failed to write rebalance journal event");
            }
        }
        Err(error) => warn!(path, error = %error, "failed to open rebalance journal"),
    }
}

async fn validate_rebalance_step(
    step: &RebalanceStep,
    config: &RebalanceLoopConfig,
    price_source: &Arc<AnyPriceSource>,
) -> Result<(), String> {
    if !(0.0..=1.0).contains(&config.min_fill_pct) {
        return Err(format!(
            "REBALANCE_MIN_FILL_PCT must be between 0 and 1, got {}",
            config.min_fill_pct
        ));
    }
    validate_rebalance_assets(step, &config.allowed_assets)?;
    validate_rebalance_venues(step, &config.allowed_venues)?;
    if config.max_step_usd > Decimal::ZERO {
        let notional_usd = rebalance_step_notional_usd(step, price_source).await?;
        if notional_usd > config.max_step_usd {
            return Err(format!(
                "rebalance step notional ${notional_usd} exceeds max ${}",
                config.max_step_usd
            ));
        }
    }
    Ok(())
}

fn validate_rebalance_assets(step: &RebalanceStep, allowed: &[String]) -> Result<(), String> {
    match step {
        RebalanceStep::Trade(trade) => {
            for asset in [&trade.base_asset, &trade.quote_asset] {
                if !contains_ignore_ascii_case(allowed, asset) {
                    return Err(format!("rebalance asset {asset} is not allowlisted"));
                }
            }
        }
        RebalanceStep::Withdraw(withdraw) => {
            if !contains_ignore_ascii_case(allowed, &withdraw.asset) {
                return Err(format!(
                    "rebalance asset {} is not allowlisted",
                    withdraw.asset
                ));
            }
        }
    }
    Ok(())
}

fn validate_rebalance_venues(step: &RebalanceStep, allowed: &[String]) -> Result<(), String> {
    match step {
        RebalanceStep::Trade(trade) => {
            if !contains_ignore_ascii_case(allowed, &trade.venue.to_string()) {
                return Err(format!(
                    "rebalance venue {} is not allowlisted",
                    trade.venue
                ));
            }
        }
        RebalanceStep::Withdraw(withdraw) => {
            for venue in [withdraw.from_venue, withdraw.to_venue] {
                if !contains_ignore_ascii_case(allowed, &venue.to_string()) {
                    return Err(format!("rebalance venue {venue} is not allowlisted"));
                }
            }
        }
    }
    Ok(())
}

async fn rebalance_step_notional_usd(
    step: &RebalanceStep,
    price_source: &Arc<AnyPriceSource>,
) -> Result<Decimal, String> {
    match step {
        RebalanceStep::Trade(trade) => {
            let price = asset_usd_price(&trade.base_asset, price_source).await?;
            Ok(trade.amount * price)
        }
        RebalanceStep::Withdraw(withdraw) => {
            let price = asset_usd_price(&withdraw.asset, price_source).await?;
            Ok(withdraw.amount * price)
        }
    }
}

async fn asset_usd_price(
    asset: &str,
    price_source: &Arc<AnyPriceSource>,
) -> Result<Decimal, String> {
    if asset.eq_ignore_ascii_case(USDC_SYMBOL)
        || asset.eq_ignore_ascii_case(USDT_SYMBOL)
        || asset.eq_ignore_ascii_case(USD_SYMBOL)
    {
        return Ok(Decimal::ONE);
    }
    let priced_asset = rebalance_pricing_asset(asset);
    let direct_pair = format!("{}/{}", priced_asset, USDC_SYMBOL);
    if let Ok(price) = price_source.get_latest_price(&direct_pair).await {
        return Ok(price);
    }
    if priced_asset == ETH_SYMBOL {
        return Err("failed to price ETH via ETH/USDC for rebalance max-step guard".into());
    }
    let eth_pair = format!("{}/ETH", priced_asset);
    let asset_eth = price_source
        .get_latest_price(&eth_pair)
        .await
        .map_err(|e| format!("failed to price {asset} via {eth_pair}: {e}"))?;
    let eth_usd = price_source
        .get_latest_price(ETH_USDC_PAIR)
        .await
        .map_err(|e| format!("failed to price ETH via ETH/USDC: {e}"))?;
    Ok(asset_eth * eth_usd)
}

fn rebalance_pricing_asset(asset: &str) -> String {
    if asset.eq_ignore_ascii_case(WETH_SYMBOL) {
        ETH_SYMBOL.to_string()
    } else {
        asset.to_ascii_uppercase()
    }
}

fn contains_ignore_ascii_case(values: &[String], needle: &str) -> bool {
    values
        .iter()
        .any(|value| value.eq_ignore_ascii_case(needle))
}

async fn verify_rebalance_after_step(
    step: &RebalanceStep,
    transfer: Option<&RebalanceTransferContext>,
    inventory: &Arc<RwLock<InventoryTracker>>,
    alert_sink: &Arc<dyn AlertSink>,
    halt: &Arc<HaltCoordinator>,
    journal_path: &str,
    tolerance_pct: Decimal,
) -> Result<(), String> {
    if tolerance_pct <= Decimal::ZERO {
        return Ok(());
    }
    match step {
        RebalanceStep::Trade(_) => {
            let Some(transfer) = transfer else {
                return Err(
                    "rebalance transfer context missing for post-trade verification".into(),
                );
            };
            let mismatches = verify_cex_balance(&transfer.exchange, inventory, tolerance_pct)
                .await
                .ok_or_else(|| {
                    "failed to fetch CEX balances for rebalance verification".to_string()
                })?;
            if mismatches.is_empty() {
                append_rebalance_journal(
                    journal_path,
                    json!({
                        "event": "post_step_verified",
                        "step": rebalance_step_label(step),
                        "venue": "binance",
                    }),
                );
                return Ok(());
            }
            for mismatch in mismatches {
                append_rebalance_journal(
                    journal_path,
                    json!({
                        "event": "balance_mismatch",
                        "venue": mismatch.venue.to_string(),
                        "asset": mismatch.asset,
                        "tracked": mismatch.tracked.to_string(),
                        "actual": mismatch.actual.to_string(),
                        "diff": mismatch.diff.to_string(),
                    }),
                );
                emit_best_effort(
                    alert_sink,
                    &AlertEvent::BalanceMismatch {
                        venue: mismatch.venue.to_string(),
                        asset: mismatch.asset,
                        tracked: mismatch.tracked.to_string(),
                        actual: mismatch.actual.to_string(),
                        diff: mismatch.diff.to_string(),
                    },
                )
                .await;
            }
            halt.halt("rebalance post-step balance mismatch");
            Err("rebalance post-step balance mismatch; bot halted".into())
        }
        RebalanceStep::Withdraw(withdraw) => {
            append_rebalance_journal(
                journal_path,
                json!({
                    "event": "post_step_verified",
                    "step": rebalance_step_label(step),
                    "asset": withdraw.asset,
                    "venue": "transfer",
                }),
            );
            Ok(())
        }
    }
}

fn parse_rebalance_csv_upper(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_uppercase())
        .collect()
}

fn parse_rebalance_csv_lower(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase())
        .collect()
}

async fn execute_rebalance_trade(
    trade: &peanut_internship_rust::inventory::types::TradeStep,
    transfer: &RebalanceTransferContext,
    inventory: &Arc<RwLock<InventoryTracker>>,
    min_fill_pct: f64,
) -> Result<String, String> {
    if trade.venue != Venue::Binance {
        return Err(format!(
            "real CEX rebalance trade only supports Binance, got {}",
            trade.venue
        ));
    }
    let order_book = transfer
        .exchange
        .fetch_order_book(&trade.symbol, DEFAULT_ORDERBOOK_DEPTH)
        .await
        .map_err(|e| format!("failed to fetch rebalance order book: {e}"))?;
    let analyzer = OrderBookAnalyzer::new(order_book);
    let walk = analyzer
        .walk_the_book(&trade.side.to_lowercase(), trade.amount)
        .map_err(|e| format!("rebalance book walk failed: {e}"))?;
    if walk.slippage_bps > trade.max_slippage_bps {
        return Err(format!(
            "rebalance slippage {} bps exceeds limit {} bps",
            walk.slippage_bps, trade.max_slippage_bps
        ));
    }
    let price = match trade.side.as_str() {
        "BUY" => analyzer
            .orderbook()
            .best_ask
            .map(|(price, _)| price)
            .unwrap_or(walk.avg_price),
        "SELL" => analyzer
            .orderbook()
            .best_bid
            .map(|(price, _)| price)
            .unwrap_or(walk.avg_price),
        other => return Err(format!("unsupported rebalance trade side: {other}")),
    };
    let amount_f = trade.amount.to_f64().ok_or_else(|| {
        format!(
            "rebalance amount {} cannot be represented as f64",
            trade.amount
        )
    })?;
    let price_f = price
        .to_f64()
        .ok_or_else(|| format!("rebalance price {price} cannot be represented as f64"))?;
    let order = transfer
        .exchange
        .create_limit_ioc_order(&trade.symbol, &trade.side, amount_f, price_f)
        .await
        .map_err(|e| e.to_string())?;
    if order.amount_filled <= Decimal::ZERO {
        return Err(format!(
            "rebalance order did not fill: status={}, order_id={}",
            order.status, order.id
        ));
    }
    let min_fill_ratio = Decimal::from_f64_retain(min_fill_pct)
        .ok_or_else(|| format!("invalid rebalance min fill pct: {min_fill_pct}"))?;
    let min_fill = trade.amount * min_fill_ratio;
    if order.amount_filled < min_fill {
        return Err(format!(
            "rebalance order underfilled: filled={}, required={}, order_id={}",
            order.amount_filled, min_fill, order.id
        ));
    }
    let fee_asset = if order.fee_asset.is_empty() {
        trade.quote_asset.as_str()
    } else {
        order.fee_asset.as_str()
    };
    inventory
        .write()
        .await
        .record_trade(
            trade.venue,
            &trade.side.to_lowercase(),
            &trade.base_asset,
            &trade.quote_asset,
            order.amount_filled,
            order.amount_filled * order.avg_fill_price,
            order.fee,
            fee_asset,
        )
        .map_err(|e| format!("failed to record rebalance trade in inventory: {e}"))?;
    Ok(format!(
        "binance_order:{} status={} filled={} avg_price={}",
        order.id, order.status, order.amount_filled, order.avg_fill_price
    ))
}

async fn execute_cex_to_wallet_withdraw(
    withdraw: &peanut_internship_rust::inventory::types::WithdrawStep,
    transfer: &RebalanceTransferContext,
) -> Result<String, String> {
    let send_amount = withdraw.amount - withdraw.fee;
    if send_amount <= Decimal::ZERO {
        return Err(format!(
            "withdraw amount {} is not greater than fee {}",
            withdraw.amount, withdraw.fee
        ));
    }
    if withdraw.asset.eq_ignore_ascii_case(WETH_SYMBOL)
        && !transfer
            .cex_withdraw_address
            .eq_ignore_ascii_case(&transfer.wallet.address())
    {
        return Err("WETH rebalance requires Binance withdrawal address to be the signer wallet so native ETH can be wrapped after receipt".into());
    }
    verify_binance_withdrawal_capability(transfer, withdraw, send_amount).await?;
    let cex_asset = rebalance_cex_asset(&withdraw.asset);
    let withdraw_id = transfer
        .exchange
        .withdraw_to_address(
            &cex_asset,
            &transfer.cex_withdraw_address,
            send_amount,
            &transfer.cex_withdraw_network,
        )
        .await
        .map_err(|e| e.to_string())?;
    let record = wait_for_binance_withdrawal(transfer, &cex_asset, &withdraw_id).await?;
    let mut reference = format!(
        "binance_withdraw:{} status={} tx_id={}",
        withdraw_id,
        record.status,
        record.tx_id.unwrap_or_default()
    );
    if withdraw.asset.eq_ignore_ascii_case(WETH_SYMBOL) {
        let wrap_tx = wrap_native_eth_for_rebalance(withdraw, transfer, send_amount).await?;
        reference.push_str(&format!(" wrapped_weth_tx={wrap_tx}"));
    }
    Ok(reference)
}

async fn verify_binance_withdrawal_capability(
    transfer: &RebalanceTransferContext,
    withdraw: &peanut_internship_rust::inventory::types::WithdrawStep,
    send_amount: Decimal,
) -> Result<(), String> {
    let configs = transfer
        .exchange
        .fetch_capital_config()
        .await
        .map_err(|e| format!("failed to fetch Binance capital config: {e}"))?;
    validate_binance_withdrawal_capability(
        &configs,
        withdraw,
        send_amount,
        &transfer.cex_withdraw_network,
    )
}

fn validate_binance_withdrawal_capability(
    configs: &[peanut_internship_rust::exchange::CapitalCoinConfig],
    withdraw: &peanut_internship_rust::inventory::types::WithdrawStep,
    send_amount: Decimal,
    expected_network: &str,
) -> Result<(), String> {
    let cex_asset = rebalance_cex_asset(&withdraw.asset);
    let coin = configs
        .iter()
        .find(|coin| coin.coin.eq_ignore_ascii_case(&cex_asset))
        .ok_or_else(|| format!("Binance capital config missing coin {cex_asset}"))?;
    let network = coin
        .networks
        .iter()
        .find(|network| network.network.eq_ignore_ascii_case(expected_network))
        .ok_or_else(|| {
            let available = coin
                .networks
                .iter()
                .map(|network| network.network.as_str())
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "Binance coin {} does not support withdrawal network {}; available networks: {}",
                cex_asset, expected_network, available
            )
        })?;
    if !network.withdraw_enable {
        return Err(format!(
            "Binance withdrawal disabled for {} on network {}",
            cex_asset, network.network
        ));
    }
    if send_amount < network.withdraw_min {
        return Err(format!(
            "Binance withdrawal amount {} below minimum {} for {} on {}",
            send_amount, network.withdraw_min, cex_asset, network.network
        ));
    }
    if withdraw.fee < network.withdraw_fee {
        return Err(format!(
            "rebalance configured withdrawal fee {} below Binance fee {} for {} on {}",
            withdraw.fee, network.withdraw_fee, cex_asset, network.network
        ));
    }
    Ok(())
}

fn rebalance_cex_asset(asset: &str) -> String {
    if asset.eq_ignore_ascii_case(WETH_SYMBOL) {
        ETH_SYMBOL.to_string()
    } else {
        asset.to_ascii_uppercase()
    }
}

fn rebalance_weth_token(
    transfer: &RebalanceTransferContext,
) -> Result<&RebalanceTokenConfig, String> {
    transfer
        .token_book
        .get(WETH_SYMBOL)
        .or_else(|| transfer.token_book.get(ETH_SYMBOL))
        .ok_or_else(|| "asset WETH missing from rebalance token book".to_string())
}

fn rebalance_weth_token_from_book(
    token_book: &HashMap<String, RebalanceTokenConfig>,
) -> Result<&RebalanceTokenConfig, String> {
    token_book
        .get(WETH_SYMBOL)
        .or_else(|| token_book.get(ETH_SYMBOL))
        .ok_or_else(|| "asset WETH missing from rebalance token book".to_string())
}

fn validate_rebalance_weth_transfer_config(
    allowed_assets: &[String],
    token_book: &HashMap<String, RebalanceTokenConfig>,
    chain_id: u64,
    cex_withdraw_address: &str,
    wallet_address: &str,
) -> Result<(), String> {
    if !allowed_assets
        .iter()
        .any(|asset| asset.eq_ignore_ascii_case(WETH_SYMBOL))
    {
        return Ok(());
    }
    if chain_id != ARBITRUM_CHAIN_ID {
        return Err(format!(
            "WETH rebalance auto-wrap is currently supported only on Arbitrum chain {ARBITRUM_CHAIN_ID}, got {chain_id}"
        ));
    }
    if !cex_withdraw_address.eq_ignore_ascii_case(wallet_address) {
        return Err("WETH rebalance requires Binance withdrawal address to be the signer wallet so native ETH can be wrapped after receipt".to_string());
    }
    let token = rebalance_weth_token_from_book(token_book)?;
    if token.decimals != ETH_DECIMALS {
        return Err(format!(
            "rebalance WETH token decimals {} do not match expected {}",
            token.decimals, ETH_DECIMALS
        ));
    }
    let expected_weth =
        Address::new(ARBITRUM_WETH_ADDRESS).map_err(|e| format!("invalid WETH constant: {e}"))?;
    if token.address != expected_weth {
        return Err(format!(
            "rebalance WETH token address {} does not match Arbitrum WETH {}",
            token.address, expected_weth
        ));
    }
    Ok(())
}

fn native_balance_required_for_value_and_gas(
    value: U256,
    max_fee_per_gas: U256,
    gas_limit: u64,
) -> U256 {
    value.saturating_add(max_fee_per_gas.saturating_mul(U256::from(gas_limit)))
}

async fn wait_for_native_balance_for_rebalance(
    transfer: &RebalanceTransferContext,
    address: &Address,
    value: U256,
    max_fee_per_gas: U256,
    gas_limit: u64,
    context: &str,
) -> Result<(), String> {
    let required = native_balance_required_for_value_and_gas(value, max_fee_per_gas, gas_limit);
    let start = std::time::Instant::now();
    loop {
        let balance = transfer
            .chain
            .get_balance(address)
            .await
            .map_err(|e| format!("{context} failed to fetch native ETH balance: {e}"))?;
        if balance.raw >= required {
            return Ok(());
        }
        if start.elapsed() >= transfer.transfer_confirm_timeout {
            let current = balance.human().map_err(|e| e.to_string())?;
            let required_human = TokenAmount::eth(required)
                .human()
                .map_err(|e| e.to_string())?;
            return Err(format!(
                "{context} timed out waiting for native ETH balance: current {current}, required {required_human}"
            ));
        }
        warn!(
            context,
            current_wei = %balance.raw,
            required_wei = %required,
            poll_secs = transfer.transfer_confirm_poll.as_secs(),
            "waiting for native ETH balance before rebalance on-chain transaction"
        );
        tokio::time::sleep(transfer.transfer_confirm_poll).await;
    }
}

async fn wrap_native_eth_for_rebalance(
    withdraw: &peanut_internship_rust::inventory::types::WithdrawStep,
    transfer: &RebalanceTransferContext,
    amount: Decimal,
) -> Result<String, String> {
    let from = Address::new(transfer.wallet.address()).map_err(|e| e.to_string())?;
    let token = rebalance_weth_token(transfer)?;
    if token.decimals != ETH_DECIMALS {
        return Err(format!(
            "rebalance WETH token decimals {} do not match expected {}",
            token.decimals, ETH_DECIMALS
        ));
    }
    let wrap_value = TokenAmount::from_human(amount, ETH_DECIMALS, Some(ETH_SYMBOL.to_string()))
        .map_err(|e| e.to_string())?;
    let gas = transfer
        .chain
        .get_gas_price()
        .await
        .map_err(|e| e.to_string())?;
    let max_priority_fee = gas.priority_fee_medium;
    let max_fee = gas.get_max_fee(GasPriority::Medium, DEFAULT_GAS_BUFFER_BPS);
    if let Some(max_gas_gwei) = transfer.max_gas_gwei {
        let cap = U256::from(max_gas_gwei) * U256::from(WEI_PER_GWEI);
        if max_fee > cap {
            return Err(format!(
                "rebalance WETH wrap max_fee_per_gas {} exceeds cap {}",
                max_fee, cap
            ));
        }
    }
    let mut tx = TransactionRequest {
        to: token.address.clone(),
        value: wrap_value.clone(),
        data: Bytes::from(WETH_DEPOSIT_SELECTOR.to_vec()),
        nonce: None,
        gas_limit: Some(REBALANCE_WETH_WRAP_GAS_LIMIT),
        max_fee_per_gas: Some(max_fee),
        max_priority_fee: Some(max_priority_fee),
        chain_id: transfer.chain_id,
    };
    wait_for_native_balance_for_rebalance(
        transfer,
        &from,
        wrap_value.raw,
        max_fee,
        tx.gas_limit.unwrap_or(REBALANCE_WETH_WRAP_GAS_LIMIT),
        REBALANCE_WETH_WRAP_CONTEXT,
    )
    .await?;
    match transfer.chain.estimate_gas(&tx).await {
        Ok(estimated) => {
            let buffered: U256 = U256::from(estimated)
                .saturating_mul(U256::from(DEFAULT_GAS_BUFFER_BPS))
                / U256::from(BPS_SCALE);
            tx.gas_limit = Some(buffered.as_u64().max(MIN_GAS_LIMIT));
        }
        Err(error) => {
            warn!(
                error = %error,
                asset = %withdraw.asset,
                amount = %amount,
                "failed to estimate rebalance WETH wrap gas, using configured fallback gas limit"
            );
        }
    }
    wait_for_native_balance_for_rebalance(
        transfer,
        &from,
        wrap_value.raw,
        max_fee,
        tx.gas_limit.unwrap_or(REBALANCE_WETH_WRAP_GAS_LIMIT),
        REBALANCE_WETH_WRAP_CONTEXT,
    )
    .await?;
    let nonce = transfer
        .nonce_manager
        .reserve_next(&transfer.chain, transfer.chain_id, &from)
        .await
        .map_err(|e| e.to_string())?;
    tx.nonce = Some(nonce);
    let signed = match transfer.wallet.sign_transaction_bytes(&tx).await {
        Ok(signed) => signed,
        Err(e) => {
            transfer
                .nonce_manager
                .mark_failed(transfer.chain_id, &from, nonce)
                .await;
            return Err(format!("rebalance WETH wrap sign failed: {e}"));
        }
    };
    let tx_hash = match transfer.chain.send_transaction(&signed).await {
        Ok(hash) => hash,
        Err(e) => {
            transfer
                .nonce_manager
                .mark_failed(transfer.chain_id, &from, nonce)
                .await;
            return Err(format!("rebalance WETH wrap send failed: {e}"));
        }
    };
    let receipt = transfer
        .chain
        .wait_for_receipt(&tx_hash, transfer.transfer_confirm_timeout.as_secs(), 2.0)
        .await
        .map_err(|e| format!("rebalance WETH wrap tx {tx_hash} confirmation failed: {e}"))?;
    if !receipt.status {
        return Err(format!(
            "rebalance WETH wrap tx {tx_hash} reverted on-chain"
        ));
    }
    info!(
        tx = %tx_hash,
        amount = %amount,
        "auto-rebalance wrapped native ETH to WETH"
    );
    Ok(tx_hash)
}

async fn execute_wallet_to_cex_transfer(
    withdraw: &peanut_internship_rust::inventory::types::WithdrawStep,
    transfer: &RebalanceTransferContext,
) -> Result<String, String> {
    let from = Address::new(transfer.wallet.address()).map_err(|e| e.to_string())?;
    if withdraw.asset.eq_ignore_ascii_case(WETH_SYMBOL) {
        return Err("Wallet→CEX WETH rebalance requires unwrap-to-ETH before Binance deposit; refusing to send ERC-20 WETH to Binance".into());
    }
    let gas = transfer
        .chain
        .get_gas_price()
        .await
        .map_err(|e| e.to_string())?;
    let max_priority_fee = gas.priority_fee_medium;
    let max_fee = gas.get_max_fee(GasPriority::Medium, DEFAULT_GAS_BUFFER_BPS);
    if let Some(max_gas_gwei) = transfer.max_gas_gwei {
        let cap = U256::from(max_gas_gwei) * U256::from(WEI_PER_GWEI);
        if max_fee > cap {
            return Err(format!(
                "rebalance tx max_fee_per_gas {} exceeds cap {}",
                max_fee, cap
            ));
        }
    }

    let mut tx = if withdraw.asset.eq_ignore_ascii_case(ETH_SYMBOL) {
        TransactionRequest {
            to: transfer.cex_deposit_address.clone(),
            value: TokenAmount::from_human(
                withdraw.amount,
                ETH_DECIMALS,
                Some(ETH_SYMBOL.to_string()),
            )
            .map_err(|e| e.to_string())?,
            data: Bytes::new(),
            nonce: None,
            gas_limit: Some(REBALANCE_NATIVE_TRANSFER_GAS_LIMIT),
            max_fee_per_gas: Some(max_fee),
            max_priority_fee: Some(max_priority_fee),
            chain_id: transfer.chain_id,
        }
    } else {
        let token = transfer
            .token_book
            .get(&withdraw.asset)
            .ok_or_else(|| format!("asset {} missing from rebalance token book", withdraw.asset))?;
        let amount = TokenAmount::from_human(
            withdraw.amount,
            token.decimals,
            Some(withdraw.asset.clone()),
        )
        .map_err(|e| e.to_string())?;
        TransactionRequest {
            to: token.address.clone(),
            value: TokenAmount::eth(0u64),
            data: Bytes::from(erc20_transfer_calldata(
                &transfer.cex_deposit_address,
                amount.raw,
            )),
            nonce: None,
            gas_limit: Some(REBALANCE_TOKEN_TRANSFER_GAS_LIMIT),
            max_fee_per_gas: Some(max_fee),
            max_priority_fee: Some(max_priority_fee),
            chain_id: transfer.chain_id,
        }
    };

    if let Ok(estimated) = transfer.chain.estimate_gas(&tx).await {
        let buffered: U256 = U256::from(estimated)
            .saturating_mul(U256::from(DEFAULT_GAS_BUFFER_BPS))
            / U256::from(BPS_SCALE);
        tx.gas_limit = Some(buffered.as_u64().max(MIN_GAS_LIMIT));
    }
    let nonce = transfer
        .nonce_manager
        .reserve_next(&transfer.chain, transfer.chain_id, &from)
        .await
        .map_err(|e| e.to_string())?;
    tx.nonce = Some(nonce);
    let signed = match transfer.wallet.sign_transaction_bytes(&tx).await {
        Ok(signed) => signed,
        Err(e) => {
            transfer
                .nonce_manager
                .mark_failed(transfer.chain_id, &from, nonce)
                .await;
            return Err(e.to_string());
        }
    };
    let tx_hash = match transfer.chain.send_transaction(&signed).await {
        Ok(hash) => hash,
        Err(e) => {
            transfer
                .nonce_manager
                .mark_failed(transfer.chain_id, &from, nonce)
                .await;
            return Err(e.to_string());
        }
    };
    let receipt = transfer
        .chain
        .wait_for_receipt(&tx_hash, transfer.transfer_confirm_timeout.as_secs(), 2.0)
        .await
        .map_err(|e| format!("wallet tx {tx_hash} confirmation failed: {e}"))?;
    if !receipt.status {
        return Err(format!("wallet tx {tx_hash} reverted on-chain"));
    }
    let deposit = wait_for_binance_deposit(transfer, &withdraw.asset, &tx_hash).await?;
    Ok(format!(
        "wallet_tx:{tx_hash} confirmed_block={} gas_used={} binance_deposit_status={}",
        receipt.block_number, receipt.gas_used, deposit.status
    ))
}

async fn wait_for_binance_withdrawal(
    transfer: &RebalanceTransferContext,
    asset: &str,
    withdraw_id: &str,
) -> Result<peanut_internship_rust::exchange::WithdrawalRecord, String> {
    let deadline = tokio::time::Instant::now() + transfer.transfer_confirm_timeout;
    loop {
        let records = transfer
            .exchange
            .fetch_withdrawal_history(asset)
            .await
            .map_err(|e| format!("failed to fetch Binance withdrawal history: {e}"))?;
        if let Some(record) = records.into_iter().find(|record| record.id == withdraw_id) {
            if record.completed() {
                return Ok(record);
            }
            if record.failed() {
                return Err(format!(
                    "Binance withdrawal {withdraw_id} failed with status {}",
                    record.status
                ));
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "timeout waiting for Binance withdrawal {withdraw_id} completion"
            ));
        }
        tokio::time::sleep(transfer.transfer_confirm_poll).await;
    }
}

async fn wait_for_binance_deposit(
    transfer: &RebalanceTransferContext,
    asset: &str,
    tx_hash: &str,
) -> Result<peanut_internship_rust::exchange::DepositRecord, String> {
    let deadline = tokio::time::Instant::now() + transfer.transfer_confirm_timeout;
    loop {
        let records = transfer
            .exchange
            .fetch_deposit_history(asset, Some(tx_hash))
            .await
            .map_err(|e| format!("failed to fetch Binance deposit history: {e}"))?;
        if let Some(record) = records.into_iter().find(|record| {
            record
                .tx_id
                .as_deref()
                .is_some_and(|tx_id| tx_id.eq_ignore_ascii_case(tx_hash))
        }) {
            if record.credited() {
                return Ok(record);
            }
            if record.failed() {
                return Err(format!(
                    "Binance deposit for tx {tx_hash} failed with status {}",
                    record.status
                ));
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "timeout waiting for Binance deposit credit for tx {tx_hash}"
            ));
        }
        tokio::time::sleep(transfer.transfer_confirm_poll).await;
    }
}

fn erc20_transfer_calldata(to: &Address, amount: U256) -> Vec<u8> {
    let mut data = hex::decode("a9059cbb").expect("valid ERC20 transfer selector");
    data.resize(4 + 12, 0);
    data.extend_from_slice(&to.as_eth_address().0);
    let mut amount_bytes = [0u8; 32];
    amount.to_big_endian(&mut amount_bytes);
    data.extend_from_slice(&amount_bytes);
    data
}

fn spawn_book_ticker_cache(
    ws_url: String,
    pair: String,
    snapshots: Arc<RwLock<HashMap<String, OrderBookSnapshot>>>,
) {
    tokio::spawn(async move {
        loop {
            match subscribe_book_ticker_stream(&ws_url, &pair).await {
                Ok(mut rx) => {
                    info!(pair, "CEX bookTicker stream connected");
                    while let Some(event) = rx.recv().await {
                        let bid_price = Decimal::from_str_exact(&event.bid_price);
                        let bid_qty = Decimal::from_str_exact(&event.bid_qty);
                        let ask_price = Decimal::from_str_exact(&event.ask_price);
                        let ask_qty = Decimal::from_str_exact(&event.ask_qty);
                        let (Ok(bid_price), Ok(bid_qty), Ok(ask_price), Ok(ask_qty)) =
                            (bid_price, bid_qty, ask_price, ask_qty)
                        else {
                            warn!(pair, "failed to parse CEX bookTicker prices");
                            continue;
                        };

                        let mid = (bid_price + ask_price) / Decimal::TWO;
                        let spread_bps = if mid.is_zero() {
                            None
                        } else {
                            Some((ask_price - bid_price) / mid * Decimal::from(BPS_SCALE))
                        };
                        let snapshot = OrderBookSnapshot {
                            symbol: pair.clone(),
                            timestamp: unix_millis(),
                            bids: vec![(bid_price, bid_qty)],
                            asks: vec![(ask_price, ask_qty)],
                            best_bid: Some((bid_price, bid_qty)),
                            best_ask: Some((ask_price, ask_qty)),
                            mid_price: Some(mid),
                            spread_bps,
                        };
                        debug!(pair, "CEX bookTicker update received");
                        snapshots.write().await.insert(pair.clone(), snapshot);
                    }
                    warn!(pair, "CEX bookTicker stream ended; reconnecting");
                }
                Err(error) => {
                    warn!(pair, error = %error, "CEX bookTicker stream connect failed");
                }
            }
            tokio::time::sleep(Duration::from_secs(DEFAULT_RECONNECT_DELAY_SECS)).await;
        }
    });
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();
    let cli = Cli::parse();
    let log_file_path = init_tracing(&cli.log_dir)?;
    info!(log_file = %log_file_path.display(), "file logging enabled");
    if cli.check_config {
        run_config_check(&cli)?;
        info!("config check passed");
        return Ok(());
    }

    let trade_size =
        Decimal::from_str_exact(&cli.size).map_err(|e| format!("invalid --size: {e}"))?;
    let min_score = Decimal::from(cli.min_score);

    // Kill-switch: shared between main loop, metrics HTTP, and PnL breaker.
    let watchdog_path = if cli.halt_file_path.is_empty() {
        None
    } else {
        Some(PathBuf::from(&cli.halt_file_path))
    };
    let halt_coordinator = Arc::new(HaltCoordinator::new(watchdog_path));
    if !cli.halt_file_path.is_empty() {
        let halt = Arc::clone(&halt_coordinator);
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(Duration::from_millis(DEFAULT_WATCHDOG_INTERVAL_MS));
            loop {
                interval.tick().await;
                halt.check_watchdog();
                if halt.is_halted() {
                    break;
                }
            }
        });
    }

    // PnL-based breaker: trips when cumulative daily realised PnL drops
    // below -max_daily_loss_usd. Shared via Arc<Mutex<>> between the
    // completion consumer (records PnL) and the tick loop (checks halt).
    let max_daily_loss = Decimal::from_str_exact(&cli.max_daily_loss_usd)
        .map_err(|e| format!("invalid --max-daily-loss-usd: {e}"))?;
    let pnl_breaker = Arc::new(Mutex::new(
        peanut_internship_rust::executor::PnlBreaker::new(
            peanut_internship_rust::executor::PnlBreakerConfig {
                max_daily_loss_usd: max_daily_loss,
            },
        ),
    ));
    if max_daily_loss > Decimal::ZERO {
        info!(max_daily_loss_usd = %max_daily_loss, "PnL breaker enabled");
    } else {
        info!("PnL breaker disabled (--max-daily-loss-usd 0)");
    }

    let initial_capital = Decimal::from_str_exact(&cli.initial_capital_usd)
        .map_err(|e| format!("invalid --initial-capital-usd: {e}"))?;
    let risk_limits = RiskLimits {
        max_trade_usd: Decimal::from_str_exact(&cli.risk_max_trade_usd)
            .map_err(|e| format!("invalid --risk-max-trade-usd: {e}"))?,
        max_daily_loss: Decimal::from_str_exact(&cli.risk_max_daily_loss_usd)
            .map_err(|e| format!("invalid --risk-max-daily-loss-usd: {e}"))?,
        max_trades_per_hour: cli.risk_max_trades_per_hour,
        consecutive_loss_limit: cli.risk_consecutive_loss_limit,
        ..RiskLimits::default()
    };
    let risk_manager = Arc::new(Mutex::new(RiskManager::new(
        risk_limits.clone(),
        initial_capital,
    )));
    let pre_trade_validator = Arc::new(PreTradeValidator::default());
    info!(
        dry_run = cli.dry_run,
        initial_capital_usd = %initial_capital,
        max_trade_usd = %risk_limits.max_trade_usd,
        max_daily_loss_usd = %risk_limits.max_daily_loss,
        max_trades_per_hour = risk_limits.max_trades_per_hour,
        consecutive_loss_limit = risk_limits.consecutive_loss_limit,
        kill_switch_file = DEFAULT_KILL_SWITCH_FILE,
        "safety controls enabled"
    );

    // Prometheus metrics: init global registry + spawn /metrics server.
    // When `--metrics-port 0`, we still init the registry (so instrumented
    // code records observations) but skip the HTTP endpoint.
    let _metrics = peanut_internship_rust::observability::init_metrics(
        peanut_internship_rust::observability::Metrics::new(),
    );
    if cli.metrics_port != 0 {
        let addr = std::net::SocketAddr::from(([0, 0, 0, 0], cli.metrics_port));
        let m = _metrics.clone();
        let h = Arc::clone(&halt_coordinator);
        tokio::spawn(async move {
            if let Err(e) =
                peanut_internship_rust::observability::serve_metrics_with_halt(addr, m, Some(h))
                    .await
            {
                error!(error = %e, "metrics server terminated");
            }
        });
        info!(port = cli.metrics_port, "metrics endpoint /metrics enabled");
    }
    if !cli.event_log_path.is_empty() {
        peanut_internship_rust::observability::init_event_logger(&cli.event_log_path)?;
        info!(path = %cli.event_log_path, "observability event log enabled");
    }

    let production = cli.production || peanut_internship_rust::config::env_production_enabled();
    if production {
        warn!("PRODUCTION MODE - REAL MONEY");
    } else {
        info!("Testnet mode - fake money");
    }
    validate_live_execution_config(&cli)?;

    let exchange_cfg = BinanceConfig::from_env_for(production)?;
    info!(
        sandbox = exchange_cfg.sandbox,
        base_url = %exchange_cfg.base_url,
        ws_url = %exchange_cfg.ws_url,
        "Binance configuration loaded"
    );
    let cex_ws_url = exchange_cfg.ws_url.clone();
    let exchange = Arc::new(ExchangeClient::new(exchange_cfg)?);
    let tracked_pairs = tracked_pairs_for_price_source(&cli.pair);

    let cex_order_books: Arc<dyn CexOrderBookSource> = Arc::new(
        WsCexOrderBookSource::new(Arc::clone(&exchange), cex_ws_url, &tracked_pairs).await,
    );

    // Inventory tracker (empty; will be populated by sync_balances).
    let inventory = Arc::new(RwLock::new(InventoryTracker::new(
        vec![Venue::Binance, Venue::Wallet],
        cli.dex_chain_id,
    )));

    // Optional synthetic seed so simulation demos work without live balance
    // endpoints. Applied BEFORE the first tick so the inventory pre-check
    // in `SignalGenerator` has something to approve.
    if !cli.seed_inventory.is_empty() {
        if !cli.simulation && !cli.dry_run {
            warn!(
                "--seed-inventory used outside --simulation/--dry-run; synthetic balances will be overwritten by the next sync"
            );
        }
        let mut guard = inventory.write().await;
        for spec in &cli.seed_inventory {
            match parse_seed_spec(spec) {
                Ok((venue, balances)) => {
                    let count = balances.len();
                    info!(venue = %venue, assets = count, "seeding inventory");
                    seed_inventory_balances(&mut guard, venue, balances);
                }
                Err(e) => {
                    return Err(format!("invalid --seed-inventory {spec:?}: {e}").into());
                }
            }
        }
        drop(guard);
    }

    // Signal generator + scorer.
    //
    // When `--dex-address-book` is absent, we use deterministic synthetic DEX
    // prices. When it is present, live DEX pricing is mandatory: missing pools
    // or RPC configuration abort startup instead of silently falling back.
    let price_source: Arc<AnyPriceSource> = {
        if cli.dex_address_book.is_empty() {
            info!("price source: stub (synthetic DEX prices)");
            Arc::new(AnyPriceSource::Stub(StubPriceSource::new_with_order_books(
                Arc::clone(&cex_order_books),
            )))
        } else {
            let live_pools = load_live_pool_book(&cli.dex_address_book, Some(&tracked_pairs))?;
            if live_pools.is_empty() {
                return Err(format!(
                    "--dex-address-book {} does not contain any entries with a pool address",
                    cli.dex_address_book
                )
                .into());
            }
            let rpc_urls = resolve_rpc_urls(&cli).ok_or(
                "--dex-address-book was provided, so --eth-rpc-url or ETH_RPC_URL is required",
            )?;
            let client = ChainClient::new(rpc_urls.clone(), RPC_TIMEOUT_SECS, RPC_RETRIES)?;
            log_rpc_health("live price source", &client).await;
            let live = LivePriceSource::new_with_order_books(
                Arc::clone(&cex_order_books),
                client,
                live_pools,
            )
            .await?;
            // Start the background WS block feed so DEX prices update
            // on every new block (same real-time pattern as CEX bookTicker).
            let size = Decimal::from_str_exact(&cli.size).unwrap_or_else(|e| {
                warn!(
                    configured_size = %cli.size,
                    error = %e,
                    "failed to parse --size for DEX block feed; using 1"
                );
                Decimal::ONE
            });
            if let Some(ws_url) = resolve_ws_url(&rpc_urls) {
                match live.start_block_feed(&ws_url, size).await {
                    Ok(()) => info!(ws_url, "DEX block feed started"),
                    Err(e) => {
                        warn!(error = %e, "DEX block feed failed, falling back to per-tick RPC")
                    }
                }
            } else {
                warn!("No WS URL for DEX block feed, falling back to per-tick RPC");
            }
            info!("price source: live DEX pools");
            Arc::new(AnyPriceSource::Live(live))
        }
    };
    let generator_config = GeneratorConfig {
        min_profit_usd: Decimal::from_str_exact(&cli.min_profit_usd)
            .map_err(|e| format!("invalid --min-profit-usd: {e}"))?,
        min_spread_bps: Decimal::from(cli.min_spread_bps),
        max_position_usd: Decimal::from_str_exact(&cli.risk_max_trade_usd)
            .map_err(|e| format!("invalid --risk-max-trade-usd: {e}"))?,
        ..GeneratorConfig::default()
    };
    // Shared fee model. A single source of truth keeps pre-trade gating
    // (`SignalGenerator`) and post-trade realised PnL (`Executor::calc_pnl`)
    // in lockstep — skewing one without the other is a classic source of
    // "we signalled profit but booked a loss" bugs.
    let effective_dex_swap_bps =
        effective_dex_fee_bps(cli.fee_dex_swap_bps, &cli.dex_address_book)?;
    let fees = FeeStructure {
        cex_taker_bps: Decimal::from(cli.fee_cex_taker_bps),
        dex_swap_bps: effective_dex_swap_bps,
        gas_cost_usd: Decimal::from_str_exact(&cli.fee_gas_usd)
            .map_err(|e| format!("invalid --fee-gas-usd: {e}"))?,
    };
    let gas_estimator = match cli.fee_gas_mode.to_ascii_lowercase().as_str() {
        "fixed" => GasFeeEstimator::Fixed,
        "rpc" => {
            let rpc_urls = resolve_rpc_urls(&cli)
                .ok_or("--fee-gas-mode rpc requires --eth-rpc-url or ETH_RPC_URL")?;
            GasFeeEstimator::Rpc {
                client: ChainClient::new(rpc_urls, RPC_TIMEOUT_SECS, RPC_RETRIES)?,
                gas_units: cli.fee_gas_units,
                buffer_bps: cli.fee_gas_buffer_bps,
            }
        }
        "estimate" => {
            let (rpc_urls, wallet_address) = resolve_wallet_config(&cli).ok_or(
                "--fee-gas-mode estimate requires --eth-rpc-url/ETH_RPC_URL and --wallet-address/WALLET_ADDRESS",
            )?;
            if cli.dex_address_book.is_empty() {
                return Err("--fee-gas-mode estimate requires --dex-address-book".into());
            }
            GasFeeEstimator::Estimate {
                client: ChainClient::new(rpc_urls, RPC_TIMEOUT_SECS, RPC_RETRIES)?,
                fallback_gas_units: cli.fee_gas_units,
                buffer_bps: cli.fee_gas_buffer_bps,
                wallet: Address::new(&wallet_address)?,
                address_book: Arc::new(load_address_book(&cli.dex_address_book)?),
                slippage_bps: cli.dex_slippage_bps,
                chain_id: cli.dex_chain_id,
                v2_router: if cli.dex_router.is_empty() {
                    DexSwapperConfig::default().router
                } else {
                    Address::new(&cli.dex_router)?
                },
                v3_router: if cli.dex_router.is_empty() {
                    Address::new(ARBITRUM_UNISWAP_V3_SWAP_ROUTER)?
                } else {
                    Address::new(&cli.dex_router)?
                },
            }
        }
        "anvil" => {
            let fork_url = resolve_anvil_fork_url(&cli)
                .ok_or("--fee-gas-mode anvil requires --anvil-fork-url or ANVIL_FORK_URL")?;
            let wallet_address = resolve_wallet_address(&cli)
                .ok_or("--fee-gas-mode anvil requires --wallet-address or WALLET_ADDRESS")?;
            if cli.dex_address_book.is_empty() {
                return Err("--fee-gas-mode anvil requires --dex-address-book".into());
            }
            GasFeeEstimator::Anvil {
                client: ChainClient::new(vec![fork_url.clone()], RPC_TIMEOUT_SECS, RPC_RETRIES)?,
                simulator: ForkSimulator::new(&fork_url)?,
                fallback_gas_units: cli.fee_gas_units,
                buffer_bps: cli.fee_gas_buffer_bps,
                wallet: Address::new(&wallet_address)?,
                address_book: Arc::new(load_address_book(&cli.dex_address_book)?),
                slippage_bps: cli.dex_slippage_bps,
                chain_id: cli.dex_chain_id,
                v2_router: if cli.dex_router.is_empty() {
                    DexSwapperConfig::default().router
                } else {
                    Address::new(&cli.dex_router)?
                },
                v3_router: if cli.dex_router.is_empty() {
                    Address::new(ARBITRUM_UNISWAP_V3_SWAP_ROUTER)?
                } else {
                    Address::new(&cli.dex_router)?
                },
            }
        }
        other => {
            return Err(format!(
                "invalid --fee-gas-mode '{other}', expected fixed, rpc, estimate, or anvil"
            )
            .into());
        }
    };
    info!(
        min_profit_usd = %generator_config.min_profit_usd,
        min_spread_bps = %generator_config.min_spread_bps,
        cex_taker_bps = %fees.cex_taker_bps,
        dex_swap_bps = %fees.dex_swap_bps,
        configured_dex_swap_bps = cli.fee_dex_swap_bps,
        gas_cost_usd = %fees.gas_cost_usd,
        gas_mode = %cli.fee_gas_mode,
        gas_units = cli.fee_gas_units,
        gas_buffer_bps = cli.fee_gas_buffer_bps,
        "generator thresholds + fee model"
    );
    let configured_min_profit_usd = generator_config.min_profit_usd;
    let mut generator = SignalGenerator::new(
        Arc::clone(&price_source),
        Arc::clone(&inventory),
        fees.clone(),
        generator_config,
    );
    let scorer_config = ScorerConfig {
        min_spread_bps: Decimal::from(cli.min_spread_bps),
        ..ScorerConfig::default()
    };
    let scorer = SignalScorer::new(scorer_config);

    // Replay protection: persistent journal when a path is supplied, else
    // the default in-memory guard. Any SQLite error aborts startup — silent
    // fallback to in-memory would weaken the deduplication guarantee ops
    // thought they were buying.
    let replay = if cli.replay_db.is_empty() {
        peanut_internship_rust::executor::recovery::ReplayProtection::default()
    } else {
        let ttl = Duration::from_secs(cli.replay_ttl_secs);
        info!(path = %cli.replay_db, ttl_s = cli.replay_ttl_secs, "replay: SQLite journal");
        peanut_internship_rust::executor::recovery::ReplayProtection::with_journal(
            &cli.replay_db,
            ttl,
        )?
    };

    // Executor: simulated or live legs. Wrap in `Arc` so the queue worker can
    // share it with the producer (tick) for breaker introspection.
    let nonce_manager = NonceManager::new();
    let mut effective_use_flashbots = cli.use_flashbots;
    let legs: Arc<dyn peanut_internship_rust::executor::engine::LegExecutor> = if cli.simulation
        || cli.dry_run
    {
        Arc::new(
            SimulatedLegs::default().with_price_source(Arc::clone(&price_source)
                as Arc<dyn peanut_internship_rust::strategy::generator::PriceSource + Send + Sync>),
        )
    } else {
        if !cli.dex_address_book.is_empty() {
            let rpc_urls = resolve_wallet_config(&cli)
                .map(|(rpc, _)| rpc)
                .ok_or("live DEX execution requires --eth-rpc-url/ETH_RPC_URL and --wallet-address/WALLET_ADDRESS")?;
            let chain_client = ChainClient::new(rpc_urls, RPC_TIMEOUT_SECS, RPC_RETRIES)?;
            log_rpc_health("live dex", &chain_client).await;
            let wallet = WalletManager::from_env(&cli.wallet_key_env)?;
            let recipient = Address::new(wallet.address())?;
            let address_book = Arc::new(
                load_address_book_with_fee_discovery(
                    &cli.dex_address_book,
                    &chain_client,
                    cli.dex_chain_id,
                )
                .await?,
            );
            let (has_v2, has_v3) = live_execution_pool_kinds(&cli.dex_address_book, &cli.pair)?;
            if has_v2 && has_v3 && !cli.dex_router.is_empty() {
                return Err("mixed V2/V3 live execution requires --dex-router to be empty so V2 and V3 can use their own routers".into());
            }
            let v2_config = DexSwapperConfig {
                router: if cli.dex_router.is_empty() {
                    DexSwapperConfig::default().router
                } else {
                    Address::new(&cli.dex_router)?
                },
                slippage_bps: cli.dex_slippage_bps,
                deadline_secs: cli.dex_deadline_secs,
                chain_id: cli.dex_chain_id,
                max_gas_gwei: (cli.max_gas_gwei > 0).then_some(cli.max_gas_gwei),
                ..DexSwapperConfig::default()
            };
            let v3_config = DexSwapperConfig {
                router: if cli.dex_router.is_empty() {
                    Address::new(ARBITRUM_UNISWAP_V3_SWAP_ROUTER)?
                } else {
                    Address::new(&cli.dex_router)?
                },
                slippage_bps: cli.dex_slippage_bps,
                deadline_secs: cli.dex_deadline_secs,
                chain_id: cli.dex_chain_id,
                max_gas_gwei: (cli.max_gas_gwei > 0).then_some(cli.max_gas_gwei),
                ..DexSwapperConfig::default()
            };
            let flashbots_auth = if cli.use_flashbots {
                match WalletManager::from_env(&cli.flashbots_auth_key_env) {
                    Ok(auth_wallet) => Some(auth_wallet),
                    Err(e) if cli.require_private_dex => {
                        return Err(format!(
                            "private DEX mode requires --flashbots-auth-key-env {}: {e}",
                            cli.flashbots_auth_key_env
                        )
                        .into());
                    }
                    Err(e) => {
                        effective_use_flashbots = false;
                        warn!(
                            error = %e,
                            "Flashbots auth wallet unavailable; falling back to CEX-first public DEX mode"
                        );
                        None
                    }
                }
            } else {
                None
            };
            let flashbots_config = FlashbotsConfig {
                relay_url: cli.flashbots_relay_url.clone(),
                target_block_offset: cli.flashbots_target_block_offset,
                max_blocks_to_try: cli.flashbots_max_blocks_to_try,
                ..FlashbotsConfig::default()
            };
            let build_v2_swapper = || -> Arc<dyn DexSwapper> {
                let base_swapper =
                    UniswapV2Swapper::new(chain_client.clone(), wallet.clone(), v2_config.clone())
                        .with_nonce_manager(nonce_manager.clone());
                if let Some(auth_wallet) = flashbots_auth.clone() {
                    let relay = Arc::new(FlashbotsRelayClient::new(
                        flashbots_config.clone(),
                        auth_wallet,
                    ));
                    Arc::new(FlashbotsSwapper::new(
                        base_swapper,
                        relay,
                        flashbots_config.clone(),
                    ))
                } else {
                    Arc::new(base_swapper)
                }
            };
            let build_v3_swapper = || -> Arc<dyn DexSwapper> {
                let base_swapper =
                    UniswapV3Swapper::new(chain_client.clone(), wallet.clone(), v3_config.clone())
                        .with_nonce_manager(nonce_manager.clone());
                if let Some(auth_wallet) = flashbots_auth.clone() {
                    let relay = Arc::new(FlashbotsRelayClient::new(
                        flashbots_config.clone(),
                        auth_wallet,
                    ));
                    Arc::new(FlashbotsSwapper::new_v3(
                        base_swapper,
                        relay,
                        flashbots_config.clone(),
                    ))
                } else {
                    Arc::new(base_swapper)
                }
            };
            let swapper: Arc<dyn DexSwapper> = if has_v2 && has_v3 {
                Arc::new(CompositeDexSwapper::new(
                    build_v2_swapper(),
                    build_v3_swapper(),
                ))
            } else if has_v3 {
                build_v3_swapper()
            } else {
                build_v2_swapper()
            };
            info!(
                address_book = %cli.dex_address_book,
                has_v2,
                has_v3,
                v2_router = %v2_config.router,
                v3_router = %v3_config.router,
                chain_id = cli.dex_chain_id,
                slippage_bps = cli.dex_slippage_bps,
                max_gas_gwei = ?v2_config.max_gas_gwei,
                private_dex = effective_use_flashbots,
                "live mode: DEX leg configured"
            );
            Arc::new(LiveLegs::new(Arc::clone(&exchange)).with_dex(
                swapper,
                address_book,
                v2_config,
                recipient,
            ))
        } else {
            if cli.use_flashbots && cli.require_private_dex {
                return Err("private DEX mode requires --dex-address-book in live mode".into());
            }
            effective_use_flashbots = false;
            warn!(
                "live mode: DEX leg will return NotImplemented — pass --dex-address-book to enable"
            );
            Arc::new(LiveLegs::new(Arc::clone(&exchange)))
        }
    };
    // Reconcile store: persistent database for LEG2_TIMEOUT entries.
    let reconcile_store = if !cli.reconcile_db.is_empty() {
        let store = Arc::new(ReconcileStore::open(&cli.reconcile_db)?);
        info!(path = %cli.reconcile_db, "reconcile store enabled");
        Some(store)
    } else {
        None
    };

    // Clone the legs handle before moving it into the Executor — the
    // reconcile worker needs its own Arc for unwind calls.
    let legs_for_reconcile: Arc<dyn LegExecutor> = Arc::clone(&legs);

    let executor_config = ExecutorConfig {
        use_flashbots: effective_use_flashbots,
        ..ExecutorConfig::default()
    };
    let mut executor_builder =
        Executor::with_replay(legs, executor_config, replay).with_fees(fees.clone());
    if let Some(ref store) = reconcile_store {
        executor_builder = executor_builder.with_reconcile_store(Arc::clone(store));
    }
    let executor = Arc::new(executor_builder);

    // Scorer + PnL are touched from both the tick (read score) and the
    // completion consumer (record history + record pnl). Wrap in async
    // mutexes so cross-task updates are serialised cleanly.
    let scorer = Arc::new(Mutex::new(scorer));
    let pnl = Arc::new(Mutex::new(PnLEngine::new()));

    // --- Signal queue + worker -------------------------------------------
    // The worker pops from the queue, runs `executor.execute`, and posts the
    // terminal [`ExecutionContext`] on the completion channel. The consumer
    // task updates `scorer`/`pnl` so the producer can continue scoring
    // without blocking on execution.
    let queue = SignalQueue::new(QueueConfig {
        max_size: cli.queue_max_size,
        ..QueueConfig::default()
    });
    let (completion_tx, mut completion_rx) = mpsc::unbounded_channel();
    let (worker_shutdown_tx, worker_shutdown_rx) = watch::channel(false);
    let worker = QueueWorker::new(
        Arc::clone(&queue),
        Arc::clone(&executor),
        cli.max_concurrent_executions,
        Duration::from_millis(50),
    )
    .with_inventory(Arc::clone(&inventory))
    .with_sink(completion_tx);

    let worker_handle = tokio::spawn(async move {
        worker.run_until_shutdown(worker_shutdown_rx).await;
    });

    // Alert sink: WebhookSink when a URL is configured, else NoopSink. The
    // sink is cheap to clone (wraps an `Arc<reqwest::Client>`), so we share
    // it across the completion consumer and the breaker watcher.
    let alert_sink: Arc<dyn AlertSink> = if cli.alert_webhook_url.is_empty() {
        Arc::new(NoopSink)
    } else {
        let provider = AlertProvider::parse(&cli.alert_provider);
        // SECURITY: never log the raw webhook URL — Telegram embeds
        // the bot token in the URL path. Always use `mask_webhook_url`.
        info!(
            url = %mask_webhook_url(&cli.alert_webhook_url),
            provider = ?provider,
            "alerts enabled"
        );
        Arc::new(WebhookSink::new(
            cli.alert_webhook_url.clone(),
            provider,
            cli.alert_telegram_chat_id.clone(),
            Duration::from_secs(5),
        ))
    };
    let alert_rules = AlertRules {
        large_loss_threshold: Decimal::from(cli.alert_large_loss),
    };
    let trade_logger = if cli.trade_log_path.is_empty() {
        None
    } else {
        let logger = Arc::new(TradeJsonlLogger::open(&cli.trade_log_path)?);
        info!(path = %cli.trade_log_path, "structured trade log enabled");
        Some(logger)
    };

    // Completion consumer: update scorer history + ledger, feed PnL breaker,
    // and fire alerts derived from the terminal state via `evaluate_execution`.
    {
        let scorer = Arc::clone(&scorer);
        let pnl = Arc::clone(&pnl);
        let alert_sink = Arc::clone(&alert_sink);
        let alert_rules = alert_rules.clone();
        let pnl_breaker = Arc::clone(&pnl_breaker);
        let halt_coordinator = Arc::clone(&halt_coordinator);
        let trade_logger = trade_logger.clone();
        let risk_manager = Arc::clone(&risk_manager);
        let inventory = Arc::clone(&inventory);
        tokio::spawn(async move {
            while let Some(ctx) = completion_rx.recv().await {
                let signal = &ctx.signal;
                let buy_venue = signal.direction.buy_venue();
                let sell_venue = signal.direction.sell_venue();
                let (base, quote) = match split_pair(&signal.pair) {
                    Ok(parts) => parts,
                    Err(error) => {
                        warn!(
                            pair = %signal.pair,
                            error = %error,
                            "malformed pair in completion handler; skipping inventory release"
                        );
                        continue;
                    }
                };

                let buy_asset = quote;
                let buy_amount = signal.size * signal.cex_price;
                let sell_asset = base;
                let sell_amount = signal.size;

                {
                    let mut inv = inventory.write().await;
                    let _ = inv.release(buy_venue, buy_asset, buy_amount);
                    let _ = inv.release(sell_venue, sell_asset, sell_amount);
                }

                let pair_str = ctx.signal.pair.clone();
                let duration_ms = ctx
                    .finished_at
                    .map(|finished| finished.duration_since(ctx.started_at).as_millis());
                peanut_internship_rust::observability::emit_event(
                    "execution_terminal",
                    json!({
                        "signal_id": ctx.signal.signal_id,
                        "pair": ctx.signal.pair,
                        "direction": ctx.signal.direction.to_string(),
                        "state": ctx.state.to_string(),
                        "error": ctx.error,
                        "actual_net_pnl": ctx.actual_net_pnl.map(|v| v.to_string()),
                        "duration_ms": duration_ms,
                        "leg1_venue": ctx.leg1_venue,
                        "leg1_handle": ctx.leg1_handle,
                        "leg1_fill_price": ctx.leg1_fill_price.map(|v| v.to_string()),
                        "leg1_fill_size": ctx.leg1_fill_size.map(|v| v.to_string()),
                        "leg2_venue": ctx.leg2_venue,
                        "leg2_handle": ctx.leg2_handle,
                        "leg2_fill_price": ctx.leg2_fill_price.map(|v| v.to_string()),
                        "leg2_fill_size": ctx.leg2_fill_size.map(|v| v.to_string()),
                    }),
                );
                let done = ctx.state.is_filled();
                scorer.lock().await.record_result(&pair_str, done);
                if done {
                    if let Some(net) = ctx.actual_net_pnl {
                        info!(
                            "✅ TRADE DONE | pair={} | state={} | expected_net={} | actual_net={} | actual_gross={} | fees={} | cex_fee={} | gas_fee={} | gas_used={} | gas_wei={}",
                            pair_str,
                            ctx.state,
                            fmt_usd(ctx.signal.expected_net_pnl),
                            fmt_usd(net),
                            fmt_usd(ctx.actual_gross_pnl_usd.unwrap_or(Decimal::ZERO)),
                            fmt_usd(ctx.actual_fees_usd.unwrap_or(Decimal::ZERO)),
                            fmt_usd(ctx.actual_cex_fee_usd.unwrap_or(Decimal::ZERO)),
                            fmt_usd(ctx.actual_onchain_gas_fee_usd.unwrap_or(Decimal::ZERO)),
                            ctx.onchain_gas_used
                                .map(|v| v.to_string())
                                .unwrap_or_else(|| "n/a".to_string()),
                            ctx.onchain_gas_fee_wei
                                .map(|v| v.to_string())
                                .unwrap_or_else(|| "n/a".to_string())
                        );
                        emit_best_effort(
                            &alert_sink,
                            &AlertEvent::TradeExecuted {
                                signal_id: ctx.signal.signal_id.clone(),
                                pair: pair_str.clone(),
                                direction: ctx.signal.direction.to_string(),
                                expected_net_pnl: fmt_usd(ctx.signal.expected_net_pnl),
                                actual_net_pnl: fmt_usd(net),
                                actual_gross_pnl: fmt_usd(
                                    ctx.actual_gross_pnl_usd.unwrap_or(Decimal::ZERO),
                                ),
                                actual_fees: fmt_usd(ctx.actual_fees_usd.unwrap_or(Decimal::ZERO)),
                                actual_cex_fee: fmt_usd(
                                    ctx.actual_cex_fee_usd.unwrap_or(Decimal::ZERO),
                                ),
                                actual_gas_fee: fmt_usd(
                                    ctx.actual_onchain_gas_fee_usd.unwrap_or(Decimal::ZERO),
                                ),
                                gas_used: ctx.onchain_gas_used.map(|v| v.to_string()),
                            },
                        )
                        .await;
                        // Feed PnL breaker — trips halt when daily loss exceeded.
                        let (is_halted, was_halted, cumulative_pnl, max_daily_loss) = {
                            let mut pb = pnl_breaker.lock().await;
                            let was_h = pb.is_halted();
                            pb.record_pnl(net);
                            (
                                pb.is_halted(),
                                was_h,
                                pb.cumulative_pnl().to_string(),
                                pb.max_daily_loss().to_string(),
                            )
                        };

                        if is_halted && !was_halted {
                            let ev = AlertEvent::DailyLossHalt {
                                cumulative_pnl,
                                max_daily_loss,
                            };
                            emit_best_effort(&alert_sink, &ev).await;
                            halt_coordinator.halt("daily PnL loss threshold exceeded");
                        }

                        peanut_internship_rust::observability::metrics_handle()
                            .set_pnl_breaker_halted(is_halted);
                        risk_manager.lock().await.record_trade_result(net);
                        // Auto kill switch: halt the bot if capital fell below
                        // the absolute minimum safety threshold.
                        {
                            let rm = risk_manager.lock().await;
                            if rm.is_below_absolute_min_capital() {
                                let capital = rm.current_capital();
                                drop(rm); // release lock before async emit + halt
                                let ev = AlertEvent::KillSwitchTriggered {
                                    path: format!(
                                        "capital ${capital:.2} below absolute minimum $50"
                                    ),
                                };
                                emit_best_effort(&alert_sink, &ev).await;
                                halt_coordinator
                                    .halt("capital below absolute minimum — auto kill switch");
                            }
                        }
                    }
                    let record = execution_to_arb_record(&ctx);
                    if let Some(logger) = trade_logger.as_ref()
                        && let Err(e) = logger.append(&record)
                    {
                        error!(error = %e, signal = %ctx.signal.signal_id, "trade log append failed");
                    }
                    pnl.lock().await.record(record);
                } else {
                    risk_manager.lock().await.record_trade_cancelled();
                    warn!(pair = pair_str, state = %ctx.state, error = ?ctx.error, "FAILED");
                }
                for ev in evaluate_execution(&ctx, &alert_rules) {
                    emit_best_effort(&alert_sink, &ev).await;
                }
            }
        });
    }

    // Breaker watcher: emit BreakerOpened / BreakerClosed on state flips.
    // Polls the shared breaker once per second — cheap (a mutex + atomic
    // check) and catches transitions caused by either successful recovery
    // or the next failure pushing it over threshold.
    {
        let breaker = executor.circuit_breaker();
        let alert_sink = Arc::clone(&alert_sink);
        tokio::spawn(async move {
            let mut last_open = false;
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let (is_open, failures) = {
                    let mut guard = breaker.lock().await;
                    (guard.is_open(), guard.failure_count() as u32)
                };
                if is_open != last_open {
                    let ev = if is_open {
                        AlertEvent::BreakerOpened { failures }
                    } else {
                        AlertEvent::BreakerClosed
                    };
                    info!(alert = %ev.summary(), "circuit breaker alert");
                    emit_best_effort(&alert_sink, &ev).await;
                    last_open = is_open;
                }
            }
        });
    }

    // Reconcile worker: polls PendingReconcile entries for their on-chain
    // receipt, marks them resolved/reverted/expired, and fires unwind +
    // alerts on reverts. Requires an RPC URL to be configured.
    if let Some(ref store) = reconcile_store {
        let rpc_urls = resolve_wallet_config(&cli).map(|(rpc, _)| rpc);
        if let Some(rpc_urls) = rpc_urls {
            match ChainClient::new(rpc_urls, RPC_TIMEOUT_SECS, RPC_RETRIES) {
                Ok(chain_client) => {
                    log_rpc_health("reconcile worker", &chain_client).await;
                    let provider = Arc::new(ChainReceiptProvider::new(chain_client));
                    let worker = ReconcileWorker::new(
                        Arc::clone(store),
                        provider,
                        ReconcileConfig {
                            poll_interval: Duration::from_secs(cli.reconcile_poll_secs.max(1)),
                            max_age: Duration::from_secs(cli.reconcile_max_age_secs),
                        },
                    );
                    let alert_sink = Arc::clone(&alert_sink);
                    let legs_for_unwind = Arc::clone(&legs_for_reconcile);
                    let poll_interval = worker.poll_interval();
                    tokio::spawn(async move {
                        info!("reconcile worker started");
                        loop {
                            tokio::select! {
                                _ = tokio::signal::ctrl_c() => {
                                    info!("reconcile worker: shutdown signal received");
                                    break;
                                }
                                _ = tokio::time::sleep(poll_interval) => {}
                            }
                            match worker.tick_once().await {
                                Ok(outcomes) => {
                                    handle_reconcile_outcomes(
                                        outcomes,
                                        &alert_sink,
                                        &legs_for_unwind,
                                    )
                                    .await;
                                }
                                Err(e) => {
                                    warn!(error = %e, "reconcile tick failed");
                                }
                            }
                        }
                    });
                    info!("reconcile worker spawned");
                }
                Err(e) => {
                    warn!(error = %e, "failed to create ChainClient for reconcile worker; worker disabled");
                }
            }
        } else {
            warn!(
                "reconcile store configured but no --eth-rpc-url; worker disabled (store-only mode)"
            );
        }
    }

    // Optional on-chain wallet fetcher. Initialised once and reused across
    // sync passes so the ChainClient's internal connection pool warms up.
    let wallet_fetcher = match resolve_wallet_config(&cli) {
        Some((rpc_urls, addr)) => match WalletBalanceFetcher::new_multi(rpc_urls.clone(), &addr) {
            Ok(f) => {
                let chain_id = cli.dex_chain_id;
                let tokens = WalletBalanceFetcher::default_tokens_for_chain(chain_id);
                let f = f.with_chain_id(chain_id).with_tokens_dynamic(tokens);
                info!(wallet = %addr, rpc_endpoints = rpc_urls.len(), chain_id, "wallet balance fetcher enabled");
                Some(f)
            }
            Err(e) => {
                warn!(error = %e, "failed to init WalletBalanceFetcher; wallet sync disabled");
                None
            }
        },
        None => {
            info!(
                "wallet balance fetcher disabled; missing --eth-rpc-url/ETH_RPC_URL or --wallet-address/WALLET_ADDRESS"
            );
            None
        }
    };

    let mode_str = if cli.dry_run {
        "dry-run"
    } else if production {
        "production"
    } else {
        "testnet"
    };
    let pairs_str = cli.pair.join(",");
    emit_best_effort(
        &alert_sink,
        &AlertEvent::BotStarted {
            mode: mode_str.to_string(),
            pairs: pairs_str.clone(),
        },
    )
    .await;
    peanut_internship_rust::observability::emit_event(
        "bot_started",
        json!({
            "mode": mode_str,
            "pairs": cli.pair,
            "size": trade_size.to_string(),
            "simulation": cli.simulation,
            "dry_run": cli.dry_run,
            "max_concurrent": cli.max_concurrent_executions,
            "metrics_port": cli.metrics_port,
        }),
    );
    info!(
        pairs = ?cli.pair,
        size = %trade_size,
        max_concurrent = cli.max_concurrent_executions,
        wallet_sync = wallet_fetcher.is_some(),
        balance_sync_secs = cli.balance_sync_interval_secs,
        "bot starting"
    );

    let preserve_seeded_inventory =
        (cli.simulation || cli.dry_run) && !cli.seed_inventory.is_empty();
    if preserve_seeded_inventory {
        info!("simulation/dry-run seed inventory active; skipping balance sync");
    } else {
        // Pre-initialize inventory with 0s for the target pairs to avoid noisy
        // "lookup missing" warnings before the first sync completes.
        {
            let mut inv = inventory.write().await;
            for p in &cli.pair {
                if let Ok((base, quote)) = split_pair(p) {
                    for venue in inv.venues().to_vec() {
                        let mut initial_bals = HashMap::new();
                        initial_bals.insert(base.to_string(), Decimal::ZERO);
                        initial_bals.insert(quote.to_string(), Decimal::ZERO);
                        inv.update_from_wallet(venue, initial_bals);
                    }
                }
            }
        }

        let _ = sync_cex_balance(&exchange, &inventory).await;
        if let Some(ref f) = wallet_fetcher {
            sync_wallet_balance(f, &inventory).await;
        }
    }

    let risk_pause_reason = Arc::new(Mutex::new(None::<String>));
    let rebalance_pause_reason = Arc::new(Mutex::new(None::<String>));

    if cli.rebalance_enabled {
        let transfer_context =
            build_rebalance_transfer_context(&cli, Arc::clone(&exchange), nonce_manager.clone())?;
        let rebalance_max_step_usd = Decimal::from_str_exact(&cli.rebalance_max_step_usd)
            .map_err(|e| format!("invalid --rebalance-max-step-usd: {e}"))?;
        let rebalance_balance_verify_tolerance_pct =
            Decimal::from_f64_retain(cli.balance_verify_tolerance_pct).unwrap_or(Decimal::ONE);
        spawn_rebalance_loop(
            Arc::clone(&inventory),
            Arc::clone(&halt_coordinator),
            Arc::clone(&alert_sink),
            Arc::clone(&rebalance_pause_reason),
            Arc::clone(&price_source),
            RebalanceLoopConfig {
                dry_run: cli.rebalance_dry_run,
                interval: Duration::from_secs(cli.rebalance_interval_secs.max(1)),
                threshold_pct: cli.rebalance_threshold_pct,
                quote_asset: cli.rebalance_quote_asset.clone(),
                max_slippage_bps: Decimal::from(cli.rebalance_max_slippage_bps),
                allowed_assets: parse_rebalance_csv_upper(&cli.rebalance_allowed_assets),
                allowed_venues: parse_rebalance_csv_lower(&cli.rebalance_allowed_venues),
                max_step_usd: rebalance_max_step_usd,
                min_fill_pct: cli.rebalance_min_fill_pct,
                pause_trading: cli.rebalance_pause_trading,
                balance_verify_tolerance_pct: rebalance_balance_verify_tolerance_pct,
                journal_path: cli.rebalance_journal_path.clone(),
            },
            transfer_context,
        );
    } else {
        info!("auto-rebalance disabled");
    }

    let sync_interval = Duration::from_secs(cli.balance_sync_interval_secs.max(1));
    let mut last_sync = std::time::Instant::now();
    let tick_deps = TickDeps {
        scorer: Arc::clone(&scorer),
        executor: Arc::clone(&executor),
        queue: Arc::clone(&queue),
        dry_run: cli.dry_run,
        verbose: cli.verbose,
        risk_manager: Arc::clone(&risk_manager),
        risk_pause_reason: Arc::clone(&risk_pause_reason),
        rebalance_pause_reason: Arc::clone(&rebalance_pause_reason),
        alert_sink: Arc::clone(&alert_sink),
        pre_trade_validator: Arc::clone(&pre_trade_validator),
        cex_order_books: Arc::clone(&cex_order_books),
        price_source: Arc::clone(&price_source),
        base_fees: fees.clone(),
        gas_estimator,
        dex_fee_included_in_quote: !cli.dex_address_book.is_empty(),
        dex_pool_fee_bps: load_dex_pool_fee_bps_map(&cli.dex_address_book)?,
        min_profit_usd: configured_min_profit_usd,
    };

    loop {
        // Kill-switch: watchdog file + HTTP halt + PnL breaker all converge
        // here. Once halted, the bot breaks out of the main loop cleanly.
        halt_coordinator.check_watchdog();
        if halt_coordinator.is_halted() {
            let reason = halt_coordinator
                .reason()
                .unwrap_or_else(|| "unknown".into());
            error!(reason = %reason, "BOT HALTED — exiting main loop");
            emit_best_effort(
                &alert_sink,
                &AlertEvent::KillSwitchTriggered {
                    path: reason.clone(),
                },
            )
            .await;
            peanut_internship_rust::observability::emit_event(
                "bot_stopped",
                json!({
                    "reason": reason,
                    "halted": true,
                }),
            );
            break;
        }

        let tick_result = tokio::select! {
            result = tick(&cli.pair, trade_size, min_score, &mut generator, &tick_deps) => result,
            _ = wait_for_halt(Arc::clone(&halt_coordinator)) => Ok(()),
        };
        if halt_coordinator.is_halted() {
            let reason = halt_coordinator
                .reason()
                .unwrap_or_else(|| "unknown".into());
            error!(reason = %reason, "BOT HALTED — exiting main loop");
            emit_best_effort(
                &alert_sink,
                &AlertEvent::BotStopped {
                    reason: reason.clone(),
                },
            )
            .await;
            peanut_internship_rust::observability::emit_event(
                "bot_stopped",
                json!({
                    "reason": reason,
                    "halted": true,
                }),
            );
            break;
        }
        if let Err(e) = tick_result {
            error!("tick error: {e}");
            tokio::time::sleep(Duration::from_secs(DEFAULT_RETRY_DELAY_SECS)).await;
            continue;
        }
        // Gate balance syncs by the configured interval — tight `--tick-ms`
        // loops must not hammer the exchange `/account` or RPC endpoints.
        if last_sync.elapsed() >= sync_interval {
            if !preserve_seeded_inventory {
                let cex_sync_ok = sync_cex_balance(&exchange, &inventory).await;
                // Post-trade balance verification: compare tracked vs actual.
                if cli.balance_verify_tolerance_pct > 0.0 && cex_sync_ok {
                    let tolerance = Decimal::from_f64_retain(cli.balance_verify_tolerance_pct)
                        .unwrap_or(Decimal::ONE);
                    if let Some(mismatches) =
                        verify_cex_balance(&exchange, &inventory, tolerance).await
                    {
                        for m in &mismatches {
                            warn!(
                                venue = %m.venue,
                                asset = %m.asset,
                                tracked = %m.tracked,
                                actual = %m.actual,
                                diff = %m.diff,
                                "balance mismatch detected"
                            );
                            let ev = AlertEvent::BalanceMismatch {
                                venue: m.venue.to_string(),
                                asset: m.asset.clone(),
                                tracked: m.tracked.to_string(),
                                actual: m.actual.to_string(),
                                diff: m.diff.to_string(),
                            };
                            emit_best_effort(&alert_sink, &ev).await;
                        }
                        if !mismatches.is_empty() {
                            halt_coordinator
                                .halt("balance mismatch — manual investigation required");
                        }
                    }
                } else if cli.balance_verify_tolerance_pct > 0.0 {
                    warn!("skipping CEX balance verification because CEX balance sync failed");
                }
                if let Some(ref f) = wallet_fetcher {
                    sync_wallet_balance(f, &inventory).await;
                }
            }
            last_sync = std::time::Instant::now();
        }
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("shutdown signal received — stopping new ticks");
                break;
            }
            _ = tokio::time::sleep(Duration::from_millis(cli.tick_ms)) => {}
        }
    }

    emit_best_effort(
        &alert_sink,
        &AlertEvent::BotStopped {
            reason: "clean shutdown".to_string(),
        },
    )
    .await;
    peanut_internship_rust::observability::emit_event(
        "bot_stopped",
        json!({
            "reason": "clean shutdown",
            "halted": halt_coordinator.is_halted(),
        }),
    );
    info!("main loop exited; draining queue worker");
    if worker_shutdown_tx.send(true).is_err() {
        warn!("queue worker shutdown receiver already closed");
    }
    match tokio::time::timeout(Duration::from_secs(cli.shutdown_drain_secs), worker_handle).await {
        Ok(Ok(())) => info!("queue worker drained cleanly"),
        Ok(Err(e)) => warn!(error = %e, "queue worker task join failed"),
        Err(_) => warn!(
            timeout_s = cli.shutdown_drain_secs,
            "queue worker drain timed out; exiting with work possibly still in-flight"
        ),
    }
    Ok(())
}

struct TickDeps {
    scorer: Arc<Mutex<SignalScorer>>,
    executor: Arc<Executor>,
    queue: Arc<SignalQueue>,
    dry_run: bool,
    verbose: bool,
    risk_manager: Arc<Mutex<RiskManager>>,
    risk_pause_reason: Arc<Mutex<Option<String>>>,
    rebalance_pause_reason: Arc<Mutex<Option<String>>>,
    alert_sink: Arc<dyn AlertSink>,
    pre_trade_validator: Arc<PreTradeValidator>,
    cex_order_books: Arc<dyn CexOrderBookSource>,
    price_source: Arc<AnyPriceSource>,
    base_fees: FeeStructure,
    gas_estimator: GasFeeEstimator,
    dex_fee_included_in_quote: bool,
    dex_pool_fee_bps: HashMap<String, Decimal>,
    min_profit_usd: Decimal,
}

#[derive(Clone)]
enum GasFeeEstimator {
    Fixed,
    Rpc {
        client: ChainClient,
        gas_units: u64,
        buffer_bps: u64,
    },
    Estimate {
        client: ChainClient,
        fallback_gas_units: u64,
        buffer_bps: u64,
        wallet: Address,
        address_book: Arc<PairAddressBook>,
        slippage_bps: u64,
        chain_id: u64,
        v2_router: Address,
        v3_router: Address,
    },
    Anvil {
        client: ChainClient,
        simulator: ForkSimulator,
        fallback_gas_units: u64,
        buffer_bps: u64,
        wallet: Address,
        address_book: Arc<PairAddressBook>,
        slippage_bps: u64,
        chain_id: u64,
        v2_router: Address,
        v3_router: Address,
    },
}

#[derive(Clone)]
struct GasFeeEstimate {
    mode: &'static str,
    gas_units: u64,
    max_fee_gwei: Decimal,
    eth_usd: Option<Decimal>,
    gas_usd: Decimal,
}

impl GasFeeEstimator {
    async fn estimate_usd(
        &self,
        configured_gas_usd: Decimal,
        eth_usd: Option<Decimal>,
    ) -> Result<GasFeeEstimate, Box<dyn std::error::Error>> {
        match self {
            Self::Fixed => Ok(GasFeeEstimate {
                mode: "fixed",
                gas_units: 0,
                max_fee_gwei: Decimal::ZERO,
                eth_usd: None,
                gas_usd: configured_gas_usd,
            }),
            Self::Rpc {
                client,
                gas_units,
                buffer_bps,
            }
            | Self::Estimate {
                client,
                fallback_gas_units: gas_units,
                buffer_bps,
                ..
            }
            | Self::Anvil {
                client,
                fallback_gas_units: gas_units,
                buffer_bps,
                ..
            } => {
                let eth_usd =
                    eth_usd.ok_or("--fee-gas-mode rpc/estimate/anvil requires ETH/USDC price")?;
                let gas_price = client.get_gas_price().await?;
                let max_fee = gas_price.get_max_fee(GasPriority::Medium, *buffer_bps);
                let max_fee_wei = u256_to_decimal(max_fee)?;
                let gas_units_decimal = Decimal::from(*gas_units);
                let wei_per_eth = Decimal::from(WEI_PER_ETH);
                let gas_usd = max_fee_wei * gas_units_decimal / wei_per_eth * eth_usd;
                Ok(GasFeeEstimate {
                    mode: match self {
                        Self::Estimate { .. } => "rpc-fallback",
                        Self::Anvil { .. } => "anvil-fallback",
                        Self::Rpc { .. } => "rpc",
                        Self::Fixed => "fixed",
                    },
                    gas_units: *gas_units,
                    max_fee_gwei: max_fee_wei / Decimal::from(WEI_PER_GWEI),
                    eth_usd: Some(eth_usd),
                    gas_usd,
                })
            }
        }
    }
}

fn u256_to_decimal(value: U256) -> Result<Decimal, Box<dyn std::error::Error>> {
    Decimal::from_str_exact(&value.to_string())
        .map_err(|e| format!("failed to convert U256 {value} to Decimal: {e}").into())
}

async fn gas_units_to_usd(
    client: &ChainClient,
    gas_units: u64,
    buffer_bps: u64,
    eth_usd: Decimal,
    mode: &'static str,
) -> Result<GasFeeEstimate, Box<dyn std::error::Error>> {
    let gas_price = client.get_gas_price().await?;
    let max_fee = gas_price.get_max_fee(GasPriority::Medium, buffer_bps);
    let max_fee_wei = u256_to_decimal(max_fee)?;
    let wei_per_eth = Decimal::from(WEI_PER_ETH);
    let gas_usd = max_fee_wei * Decimal::from(gas_units) / wei_per_eth * eth_usd;
    Ok(GasFeeEstimate {
        mode,
        gas_units,
        max_fee_gwei: max_fee_wei / Decimal::from(WEI_PER_GWEI),
        eth_usd: Some(eth_usd),
        gas_usd,
    })
}

fn decimal_to_u256_scaled(
    value: Decimal,
    decimals: u8,
) -> Result<U256, Box<dyn std::error::Error>> {
    if value < Decimal::ZERO {
        return Err(format!("negative amount: {value}").into());
    }
    let scale = Decimal::from(10u64.pow(decimals as u32));
    let scaled = (value * scale).trunc();
    U256::from_dec_str(&scaled.to_string())
        .map_err(|e| format!("scale overflow ({scaled}): {e}").into())
}

fn decode_u256_return(raw: &[u8]) -> Result<U256, Box<dyn std::error::Error>> {
    if raw.len() < 32 {
        return Err(format!("expected uint256 return, got {} bytes", raw.len()).into());
    }
    Ok(U256::from_big_endian(&raw[raw.len() - 32..]))
}

fn current_unix_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

async fn erc20_allowance(
    client: &ChainClient,
    token: &Address,
    owner: &Address,
    spender: &Address,
    chain_id: u64,
) -> Result<U256, Box<dyn std::error::Error>> {
    let req = TransactionRequest::contract_call(
        token.clone(),
        build_allowance_calldata(owner, spender),
        chain_id,
    );
    let raw = client.call(&req, BlockId::Latest).await?;
    decode_u256_return(&raw)
}

async fn estimate_signal_gas(
    estimator: &GasFeeEstimator,
    price_source: &AnyPriceSource,
    signal: &Signal,
) -> Result<Option<GasFeeEstimate>, Box<dyn std::error::Error>> {
    let (
        client,
        simulator,
        fallback_gas_units,
        buffer_bps,
        wallet,
        address_book,
        slippage_bps,
        chain_id,
        v2_router,
        v3_router,
    ) = match estimator {
        GasFeeEstimator::Estimate {
            client,
            fallback_gas_units,
            buffer_bps,
            wallet,
            address_book,
            slippage_bps,
            chain_id,
            v2_router,
            v3_router,
        } => (
            client,
            None,
            fallback_gas_units,
            buffer_bps,
            wallet,
            address_book,
            slippage_bps,
            chain_id,
            v2_router,
            v3_router,
        ),
        GasFeeEstimator::Anvil {
            client,
            simulator,
            fallback_gas_units,
            buffer_bps,
            wallet,
            address_book,
            slippage_bps,
            chain_id,
            v2_router,
            v3_router,
        } => (
            client,
            Some(simulator),
            fallback_gas_units,
            buffer_bps,
            wallet,
            address_book,
            slippage_bps,
            chain_id,
            v2_router,
            v3_router,
        ),
        _ => return Ok(None),
    };
    let tokens = address_book
        .get(&signal.pair)
        .ok_or_else(|| format!("missing address book entry for {}", signal.pair))?;
    let (token_in, token_out, amount_in_units, expected_out_units, decimals_in, decimals_out) =
        match signal.direction {
            Direction::BuyCexSellDex => (
                &tokens.base,
                &tokens.quote,
                signal.size,
                signal.size * signal.dex_price,
                tokens.base_decimals,
                tokens.quote_decimals,
            ),
            Direction::BuyDexSellCex => (
                &tokens.quote,
                &tokens.base,
                signal.size * signal.dex_price,
                signal.size,
                tokens.quote_decimals,
                tokens.base_decimals,
            ),
        };
    let router = match tokens.pool_kind {
        DexPoolKind::V2 => v2_router,
        DexPoolKind::V3 => v3_router,
    };
    let amount_in = decimal_to_u256_scaled(amount_in_units, decimals_in)?;
    let expected_out = decimal_to_u256_scaled(expected_out_units, decimals_out)?;
    let min_out = apply_slippage(expected_out, *slippage_bps);
    if min_out.is_zero() {
        return Err("estimate gas min_out is zero".into());
    }

    let eth_usd = price_source.get_latest_price("ETH/USDC").await?;
    let allowance = erc20_allowance(client, token_in, wallet, router, *chain_id).await?;
    let mut approval_gas = 0u64;
    let mut swap_gas_mode = "estimate";
    let swap_gas = if allowance >= amount_in {
        let deadline = U256::from(current_unix_ts().saturating_add(60));
        let calldata = match tokens.pool_kind {
            DexPoolKind::V2 => {
                let path = vec![token_in.clone(), token_out.clone()];
                build_swap_calldata(amount_in, min_out, &path, wallet, deadline)
            }
            DexPoolKind::V3 => v3_swap_calldata_for_pair(
                tokens, token_in, token_out, wallet, deadline, amount_in, min_out,
            )?,
        };
        if let Some(simulator) = simulator {
            let decoder = match tokens.pool_kind {
                DexPoolKind::V2 => AmountOutDecoder::UniswapV2Amounts,
                DexPoolKind::V3 => AmountOutDecoder::SingleUint,
            };
            let result = simulator
                .simulate_swap(
                    router.clone(),
                    SwapParams {
                        calldata: Bytes::from(calldata),
                        value: U256::zero(),
                        gas_limit: None,
                        decoder,
                    },
                    wallet.clone(),
                )
                .await?;
            if !result.success {
                return Err(format!(
                    "anvil fork swap simulation failed: {}",
                    result.error.unwrap_or_else(|| "unknown revert".into())
                )
                .into());
            }
            if result.gas_used == 0 {
                return Err("anvil fork gas simulation returned zero gas".into());
            }
            swap_gas_mode = "anvil";
            result.gas_used
        } else {
            let tx = TransactionRequest::contract_call(router.clone(), calldata, *chain_id);
            match client.estimate_gas_from(&tx, wallet).await {
                Ok(gas) => gas,
                Err(e) => {
                    debug!(
                        pair = %signal.pair,
                        error = %e,
                        fallback = *fallback_gas_units,
                        "swap gas estimation failed (wallet may lack WETH); using fallback"
                    );
                    swap_gas_mode = "estimate-fallback";
                    *fallback_gas_units
                }
            }
        }
    } else {
        let approve_tx = TransactionRequest::contract_call(
            token_in.clone(),
            build_approve_calldata(router, U256::MAX),
            *chain_id,
        );
        approval_gas = client.estimate_gas_from(&approve_tx, wallet).await?;
        swap_gas_mode = if simulator.is_some() {
            "anvil+approve+fallback"
        } else {
            "estimate+approve+fallback"
        };
        *fallback_gas_units
    };
    let total_gas = approval_gas.saturating_add(swap_gas);
    Ok(Some(
        gas_units_to_usd(client, total_gas, *buffer_bps, eth_usd, swap_gas_mode).await?,
    ))
}

fn apply_gas_estimate_to_signal(
    signal: &mut Signal,
    base_fees: &FeeStructure,
    gas: &GasFeeEstimate,
) {
    let mut fees = base_fees.clone();
    fees.gas_cost_usd = gas.gas_usd;
    let breakdown = fees.breakdown(signal.notional_usd);
    signal.expected_fees = breakdown.total_fee_usd;
    signal.expected_net_pnl = signal.expected_gross_pnl - signal.expected_fees;
}

async fn estimate_tick_fees(
    deps: &TickDeps,
) -> Result<(FeeStructure, GasFeeEstimate), Box<dyn std::error::Error>> {
    let eth_usd = match &deps.gas_estimator {
        GasFeeEstimator::Fixed => None,
        GasFeeEstimator::Rpc { .. }
        | GasFeeEstimator::Estimate { .. }
        | GasFeeEstimator::Anvil { .. } => {
            Some(deps.price_source.get_latest_price("ETH/USDC").await?)
        }
    };
    let gas = deps
        .gas_estimator
        .estimate_usd(deps.base_fees.gas_cost_usd, eth_usd)
        .await?;
    let mut fees = deps.base_fees.clone();
    fees.gas_cost_usd = gas.gas_usd;
    Ok((fees, gas))
}

struct ExpectedPnlPreview {
    direction: Direction,
    cex_price: Decimal,
    dex_price: Decimal,
    spread_bps: Decimal,
    quote_usd_price: Decimal,
    trade_value_usd: Decimal,
    breakeven_spread_bps: Decimal,
    missing_profit_usd: Decimal,
    expected_gross_pnl: Decimal,
    expected_fees: Decimal,
    fee_breakdown: FeeBreakdown,
    expected_net_pnl: Decimal,
}

fn expected_pnl_preview(
    market: &MarketState,
    fees: &FeeStructure,
    min_profit_usd: Decimal,
) -> ExpectedPnlPreview {
    let buy_cex_better = market.spread_buy_cex_bps >= market.spread_buy_dex_bps;
    let direction = if buy_cex_better {
        Direction::BuyCexSellDex
    } else {
        Direction::BuyDexSellCex
    };
    let cex_price = if buy_cex_better {
        market.cex_ask
    } else {
        market.cex_bid
    };
    let dex_price = if buy_cex_better {
        market.dex_sell
    } else {
        market.dex_buy
    };
    let spread_bps = if buy_cex_better {
        market.spread_buy_cex_bps
    } else {
        market.spread_buy_dex_bps
    };
    let trade_value_usd = market.size * cex_price * market.quote_usd_price;
    let bps = Decimal::from(BPS_SCALE);
    let expected_gross_pnl = spread_bps / bps * trade_value_usd;
    let fee_breakdown = fees.breakdown(trade_value_usd);
    let expected_fees = fee_breakdown.total_fee_usd;
    let expected_net_pnl = expected_gross_pnl - expected_fees;
    let breakeven_spread_bps = if trade_value_usd <= Decimal::ZERO {
        Decimal::MAX
    } else {
        (expected_fees + min_profit_usd) / trade_value_usd * bps
    };
    let missing_profit_usd = (min_profit_usd - expected_net_pnl).max(Decimal::ZERO);
    ExpectedPnlPreview {
        direction,
        cex_price,
        dex_price,
        spread_bps,
        quote_usd_price: market.quote_usd_price,
        trade_value_usd,
        breakeven_spread_bps,
        missing_profit_usd,
        expected_gross_pnl,
        expected_fees,
        fee_breakdown,
        expected_net_pnl,
    }
}

fn dry_run_preview_signal(
    pair: &str,
    market: &MarketState,
    preview: &ExpectedPnlPreview,
) -> Signal {
    let now = Utc::now();
    Signal {
        signal_id: format!("dry-run-preview-{pair}"),
        pair: pair.to_string(),
        direction: preview.direction,
        cex_price: preview.cex_price,
        dex_price: preview.dex_price,
        spread_bps: preview.spread_bps,
        size: market.size,
        notional_quote: market.size * preview.cex_price,
        notional_usd: preview.trade_value_usd,
        expected_gross_pnl: preview.expected_gross_pnl,
        expected_fees: preview.expected_fees,
        expected_net_pnl: preview.expected_net_pnl,
        score: Decimal::ZERO,
        timestamp: now,
        expiry: now + chrono::Duration::seconds(60),
        inventory_ok: true,
        within_limits: true,
    }
}

fn fmt_gas_units(gas: &GasFeeEstimate) -> String {
    if gas.gas_units == 0 {
        "-".to_string()
    } else {
        gas.gas_units.to_string()
    }
}

fn fmt_max_fee_gwei(gas: &GasFeeEstimate) -> String {
    if gas.gas_units == 0 {
        "-".to_string()
    } else {
        format!("{} gwei", format::fmt_qty(gas.max_fee_gwei))
    }
}

fn fmt_dex_fee_mode(included_in_quote: bool, pool_fee_bps: Option<Decimal>) -> String {
    let pool_fee = pool_fee_bps
        .map(|bps| format!("pool fee {}", format::fmt_bps(bps)))
        .unwrap_or_else(|| "pool fee unknown".to_string());
    if included_in_quote {
        format!("included in quote; {pool_fee}")
    } else {
        format!("explicit; {pool_fee}")
    }
}

fn render_dry_run_opportunity(
    pair: &str,
    status: &str,
    market: &MarketState,
    preview: &ExpectedPnlPreview,
    gas: &GasFeeEstimate,
    dex_fee_included_in_quote: bool,
    dex_pool_fee_bps: Option<Decimal>,
) -> String {
    format!(
        "\n┌─ 🧪 DRY RUN OPPORTUNITY ─────────────────────────────────────\n\
         │ pair         │ {pair}\n\
         │ status       │ {status}\n\
         │ direction    │ {direction}\n\
         │ size         │ {size}\n\
         │ notional     │ {notional}\n\
         │ quote/USD    │ {quote_usd}\n\
         ├─ Prices ─────────────────────────────────────────────\n\
         │ CEX bid/ask  │ {cex_bid} / {cex_ask}\n\
         │ DEX buy/sell │ {dex_buy} / {dex_sell}\n\
         │ best CEX/DEX │ {best_cex} / {best_dex}\n\
         ├─ Edge ───────────────────────────────────────────────\n\
         │ best spread  │ {best_spread}\n\
         │ buy CEX      │ {buy_cex_bps}\n\
         │ buy DEX      │ {buy_dex_bps}\n\
         │ breakeven    │ {breakeven}\n\
         ├─ Fees ───────────────────────────────────────────────\n\
         │ CEX taker    │ {cex_fee}\n\
         │ DEX pool     │ {dex_fee} ({dex_fee_mode})\n\
         │ gas          │ {gas_fee} ({gas_mode})\n\
         │ gas units    │ {gas_units}\n\
         │ max fee      │ {max_fee_gwei}\n\
         │ ETH/USD      │ {gas_eth_usd}\n\
         │ total fees   │ {fees} / {fee_bps}\n\
         ├─ Expected PnL ───────────────────────────────────────\n\
         │ gross        │ {gross}\n\
         │ net          │ {net}\n\
         │ missing      │ {missing}\n\
         └──────────────────────────────────────────────────────",
        direction = preview.direction,
        size = fmt_qty(market.size),
        notional = fmt_usd(preview.trade_value_usd),
        quote_usd = fmt_usd(preview.quote_usd_price),
        cex_bid = fmt_price(market.cex_bid),
        cex_ask = fmt_price(market.cex_ask),
        dex_buy = fmt_price(market.dex_buy),
        dex_sell = fmt_price(market.dex_sell),
        best_cex = fmt_price(preview.cex_price),
        best_dex = fmt_price(preview.dex_price),
        best_spread = fmt_bps(preview.spread_bps),
        buy_cex_bps = fmt_bps(market.spread_buy_cex_bps),
        buy_dex_bps = fmt_bps(market.spread_buy_dex_bps),
        breakeven = fmt_bps(preview.breakeven_spread_bps),
        cex_fee = fmt_usd(preview.fee_breakdown.cex_fee_usd),
        dex_fee = fmt_usd(preview.fee_breakdown.dex_fee_usd),
        dex_fee_mode = fmt_dex_fee_mode(dex_fee_included_in_quote, dex_pool_fee_bps),
        gas_fee = fmt_usd(preview.fee_breakdown.gas_fee_usd),
        gas_mode = gas.mode,
        gas_units = fmt_gas_units(gas),
        max_fee_gwei = fmt_max_fee_gwei(gas),
        gas_eth_usd = fmt_optional_usd(gas.eth_usd),
        gross = fmt_usd(preview.expected_gross_pnl),
        fees = fmt_usd(preview.expected_fees),
        fee_bps = fmt_bps(preview.fee_breakdown.total_fee_bps),
        net = fmt_usd(preview.expected_net_pnl),
        missing = fmt_usd(preview.missing_profit_usd),
    )
}

#[derive(Clone)]
struct SharedFileWriter {
    file: Arc<StdMutex<File>>,
}

struct SharedFileGuard {
    file: Arc<StdMutex<File>>,
}

impl<'a> MakeWriter<'a> for SharedFileWriter {
    type Writer = SharedFileGuard;

    fn make_writer(&'a self) -> Self::Writer {
        SharedFileGuard {
            file: Arc::clone(&self.file),
        }
    }
}

impl Write for SharedFileGuard {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut guard = self
            .file
            .lock()
            .map_err(|_| io::Error::other("log file mutex poisoned"))?;
        guard.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut guard = self
            .file
            .lock()
            .map_err(|_| io::Error::other("log file mutex poisoned"))?;
        guard.flush()
    }
}

fn init_tracing(log_dir: impl AsRef<Path>) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let log_dir = log_dir.as_ref();
    fs::create_dir_all(log_dir)?;
    let timestamp = Utc::now().format("%Y%m%d_%H%M%S");
    let log_path = log_dir.join(format!("bot_{timestamp}.log"));
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let file_writer = SharedFileWriter {
        file: Arc::new(StdMutex::new(file)),
    };
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let stdout_layer = tracing_subscriber::fmt::layer()
        .with_writer(io::stdout)
        .with_ansi(false)
        .with_target(false)
        .with_thread_ids(false)
        .with_thread_names(false);
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(file_writer)
        .with_ansi(false)
        .with_target(false)
        .with_thread_ids(false)
        .with_thread_names(false);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(stdout_layer)
        .with(file_layer)
        .init();

    Ok(log_path)
}

async fn wait_for_halt(halt: Arc<HaltCoordinator>) {
    let mut interval = tokio::time::interval(Duration::from_millis(250));
    loop {
        interval.tick().await;
        if halt.is_halted() {
            break;
        }
    }
}

fn risk_pause_key(reason: &str) -> String {
    if reason.starts_with("Hourly trade limit reached") {
        "Hourly trade limit reached".to_string()
    } else {
        reason.to_string()
    }
}

async fn tick(
    pairs: &[String],
    size: Decimal,
    min_score: Decimal,
    generator: &mut SignalGenerator<AnyPriceSource>,
    deps: &TickDeps,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(reason) = deps.rebalance_pause_reason.lock().await.clone() {
        let key = risk_pause_key(&reason);
        let should_alert = {
            let mut last = deps.risk_pause_reason.lock().await;
            if last.as_deref() == Some(key.as_str()) {
                false
            } else {
                *last = Some(key);
                true
            }
        };
        if should_alert {
            warn!("⏸ REBALANCE PAUSE | {}", reason);
            emit_best_effort(
                &deps.alert_sink,
                &AlertEvent::RiskPaused {
                    reason: reason.clone(),
                },
            )
            .await;
        } else {
            info!("⏸ REBALANCE PAUSE active | {}", reason);
        }
        return Ok(());
    }

    let pause_decision = deps.risk_manager.lock().await.trading_pause();
    if !pause_decision.allowed() {
        let reason = pause_decision.reason().to_string();
        let key = risk_pause_key(&reason);
        let should_alert = {
            let mut last = deps.risk_pause_reason.lock().await;
            if last.as_deref() == Some(key.as_str()) {
                false
            } else {
                *last = Some(key);
                true
            }
        };
        if should_alert {
            warn!("⏸ RISK PAUSE | {}", reason);
            emit_best_effort(
                &deps.alert_sink,
                &AlertEvent::RiskPaused {
                    reason: reason.clone(),
                },
            )
            .await;
        } else {
            info!("⏸ RISK PAUSE active | {}", reason);
        }
        return Ok(());
    }

    let resumed_reason = deps.risk_pause_reason.lock().await.take();
    if let Some(reason) = resumed_reason {
        info!("▶️ RISK PAUSE CLEARED | {}", reason);
        emit_best_effort(&deps.alert_sink, &AlertEvent::RiskResumed { reason }).await;
    }

    // Fast path: skip the tick entirely when the breaker is open. The queue
    // worker also pre-flight-checks the breaker via `Executor::execute`, but
    // filtering here avoids burning cycles on signal generation we know we
    // won't act on.
    let cb = deps.executor.circuit_breaker();
    {
        let mut cb_guard = cb.lock().await;
        if cb_guard.is_open() {
            let remaining = cb_guard.time_until_reset();
            info!(
                "⏸ CIRCUIT BREAKER active | trades blocked | resumes in ~{}s",
                remaining.as_secs()
            );
            return Ok(());
        }
    }

    let (tick_fees, gas_estimate) = estimate_tick_fees(deps).await?;
    generator.set_fees(tick_fees.clone());

    for pair in pairs {
        let (maybe_signal, maybe_market) = match generator.generate(pair, size).await {
            Ok(s) => s,
            Err(e) => {
                warn!(pair, "signal generation failed: {e}");
                continue;
            }
        };
        let mut signal = match maybe_signal {
            Some(s) => s,
            None => {
                if deps.dry_run {
                    if let Some(m) = maybe_market {
                        let mut preview = expected_pnl_preview(&m, &tick_fees, deps.min_profit_usd);
                        let mut preview_gas_estimate = gas_estimate.clone();
                        let preview_signal = dry_run_preview_signal(pair, &m, &preview);
                        if let Some(exact_gas) = estimate_signal_gas(
                            &deps.gas_estimator,
                            deps.price_source.as_ref(),
                            &preview_signal,
                        )
                        .await?
                        {
                            let mut exact_fees = deps.base_fees.clone();
                            exact_fees.gas_cost_usd = exact_gas.gas_usd;
                            preview = expected_pnl_preview(&m, &exact_fees, deps.min_profit_usd);
                            preview_gas_estimate = exact_gas;
                        }
                        let preview_status = if preview.expected_net_pnl >= deps.min_profit_usd {
                            "would_signal_after_exact_gas"
                        } else {
                            "no_signal"
                        };
                        info!(
                            "{}",
                            render_dry_run_opportunity(
                                pair,
                                preview_status,
                                &m,
                                &preview,
                                &preview_gas_estimate,
                                deps.dex_fee_included_in_quote,
                                deps.dex_pool_fee_bps.get(pair).copied()
                            )
                        );
                    } else {
                        if deps.verbose {
                            info!(pair, size = %size, "DRY_RUN no_signal (no market info)");
                        }
                    }
                }
                continue;
            }
        };

        let mut signal_gas_estimate = gas_estimate.clone();
        if let Some(exact_gas) =
            estimate_signal_gas(&deps.gas_estimator, deps.price_source.as_ref(), &signal).await?
        {
            apply_gas_estimate_to_signal(&mut signal, &deps.base_fees, &exact_gas);
            signal_gas_estimate = exact_gas;
        }
        if signal.expected_net_pnl < deps.min_profit_usd {
            info!(
                pair,
                status = "skipped_low_profit_after_gas_estimate",
                direction = %signal.direction,
                size = %signal.size,
                notional_usd = %signal.notional_usd,
                spread_bps = %signal.spread_bps,
                expected_gross_pnl_usd = %signal.expected_gross_pnl,
                expected_fees_usd = %signal.expected_fees,
                expected_net_pnl_usd = %signal.expected_net_pnl,
                min_profit_usd = %deps.min_profit_usd,
                gas_mode = signal_gas_estimate.mode,
                gas_units = signal_gas_estimate.gas_units,
                gas_fee_usd = %signal_gas_estimate.gas_usd,
                "🟡 trade skipped"
            );
            continue;
        }

        let validation = deps.pre_trade_validator.validate_signal(&signal);
        if !validation.allowed() {
            warn!(
                pair,
                status = "rejected_pre_trade",
                reason = validation.reason(),
                direction = %signal.direction,
                size = %signal.size,
                notional_usd = %signal.notional_usd,
                spread_bps = %signal.spread_bps,
                expected_gross_pnl_usd = %signal.expected_gross_pnl,
                expected_fees_usd = %signal.expected_fees,
                expected_net_pnl_usd = %signal.expected_net_pnl,
                "⛔ trade rejected"
            );
            continue;
        }

        let risk_decision = deps.risk_manager.lock().await.check_pre_trade(&signal);
        if !risk_decision.allowed() {
            warn!(
                pair,
                status = "rejected_risk",
                reason = risk_decision.reason(),
                direction = %signal.direction,
                size = %signal.size,
                notional_usd = %signal.notional_usd,
                spread_bps = %signal.spread_bps,
                expected_gross_pnl_usd = %signal.expected_gross_pnl,
                expected_fees_usd = %signal.expected_fees,
                expected_net_pnl_usd = %signal.expected_net_pnl,
                "⛔ trade rejected"
            );
            continue;
        }

        // Score + threshold. Acquire scorer lock briefly; don't hold across
        // the queue push below (push takes its own lock internally).
        let skews = generator_inventory_skews(generator).await;
        let book_result = deps
            .cex_order_books
            .fetch_order_book(pair, DEFAULT_ORDERBOOK_DEPTH)
            .await;
        let book_ref = match &book_result {
            Ok(b) => Some(b),
            Err(e) => {
                warn!(pair, error = %e, "failed to fetch order book for scorer");
                None
            }
        };
        {
            let scorer_guard = deps.scorer.lock().await;
            signal.score = scorer_guard.score(&signal, &skews, book_ref);
        }

        if signal.score < min_score {
            info!(
                pair,
                status = "skipped_low_score",
                direction = %signal.direction,
                spread_bps = %signal.spread_bps,
                size = %signal.size,
                notional_usd = %signal.notional_usd,
                score = %signal.score,
                min_score = %min_score,
                expected_gross_pnl_usd = %signal.expected_gross_pnl,
                expected_fees_usd = %signal.expected_fees,
                expected_net_pnl_usd = %signal.expected_net_pnl,
                "🟡 trade skipped"
            );
            continue;
        }

        if deps.dry_run {
            info!(
                pair,
                status = "would_trade",
                direction = %signal.direction,
                size = %signal.size,
                notional_usd = %signal.notional_usd,
                cex_price = %signal.cex_price,
                dex_price = %signal.dex_price,
                spread_bps = %signal.spread_bps,
                expected_gross_pnl_usd = %signal.expected_gross_pnl,
                expected_fees_usd = %signal.expected_fees,
                expected_net_pnl_usd = %signal.expected_net_pnl,
                gas_mode = signal_gas_estimate.mode,
                gas_units = signal_gas_estimate.gas_units,
                gas_fee_usd = %signal_gas_estimate.gas_usd,
                max_fee_gwei = %signal_gas_estimate.max_fee_gwei,
                score = %signal.score,
                "✅ DRY_RUN would_trade"
            );
            continue;
        }

        info!(
            "🚀 ENQUEUE | pair={} | direction={} | size={} | notional={} | spread={} | score={} | expected_net={} | gas={}",
            pair,
            signal.direction,
            fmt_qty(signal.size),
            fmt_usd(signal.notional_usd),
            fmt_bps(signal.spread_bps),
            signal.score.round_dp(1),
            fmt_usd(signal.expected_net_pnl),
            fmt_usd(signal_gas_estimate.gas_usd)
        );

        deps.risk_manager.lock().await.record_trade_started();
        // Enqueue for the worker to pick up. Push returns `false` when the
        // signal lost backpressure (queue full and score was the weakest).
        if !deps.queue.push(signal).await {
            deps.risk_manager.lock().await.record_trade_not_started();
            warn!(pair, "signal dropped by queue (backpressure)");
        }
    }
    Ok(())
}

/// Snapshots the current skew set from the tracker held by `generator`.
///
/// Wrapped in a helper so we only hold the read-lock for the duration of the
/// `get_skews` call — never across an await.
async fn generator_inventory_skews(
    generator: &SignalGenerator<AnyPriceSource>,
) -> Vec<peanut_internship_rust::exchange::types::SkewResult> {
    generator.tracker().read().await.get_skews()
}

/// Snapshot the CEX (Binance) balance into `inventory`. Errors are logged
/// and swallowed — a transient `/account` failure must not kill the bot.
async fn sync_cex_balance(
    exchange: &Arc<ExchangeClient>,
    inventory: &Arc<RwLock<InventoryTracker>>,
) -> bool {
    match exchange.fetch_balance().await {
        Ok(balances) => {
            for (asset, bal) in &balances {
                if bal.total > Decimal::ZERO {
                    info!(asset, total_balance = %bal.total, "Fetched CEX token balance");
                }
            }

            inventory
                .write()
                .await
                .update_from_cex(Venue::Binance, balances);
            true
        }
        Err(e) => {
            warn!("fetch_balance failed: {e}");
            false
        }
    }
}

/// Verify CEX balances: fetch fresh balances, compare against tracked, and
/// return any mismatches exceeding the tolerance threshold.
/// Returns `None` on fetch failure (verification skipped).
async fn verify_cex_balance(
    exchange: &Arc<ExchangeClient>,
    inventory: &Arc<RwLock<InventoryTracker>>,
    tolerance_pct: Decimal,
) -> Option<Vec<peanut_internship_rust::inventory::tracker::BalanceMismatch>> {
    let fresh = exchange.fetch_balance().await.ok()?;

    // Canonicalize assets using unified logic
    let mut canonical_fresh = HashMap::new();
    for (asset, bal) in fresh {
        canonical_fresh.insert(assets::canonicalize_asset(&asset), bal);
    }
    let fresh = canonical_fresh;

    // Build a simple total-balance map from the NormalizedBalance values.
    let actual: HashMap<String, Decimal> = fresh
        .iter()
        .map(|(asset, bal)| (asset.clone(), bal.free + bal.locked))
        .collect();
    let mismatches = inventory
        .read()
        .await
        .verify_balances(Venue::Binance, &actual, tolerance_pct);

    if mismatches.is_empty() {
        None
    } else {
        Some(mismatches)
    }
}

/// Snapshot on-chain wallet balances (native + well-known ERC-20) into
/// `inventory`. Errors are logged and swallowed.
async fn sync_wallet_balance(
    fetcher: &WalletBalanceFetcher,
    inventory: &Arc<RwLock<InventoryTracker>>,
) {
    match fetcher.fetch_balances().await {
        Ok(bals) => {
            inventory
                .write()
                .await
                .update_from_wallet(Venue::Wallet, bals);
        }
        Err(e) => warn!("wallet fetch_balances failed: {e}"),
    }
}

fn run_config_check(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    Decimal::from_str_exact(&cli.size).map_err(|e| format!("invalid --size: {e}"))?;
    Decimal::from_str_exact(&cli.max_daily_loss_usd)
        .map_err(|e| format!("invalid --max-daily-loss-usd: {e}"))?;
    Decimal::from_str_exact(&cli.initial_capital_usd)
        .map_err(|e| format!("invalid --initial-capital-usd: {e}"))?;
    Decimal::from_str_exact(&cli.risk_max_trade_usd)
        .map_err(|e| format!("invalid --risk-max-trade-usd: {e}"))?;
    Decimal::from_str_exact(&cli.risk_max_daily_loss_usd)
        .map_err(|e| format!("invalid --risk-max-daily-loss-usd: {e}"))?;
    Decimal::from_str_exact(&cli.fee_gas_usd).map_err(|e| format!("invalid --fee-gas-usd: {e}"))?;
    Decimal::from_str_exact(&cli.rebalance_max_step_usd)
        .map_err(|e| format!("invalid --rebalance-max-step-usd: {e}"))?;
    if !(0.0..=1.0).contains(&cli.rebalance_min_fill_pct) {
        return Err(format!(
            "--rebalance-min-fill-pct must be between 0 and 1, got {}",
            cli.rebalance_min_fill_pct
        )
        .into());
    }
    if cli.rebalance_enabled {
        if parse_rebalance_csv_upper(&cli.rebalance_allowed_assets).is_empty() {
            return Err("--rebalance-allowed-assets must not be empty".into());
        }
        if parse_rebalance_csv_lower(&cli.rebalance_allowed_venues).is_empty() {
            return Err("--rebalance-allowed-venues must not be empty".into());
        }
    }
    match cli.fee_gas_mode.to_ascii_lowercase().as_str() {
        "fixed" => {}
        "rpc" => {
            resolve_rpc_urls(cli)
                .ok_or("--fee-gas-mode rpc requires --eth-rpc-url or ETH_RPC_URL")?;
        }
        "estimate" => {
            resolve_wallet_config(cli).ok_or(
                "--fee-gas-mode estimate requires --eth-rpc-url/ETH_RPC_URL and --wallet-address/WALLET_ADDRESS",
            )?;
            if cli.dex_address_book.is_empty() {
                return Err("--fee-gas-mode estimate requires --dex-address-book".into());
            }
        }
        "anvil" => {
            resolve_anvil_fork_url(cli)
                .ok_or("--fee-gas-mode anvil requires --anvil-fork-url or ANVIL_FORK_URL")?;
            resolve_wallet_address(cli)
                .ok_or("--fee-gas-mode anvil requires --wallet-address or WALLET_ADDRESS")?;
            if cli.dex_address_book.is_empty() {
                return Err("--fee-gas-mode anvil requires --dex-address-book".into());
            }
        }
        other => {
            return Err(format!(
                "invalid --fee-gas-mode '{other}', expected fixed, rpc, estimate, or anvil"
            )
            .into());
        }
    }
    effective_dex_fee_bps(cli.fee_dex_swap_bps, &cli.dex_address_book)?;
    validate_live_execution_config(cli)?;
    if !cli.dex_address_book.is_empty() {
        let _address_book = load_address_book(&cli.dex_address_book)?;
        let tracked_pairs = tracked_pairs_for_price_source(&cli.pair);
        let _live_pools = load_live_pool_book(&cli.dex_address_book, Some(&tracked_pairs))?;
        if !cli.simulation && !cli.dry_run {
            let (has_v2, has_v3) = live_execution_pool_kinds(&cli.dex_address_book, &cli.pair)?;
            if has_v2 && has_v3 && !cli.dex_router.is_empty() {
                return Err("mixed V2/V3 live execution requires --dex-router to be empty so V2 and V3 can use their own routers".into());
            }
            if cli.dex_router.is_empty() && has_v2 {
                let _ = DexSwapperConfig::default().router;
            } else if !cli.dex_router.is_empty() {
                Address::new(&cli.dex_router).map_err(|e| format!("invalid --dex-router: {e}"))?;
            }
            resolve_rpc_urls(cli).ok_or(
                "live DEX execution requires --eth-rpc-url or ETH_RPC_URL in --check-config",
            )?;
            WalletManager::from_env(&cli.wallet_key_env)
                .map_err(|e| format!("invalid live DEX wallet config: {e}"))?;
            if cli.use_flashbots || cli.require_private_dex {
                WalletManager::from_env(&cli.flashbots_auth_key_env).map_err(|e| {
                    format!(
                        "private DEX mode requires --flashbots-auth-key-env {}: {e}",
                        cli.flashbots_auth_key_env
                    )
                })?;
            }
        }
    }
    if !cli.simulation {
        let production = cli.production || peanut_internship_rust::config::env_production_enabled();
        BinanceConfig::from_env_for(production)
            .map_err(|e| format!("invalid Binance configuration: {e}"))?;
    }
    if cli.rebalance_enabled && !cli.rebalance_dry_run {
        if resolve_rpc_urls(cli).is_none() {
            return Err("live rebalance requires --eth-rpc-url or ETH_RPC_URL".into());
        }
        WalletManager::from_env(&cli.wallet_key_env)
            .map_err(|e| format!("invalid rebalance wallet config: {e}"))?;
        if cli.rebalance_cex_deposit_address.is_empty() {
            return Err("live rebalance requires --rebalance-cex-deposit-address or REBALANCE_CEX_DEPOSIT_ADDRESS".into());
        }
        Address::new(&cli.rebalance_cex_deposit_address)
            .map_err(|e| format!("invalid rebalance CEX deposit address: {e}"))?;
        if !cli.rebalance_cex_withdraw_address.is_empty() {
            Address::new(&cli.rebalance_cex_withdraw_address)
                .map_err(|e| format!("invalid rebalance CEX withdraw address: {e}"))?;
        }
    }
    Ok(())
}

fn effective_dex_fee_bps(
    configured_bps: u64,
    dex_address_book: &str,
) -> Result<Decimal, Box<dyn std::error::Error>> {
    if dex_address_book.is_empty() {
        return Ok(Decimal::from(configured_bps));
    }
    if configured_bps != 0 {
        return Err(format!(
            "--fee-dex-swap-bps must be 0 when --dex-address-book is set because live DEX quotes already include pool fees; configured {configured_bps}"
        )
        .into());
    }
    Ok(Decimal::ZERO)
}

fn load_dex_pool_fee_bps_map(
    dex_address_book: &str,
) -> Result<HashMap<String, Decimal>, Box<dyn std::error::Error>> {
    if dex_address_book.is_empty() {
        return Ok(HashMap::new());
    }
    let pairs = load_parsed_address_book(dex_address_book)?;
    Ok(dex_pool_fee_bps_map(&pairs))
}

fn validate_live_execution_config(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    if cli.simulation || cli.dry_run {
        return Ok(());
    }
    validate_flashbots_relay_chain(cli)?;
    if cli.max_concurrent_executions != 1 {
        return Err(
            "live execution requires --max-concurrent-executions 1 until inventory reservations and nonce manager are implemented"
                .into(),
        );
    }
    if cli.dex_address_book.is_empty() {
        return Err(
            "live execution requires --dex-address-book; otherwise DEX leg is not implemented"
                .into(),
        );
    }
    if cli.reconcile_db.is_empty() {
        return Err("live execution requires --reconcile-db so LEG2_TIMEOUT entries survive restart and can be audited".into());
    }
    validate_live_dex_pool_compatibility(&cli.dex_address_book, &cli.pair)?;
    validate_live_cex_pair_symbols(&cli.pair)?;
    validate_live_asset_symbol_uniqueness(&cli.dex_address_book, &cli.pair)?;
    validate_live_known_token_symbols(&cli.dex_address_book, &cli.pair, cli.dex_chain_id)?;
    if cli.rebalance_enabled && !cli.rebalance_dry_run {
        if resolve_rpc_urls(cli).is_none() {
            return Err("live rebalance requires --eth-rpc-url or ETH_RPC_URL".into());
        }
        WalletManager::from_env(&cli.wallet_key_env)
            .map_err(|e| format!("invalid rebalance wallet config: {e}"))?;
        if cli.rebalance_cex_deposit_address.is_empty() {
            return Err("live rebalance requires --rebalance-cex-deposit-address or REBALANCE_CEX_DEPOSIT_ADDRESS".into());
        }
        Address::new(&cli.rebalance_cex_deposit_address)
            .map_err(|e| format!("invalid rebalance CEX deposit address: {e}"))?;
        if !cli.rebalance_cex_withdraw_address.is_empty() {
            Address::new(&cli.rebalance_cex_withdraw_address)
                .map_err(|e| format!("invalid rebalance CEX withdraw address: {e}"))?;
        }
    }
    Ok(())
}

fn validate_flashbots_relay_chain(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    if cli.use_flashbots
        && cli.dex_chain_id != 1
        && cli.flashbots_relay_url.trim_end_matches('/') == DEFAULT_FLASHBOTS_RELAY_URL
    {
        return Err(format!(
            "default Flashbots relay {DEFAULT_FLASHBOTS_RELAY_URL} is Ethereum mainnet-only; set --use-flashbots=false for chain {} public DEX mode or provide a chain-specific private relay via --flashbots-relay-url",
            cli.dex_chain_id
        )
        .into());
    }
    Ok(())
}

fn validate_live_dex_pool_compatibility(
    path: &str,
    pairs: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let address_book = load_parsed_address_book(path)?;
    let selected = selected_pairs(&address_book, pairs)?;
    validate_selected_pool_compatibility(&selected)
}

fn validate_live_cex_pair_symbols(pairs: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    validate_cex_pair_symbols(pairs)
}

fn validate_live_known_token_symbols(
    path: &str,
    pairs: &[String],
    chain_id: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    if chain_id != ARBITRUM_CHAIN_ID {
        return Ok(());
    }
    let native_usdc = Address::new(ARBITRUM_NATIVE_USDC)?;
    let address_book = load_parsed_address_book(path)?;
    let selected = selected_pairs(&address_book, pairs)?;
    validate_arbitrum_known_token_symbols(&selected, &native_usdc)
}

fn validate_live_asset_symbol_uniqueness(
    path: &str,
    pairs: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let address_book = load_parsed_address_book(path)?;
    let selected = selected_pairs(&address_book, pairs)?;
    validate_asset_symbol_uniqueness(&selected)
}

fn live_execution_pool_kinds(
    path: &str,
    pairs: &[String],
) -> Result<(bool, bool), Box<dyn std::error::Error>> {
    let address_book = load_parsed_address_book(path)?;
    let selected = selected_pairs(&address_book, pairs)?;
    Ok(selected_pool_kinds(&selected))
}

fn build_rebalance_transfer_context(
    cli: &Cli,
    exchange: Arc<ExchangeClient>,
    nonce_manager: NonceManager,
) -> Result<Option<RebalanceTransferContext>, Box<dyn std::error::Error>> {
    if cli.rebalance_dry_run {
        return Ok(None);
    }
    let rpc_urls = resolve_rpc_urls(cli)
        .ok_or("live rebalance requires --eth-rpc-url or ETH_RPC_URL for Wallet→CEX transfers")?;
    let chain = ChainClient::new(rpc_urls, RPC_TIMEOUT_SECS, RPC_RETRIES)?;
    let wallet = WalletManager::from_env(&cli.wallet_key_env)?;
    let cex_withdraw_address = if cli.rebalance_cex_withdraw_address.is_empty() {
        wallet.address()
    } else {
        cli.rebalance_cex_withdraw_address.clone()
    };
    Address::new(&cex_withdraw_address)
        .map_err(|e| format!("invalid rebalance CEX withdraw address: {e}"))?;
    if cli.rebalance_cex_deposit_address.is_empty() {
        return Err("live rebalance requires --rebalance-cex-deposit-address or REBALANCE_CEX_DEPOSIT_ADDRESS".into());
    }
    let cex_deposit_address = Address::new(&cli.rebalance_cex_deposit_address)
        .map_err(|e| format!("invalid rebalance CEX deposit address: {e}"))?;
    let token_book = if cli.dex_address_book.is_empty() {
        HashMap::new()
    } else {
        load_rebalance_token_book(&cli.dex_address_book)?
    };
    validate_rebalance_weth_transfer_config(
        &parse_rebalance_csv_upper(&cli.rebalance_allowed_assets),
        &token_book,
        cli.rebalance_chain_id,
        &cex_withdraw_address,
        &wallet.address(),
    )
    .map_err(|e| format!("invalid WETH rebalance transfer config: {e}"))?;
    info!(
        cex_withdraw_address = %cex_withdraw_address,
        cex_withdraw_network = %cli.rebalance_cex_withdraw_network,
        cex_deposit_address = %cex_deposit_address,
        chain_id = cli.rebalance_chain_id,
        token_count = token_book.len(),
        "live auto-rebalance transfer context configured"
    );
    Ok(Some(RebalanceTransferContext {
        exchange,
        chain,
        wallet,
        nonce_manager,
        cex_withdraw_address,
        cex_withdraw_network: cli.rebalance_cex_withdraw_network.clone(),
        cex_deposit_address,
        token_book,
        chain_id: cli.rebalance_chain_id,
        max_gas_gwei: (cli.max_gas_gwei > 0).then_some(cli.max_gas_gwei),
        transfer_confirm_timeout: Duration::from_secs(
            cli.rebalance_transfer_confirm_timeout_secs.max(1),
        ),
        transfer_confirm_poll: Duration::from_secs(cli.rebalance_transfer_confirm_poll_secs.max(1)),
    }))
}

fn load_rebalance_token_book(
    path: &str,
) -> Result<HashMap<String, RebalanceTokenConfig>, Box<dyn std::error::Error>> {
    let pairs = load_parsed_address_book(path)?;
    Ok(rebalance_token_book(&pairs))
}

fn load_address_book(path: &str) -> Result<PairAddressBook, Box<dyn std::error::Error>> {
    let pairs = load_parsed_address_book(path)?;
    let mut book = PairAddressBook::new();
    for pair in pairs {
        book.insert(
            pair.pair,
            PairTokens {
                base: pair.base,
                base_decimals: pair.base_decimals,
                quote: pair.quote,
                quote_decimals: pair.quote_decimals,
                pool_kind: match pair.pool_kind {
                    AddressBookPoolKind::V2 => DexPoolKind::V2,
                    AddressBookPoolKind::V3 => DexPoolKind::V3,
                },
                v3_fee: pair.v3_fee,
                v3_path: pair.v3_path,
                v3_fees: pair.v3_fees,
                v3_quoter: pair.quoter,
            },
        );
    }
    Ok(book)
}

async fn discover_v3_pool_fee(
    chain: &ChainClient,
    pool: &Address,
    chain_id: u64,
) -> Result<u32, Box<dyn std::error::Error>> {
    let call = TransactionRequest::contract_call(
        pool.clone(),
        UNISWAP_V3_POOL_FEE_SELECTOR.to_vec(),
        chain_id,
    );
    let raw = chain
        .call(&call, BlockId::Latest)
        .await
        .map_err(|e| format!("V3 pool fee() call failed for {pool}: {e}"))?;
    if raw.len() < 32 {
        return Err(format!("V3 pool fee() returned {} bytes, expected 32", raw.len()).into());
    }
    let bytes: [u8; 32] = raw[..32]
        .try_into()
        .map_err(|_| "V3 pool fee() return conversion failed")?;
    Ok(U256::from_big_endian(&bytes).as_u64() as u32)
}

async fn load_address_book_with_fee_discovery(
    path: &str,
    chain: &ChainClient,
    chain_id: u64,
) -> Result<PairAddressBook, Box<dyn std::error::Error>> {
    let pairs = load_parsed_address_book(path)?;
    let mut book = PairAddressBook::new();
    for pair in pairs {
        let pool_kind = match pair.pool_kind {
            AddressBookPoolKind::V2 => DexPoolKind::V2,
            AddressBookPoolKind::V3 => DexPoolKind::V3,
        };
        let v3_fee =
            if pool_kind == DexPoolKind::V3 && pair.v3_fee.is_none() && pair.v3_path.is_none() {
                let pool = pair.pool.as_ref().ok_or_else(|| {
                    format!("pair '{}' missing pool for V3 fee discovery", pair.pair)
                })?;
                Some(discover_v3_pool_fee(chain, pool, chain_id).await?)
            } else {
                pair.v3_fee
            };
        book.insert(
            pair.pair,
            PairTokens {
                base: pair.base,
                base_decimals: pair.base_decimals,
                quote: pair.quote,
                quote_decimals: pair.quote_decimals,
                pool_kind,
                v3_fee,
                v3_path: pair.v3_path,
                v3_fees: pair.v3_fees,
                v3_quoter: pair.quoter,
            },
        );
    }
    Ok(book)
}

type LivePoolEntry = LivePoolConfig;

/// Reads the same `--dex-address-book` JSON but extracts only entries that
/// have a `pool` field set, producing the tuples expected by
/// [`LivePriceSource::new`]. Silently skips entries without a pool so the
/// file can serve both the swapper (which only needs token metadata) and
/// the live pricer (which additionally needs the pool address).
fn load_live_pool_book(
    path: &str,
    pair_filter: Option<&[String]>,
) -> Result<Vec<LivePoolEntry>, Box<dyn std::error::Error>> {
    use peanut_internship_rust::core::types::Token;
    let pairs = load_parsed_address_book(path)?;
    let mut out = Vec::new();
    for pair in pairs {
        if let Some(filter) = pair_filter
            && !filter.iter().any(|wanted| wanted == &pair.pair)
        {
            continue;
        }
        let Some(pool) = pair.pool else {
            continue;
        };
        let kind = match pair.pool_kind {
            AddressBookPoolKind::V2 => LivePoolKind::V2,
            AddressBookPoolKind::V3 => LivePoolKind::V3,
        };
        let quoter = match kind {
            LivePoolKind::V2 => None,
            LivePoolKind::V3 => {
                let quoter_address = pair
                    .quoter
                    .unwrap_or(Address::new(ARBITRUM_UNISWAP_V3_QUOTER_V2)?);
                let quoter_kind = match pair.quoter_type.to_ascii_lowercase().as_str() {
                    "v2" | "quoter_v2" => V3QuoterKind::QuoterV2,
                    other => {
                        return Err(format!(
                            "pair '{}' has unsupported quoter_type '{other}', expected 'quoter_v2'",
                            pair.pair
                        )
                        .into());
                    }
                };
                Some(V3QuoterConfig {
                    address: quoter_address,
                    kind: quoter_kind,
                })
            }
        };
        let base = Token {
            address: pair.base,
            symbol: pair.base_symbol,
            decimals: pair.base_decimals,
        };
        let quote = Token {
            address: pair.quote,
            symbol: pair.quote_symbol,
            decimals: pair.quote_decimals,
        };
        out.push(LivePoolConfig {
            pair_name: pair.pair,
            address: pool,
            base,
            quote,
            kind,
            quoter,
        });
    }
    Ok(out)
}

fn tracked_pairs_for_price_source(pairs: &[String]) -> Vec<String> {
    let mut tracked_pairs = pairs.to_vec();
    for pair in pairs {
        if pair.ends_with("/ETH") && !tracked_pairs.contains(&"ETH/USDC".to_string()) {
            tracked_pairs.push("ETH/USDC".to_string());
        }
    }
    tracked_pairs
}

/// Resolves `(rpc_url, wallet_address)` from CLI first, then env vars.
/// Returns `None` when either is empty — caller skips wallet sync.
fn resolve_wallet_config(cli: &Cli) -> Option<(Vec<String>, String)> {
    let rpc_urls = resolve_rpc_urls(cli).unwrap_or_default();
    let addr = resolve_wallet_address(cli).unwrap_or_default();
    if rpc_urls.is_empty() || addr.is_empty() {
        None
    } else {
        Some((rpc_urls, addr))
    }
}

fn resolve_wallet_address(cli: &Cli) -> Option<String> {
    let addr = if cli.wallet_address.is_empty() {
        std::env::var("WALLET_ADDRESS").unwrap_or_default()
    } else {
        cli.wallet_address.clone()
    };
    if addr.is_empty() { None } else { Some(addr) }
}

fn resolve_anvil_fork_url(cli: &Cli) -> Option<String> {
    let fork_url = if cli.anvil_fork_url.is_empty() {
        std::env::var("ANVIL_FORK_URL").unwrap_or_default()
    } else {
        cli.anvil_fork_url.clone()
    };
    if fork_url.is_empty() {
        None
    } else {
        Some(fork_url)
    }
}

fn resolve_rpc_urls(cli: &Cli) -> Option<Vec<String>> {
    let rpc_raw = if cli.eth_rpc_url.is_empty() {
        std::env::var("ETH_RPC_URL").unwrap_or_default()
    } else {
        cli.eth_rpc_url.clone()
    };
    let rpc_urls = parse_rpc_urls(&rpc_raw);
    if rpc_urls.is_empty() {
        None
    } else {
        Some(rpc_urls)
    }
}

/// Resolves a WebSocket URL for the DEX block feed.
/// Checks `ETH_WS_URL` env var first, then derives from the first RPC URL:
///   https://arb1.arbitrum.io/rpc → wss://arb1.arbitrum.io/ws
fn resolve_ws_url(rpc_urls: &[String]) -> Option<String> {
    if let Ok(ws) = std::env::var("ETH_WS_URL")
        && !ws.is_empty()
    {
        return Some(ws);
    }
    let rpc = rpc_urls.first()?;
    Some(
        rpc.replace("https://", "wss://")
            .replace("http://", "ws://")
            .replace("/rpc", "/ws"),
    )
}

fn parse_rpc_urls(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

async fn log_rpc_health(label: &str, client: &ChainClient) {
    let health = client.health_check().await;
    let healthy = health.iter().filter(|h| h.healthy).count();
    info!(
        component = label,
        healthy,
        total = health.len(),
        "RPC health check complete"
    );
    for endpoint in health {
        if endpoint.healthy {
            info!(
                component = label,
                url = %endpoint.url,
                block = ?endpoint.block_number,
                "RPC endpoint healthy"
            );
        } else {
            warn!(
                component = label,
                url = %endpoint.url,
                error = ?endpoint.error,
                "RPC endpoint unhealthy"
            );
        }
    }
}

/// Processes reconcile worker outcomes: triggers unwind on reverted entries
/// and emits alerts for reverts and expirations.
async fn handle_reconcile_outcomes(
    outcomes: Vec<(PendingReconcile, TickOutcome)>,
    alert_sink: &Arc<dyn AlertSink>,
    legs: &Arc<dyn LegExecutor>,
) {
    for (entry, outcome) in outcomes {
        match outcome {
            TickOutcome::ReceiptSuccess => {
                info!(
                    signal = %entry.signal_id,
                    pair = %entry.pair,
                    "reconcile: resolved successfully"
                );
            }
            TickOutcome::ReceiptReverted => {
                let size = match Decimal::from_str_exact(&entry.leg1_fill_size) {
                    Ok(size) if size > Decimal::ZERO => size,
                    Ok(size) => {
                        error!(
                            signal = %entry.signal_id,
                            pair = %entry.pair,
                            size = %size,
                            "reconcile: invalid non-positive leg1 fill size; cannot unwind"
                        );
                        emit_best_effort(
                            alert_sink,
                            &AlertEvent::ExecutionFailed {
                                signal_id: entry.signal_id,
                                pair: entry.pair,
                                reason: format!(
                                    "leg2 reverted on-chain but unwind size was non-positive ({size})"
                                ),
                            },
                        )
                        .await;
                        continue;
                    }
                    Err(e) => {
                        error!(
                            signal = %entry.signal_id,
                            pair = %entry.pair,
                            raw_size = %entry.leg1_fill_size,
                            error = %e,
                            "reconcile: failed to parse leg1 fill size; cannot unwind"
                        );
                        emit_best_effort(
                            alert_sink,
                            &AlertEvent::ExecutionFailed {
                                signal_id: entry.signal_id,
                                pair: entry.pair,
                                reason: format!(
                                    "leg2 reverted on-chain but unwind size could not be parsed: {e}"
                                ),
                            },
                        )
                        .await;
                        continue;
                    }
                };
                if let Err(e) = legs
                    .unwind_position(&entry.pair, &entry.leg1_venue, entry.direction, size)
                    .await
                {
                    error!(
                        signal = %entry.signal_id,
                        pair = %entry.pair,
                        error = %e,
                        "reconcile: UNWIND FAILED — position left open"
                    );
                }
                emit_best_effort(
                    alert_sink,
                    &AlertEvent::ExecutionFailed {
                        signal_id: entry.signal_id,
                        pair: entry.pair,
                        reason: "leg2 reverted on-chain (reconcile worker)".into(),
                    },
                )
                .await;
            }
            TickOutcome::Expired => {
                emit_best_effort(
                    alert_sink,
                    &AlertEvent::Leg2Timeout {
                        signal_id: entry.signal_id,
                        tx_hash: Some(entry.tx_hash),
                        pair: entry.pair,
                    },
                )
                .await;
            }
            TickOutcome::StillPending => {}
        }
    }
}

fn execution_to_arb_record(
    ctx: &peanut_internship_rust::executor::engine::ExecutionContext,
) -> ArbRecord {
    let signal = &ctx.signal;
    let quote = split_pair(&signal.pair)
        .map(|(_, quote)| quote.to_string())
        .unwrap_or_else(|error| {
            warn!(pair = %signal.pair, error = %error, "malformed pair in arb record; leaving fee_asset empty");
            String::new()
        });

    // Map leg1/leg2 -> buy/sell. This depends on BOTH the direction (which
    // venue is the buy side) AND leg1_venue (which side was executed first).
    // In DEX-first flow leg1 == DEX; in CEX-first flow leg1 == CEX.
    // Previously this mapping hard-coded leg1=buy, which silently inverted
    // every DEX-first DONE row in the PnL ledger.
    use peanut_internship_rust::strategy::signal::Direction;
    let buy_venue = match signal.direction {
        Direction::BuyCexSellDex => Venue::Binance,
        Direction::BuyDexSellCex => Venue::Wallet,
    };
    let sell_venue = match signal.direction {
        Direction::BuyCexSellDex => Venue::Wallet,
        Direction::BuyDexSellCex => Venue::Binance,
    };
    let buy_is_leg1 = matches!(
        (signal.direction, ctx.leg1_venue),
        (Direction::BuyCexSellDex, "cex") | (Direction::BuyDexSellCex, "dex")
    );
    let (
        buy_size,
        buy_price,
        buy_fee,
        buy_fee_asset,
        sell_size,
        sell_price,
        sell_fee,
        sell_fee_asset,
    ) = if buy_is_leg1 {
        (
            ctx.leg1_fill_size,
            ctx.leg1_fill_price,
            ctx.leg1_fee,
            ctx.leg1_fee_asset.clone().unwrap_or_else(|| quote.clone()),
            ctx.leg2_fill_size,
            ctx.leg2_fill_price,
            ctx.leg2_fee,
            ctx.leg2_fee_asset.clone().unwrap_or_else(|| quote.clone()),
        )
    } else {
        (
            ctx.leg2_fill_size,
            ctx.leg2_fill_price,
            ctx.leg2_fee,
            ctx.leg2_fee_asset.clone().unwrap_or_else(|| quote.clone()),
            ctx.leg1_fill_size,
            ctx.leg1_fill_price,
            ctx.leg1_fee,
            ctx.leg1_fee_asset.clone().unwrap_or_else(|| quote.clone()),
        )
    };

    let started = DateTime::<chrono::Utc>::from_timestamp(signal.timestamp.timestamp(), 0)
        .unwrap_or_else(chrono::Utc::now);
    let finished = ctx
        .finished_at
        .map(|_| chrono::Utc::now())
        .unwrap_or(started);

    // Missing leg fills would only happen for `Done` contexts if upstream
    // accounting regressed — log loudly so the PnL ledger doesn't silently
    // accumulate zero-price / zero-size rows.
    if ctx.leg1_fill_size.is_none() || ctx.leg1_fill_price.is_none() {
        warn!(signal = %signal.signal_id, "arb record missing leg1 fill data");
    }
    if ctx.leg2_fill_size.is_none() || ctx.leg2_fill_price.is_none() {
        warn!(signal = %signal.signal_id, "arb record missing leg2 fill data");
    }

    let expected_quote_value = signal.size * signal.cex_price;
    let quote_usd_price = if expected_quote_value > Decimal::ZERO {
        signal.notional_usd / expected_quote_value
    } else {
        warn!(
            signal = %signal.signal_id,
            pair = %signal.pair,
            size = %signal.size,
            cex_price = %signal.cex_price,
            notional_usd = %signal.notional_usd,
            "arb record could not derive quote_usd_price; recording leg prices without USD conversion"
        );
        Decimal::ONE
    };

    let buy_leg = TradeLeg {
        id: format!("{}_buy", signal.signal_id),
        timestamp: started,
        venue: buy_venue,
        symbol: signal.pair.clone(),
        side: "buy".into(),
        amount: buy_size.unwrap_or(Decimal::ZERO),
        price: buy_price.unwrap_or(Decimal::ZERO) * quote_usd_price,
        fee: buy_fee,
        fee_asset: buy_fee_asset,
    };
    let sell_leg = TradeLeg {
        id: format!("{}_sell", signal.signal_id),
        timestamp: finished,
        venue: sell_venue,
        symbol: signal.pair.clone(),
        side: "sell".into(),
        amount: sell_size.unwrap_or(Decimal::ZERO),
        price: sell_price.unwrap_or(Decimal::ZERO) * quote_usd_price,
        fee: sell_fee,
        fee_asset: sell_fee_asset,
    };

    let actual_gas_cost_usd = ctx.actual_onchain_gas_fee_usd.unwrap_or_else(|| {
        warn!(
            signal = %signal.signal_id,
            "arb record missing actual on-chain gas fee; recording gas_cost_usd as zero"
        );
        Decimal::ZERO
    });

    ArbRecord {
        id: signal.signal_id.clone(),
        timestamp: started,
        buy_leg,
        sell_leg,
        gas_cost_usd: actual_gas_cost_usd,
        expected_gross_pnl_usd: signal.expected_gross_pnl,
        expected_fees_usd: signal.expected_fees,
        expected_net_pnl_usd: signal.expected_net_pnl,
        actual_gross_pnl_usd: ctx.actual_gross_pnl_usd.unwrap_or_else(|| {
            warn!(signal = %signal.signal_id, "arb record missing actual gross PnL");
            Decimal::ZERO
        }),
        actual_fees_usd: ctx.actual_fees_usd.unwrap_or_else(|| {
            warn!(signal = %signal.signal_id, "arb record missing actual fees");
            Decimal::ZERO
        }),
        actual_cex_fee_usd: ctx.actual_cex_fee_usd.unwrap_or_else(|| {
            warn!(signal = %signal.signal_id, "arb record missing actual CEX fee");
            Decimal::ZERO
        }),
        actual_onchain_gas_fee_usd: actual_gas_cost_usd,
        actual_net_pnl_usd: ctx.actual_net_pnl.unwrap_or_else(|| {
            warn!(signal = %signal.signal_id, "arb record missing actual net PnL");
            Decimal::ZERO
        }),
        onchain_gas_used: ctx.onchain_gas_used.map(|v| v.to_string()),
        onchain_gas_fee_wei: ctx.onchain_gas_fee_wei.map(|v| v.to_string()),
    }
}

/// Parses a `--seed-inventory` spec of shape
/// `venue:ASSET=AMOUNT[,ASSET=AMOUNT]...` into the tuple expected by
/// [`InventoryTracker::update_from_wallet`]. Venue names are matched
/// case-insensitively against `binance` and `wallet`; asset symbols are
/// upper-cased so callers can write either `usdt` or `USDT`.
fn parse_seed_spec(
    spec: &str,
) -> Result<(Venue, std::collections::HashMap<String, Decimal>), String> {
    let (venue_raw, balances_raw) = spec
        .split_once(':')
        .ok_or_else(|| "expected 'venue:ASSET=AMOUNT[,ASSET=AMOUNT]...'".to_string())?;
    let venue = match venue_raw.trim().to_ascii_lowercase().as_str() {
        "binance" | "cex" => Venue::Binance,
        "wallet" | "onchain" | "dex" => Venue::Wallet,
        other => {
            return Err(format!(
                "unknown venue '{other}' (expected binance or wallet)"
            ));
        }
    };
    let mut balances = std::collections::HashMap::new();
    for entry in balances_raw.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (asset, amount) = entry
            .split_once('=')
            .ok_or_else(|| format!("expected ASSET=AMOUNT, got '{entry}'"))?;
        let asset = asset.trim().to_ascii_uppercase();
        if asset.is_empty() {
            return Err(format!("empty asset symbol in '{entry}'"));
        }
        let amount = Decimal::from_str_exact(amount.trim())
            .map_err(|e| format!("invalid amount for {asset}: {e}"))?;
        balances.insert(asset, amount);
    }
    if balances.is_empty() {
        return Err("no asset=amount entries".into());
    }
    Ok((venue, balances))
}

fn seed_inventory_balances(
    tracker: &mut InventoryTracker,
    venue: Venue,
    balances: HashMap<String, Decimal>,
) {
    if venue.is_cex() {
        let normalized = balances
            .into_iter()
            .map(|(asset, amount)| {
                (
                    asset,
                    NormalizedBalance {
                        free: amount,
                        locked: Decimal::ZERO,
                        total: amount,
                    },
                )
            })
            .collect();
        tracker.update_from_cex(venue, normalized);
    } else {
        tracker.update_from_wallet(venue, balances);
    }
}

#[cfg(test)]
mod seed_tests {
    use super::*;

    struct FixedOrderBookSource;

    #[async_trait]
    impl CexOrderBookSource for FixedOrderBookSource {
        async fn fetch_order_book(
            &self,
            pair: &str,
            _limit: u32,
        ) -> peanut_internship_rust::strategy::StrategyResult<OrderBookSnapshot> {
            Ok(OrderBookSnapshot {
                symbol: pair.to_string(),
                timestamp: 0,
                bids: vec![(Decimal::ONE, Decimal::ONE)],
                asks: vec![(Decimal::ONE, Decimal::ONE)],
                best_bid: Some((Decimal::ONE, Decimal::ONE)),
                best_ask: Some((Decimal::ONE, Decimal::ONE)),
                mid_price: Some(Decimal::ONE),
                spread_bps: Some(Decimal::ZERO),
            })
        }
    }

    #[test]
    fn parses_binance_single_asset() {
        let (venue, map) = parse_seed_spec("binance:USDT=10000").unwrap();
        assert_eq!(venue, Venue::Binance);
        assert_eq!(map.get(USDT_SYMBOL), Some(&Decimal::from(10000)));
    }

    #[test]
    fn parses_wallet_multi_with_case_insensitive_venue_and_asset() {
        let (venue, map) = parse_seed_spec("Wallet: eth=2.5 , usdc=100").unwrap();
        assert_eq!(venue, Venue::Wallet);
        assert_eq!(map.get(ETH_SYMBOL), Some(&Decimal::new(25, 1)));
        assert_eq!(map.get(USDC_SYMBOL), Some(&Decimal::from(100)));
    }

    #[test]
    fn rejects_missing_colon() {
        assert!(parse_seed_spec("binance USDT=10").is_err());
    }

    #[test]
    fn rejects_unknown_venue() {
        assert!(parse_seed_spec("kraken:USDT=10").is_err());
    }

    #[test]
    fn rejects_empty_body() {
        assert!(parse_seed_spec("binance:").is_err());
    }

    #[test]
    fn rejects_invalid_amount() {
        assert!(parse_seed_spec("binance:USDT=notanumber").is_err());
    }

    #[test]
    fn seed_inventory_canonicalizes_cex_eth_to_weth_on_arbitrum() {
        let mut tracker =
            InventoryTracker::new(vec![Venue::Binance, Venue::Wallet], ARBITRUM_CHAIN_ID);
        let (_, binance_balances) = parse_seed_spec("binance:ETH=0.05").unwrap();
        seed_inventory_balances(&mut tracker, Venue::Binance, binance_balances);
        assert_eq!(
            tracker.get_total(Venue::Binance, WETH_SYMBOL),
            Some(Decimal::new(5, 2))
        );
        assert_eq!(tracker.get_total(Venue::Binance, ETH_SYMBOL), None);
    }

    #[test]
    fn seed_inventory_keeps_wallet_eth_native() {
        let mut tracker =
            InventoryTracker::new(vec![Venue::Binance, Venue::Wallet], ARBITRUM_CHAIN_ID);
        let (_, wallet_balances) = parse_seed_spec("wallet:ETH=0.05").unwrap();
        seed_inventory_balances(&mut tracker, Venue::Wallet, wallet_balances);
        assert_eq!(
            tracker.get_total(Venue::Wallet, ETH_SYMBOL),
            Some(Decimal::new(5, 2))
        );
        assert_eq!(tracker.get_total(Venue::Wallet, WETH_SYMBOL), None);
    }

    #[tokio::test]
    async fn rebalance_max_step_usd_rejects_oversized_dry_run_step() {
        let price_source = Arc::new(AnyPriceSource::Stub(StubPriceSource::new_with_order_books(
            Arc::new(FixedOrderBookSource),
        )));
        let config = RebalanceLoopConfig {
            dry_run: true,
            interval: Duration::from_secs(60),
            threshold_pct: 10.0,
            quote_asset: USDC_SYMBOL.to_string(),
            max_slippage_bps: Decimal::from(50),
            allowed_assets: vec![LINK_SYMBOL.to_string()],
            allowed_venues: vec!["wallet".to_string(), "binance".to_string()],
            max_step_usd: Decimal::from(5),
            min_fill_pct: 1.0,
            pause_trading: false,
            balance_verify_tolerance_pct: Decimal::ONE,
            journal_path: String::new(),
        };
        let step =
            RebalanceStep::Withdraw(peanut_internship_rust::inventory::types::WithdrawStep {
                from_venue: Venue::Wallet,
                to_venue: Venue::Binance,
                asset: LINK_SYMBOL.to_string(),
                amount: Decimal::new(75, 1),
                fee: Decimal::ZERO,
            });
        let err = validate_rebalance_step(&step, &config, &price_source)
            .await
            .unwrap_err();
        assert!(err.contains("exceeds max"));
    }

    #[test]
    fn parses_comma_separated_rpc_urls() {
        assert_eq!(
            parse_rpc_urls("http://a, https://b ,,http://c "),
            vec![
                "http://a".to_string(),
                "https://b".to_string(),
                "http://c".to_string()
            ]
        );
    }

    #[test]
    fn effective_dex_fee_requires_explicit_zero_with_live_address_book_quotes() {
        assert_eq!(effective_dex_fee_bps(30, "").unwrap(), Decimal::from(30));
        assert_eq!(
            effective_dex_fee_bps(0, "configs/address_book_arbitrum.json").unwrap(),
            Decimal::ZERO
        );
        assert!(effective_dex_fee_bps(30, "configs/address_book_arbitrum.json").is_err());
    }

    #[test]
    fn tracked_pairs_adds_eth_usdc_conversion_once() {
        assert_eq!(
            tracked_pairs_for_price_source(&["LINK/ETH".to_string()]),
            vec!["LINK/ETH".to_string(), "ETH/USDC".to_string()]
        );
        assert_eq!(
            tracked_pairs_for_price_source(&["LINK/ETH".to_string(), "ETH/USDC".to_string()]),
            vec!["LINK/ETH".to_string(), "ETH/USDC".to_string()]
        );
    }

    #[test]
    fn rebalance_csv_parsers_normalize_values() {
        assert_eq!(
            parse_rebalance_csv_upper(" link, ETH ,,"),
            vec![LINK_SYMBOL.to_string(), ETH_SYMBOL.to_string()]
        );
        assert_eq!(
            parse_rebalance_csv_lower(" Binance, WALLET ,,"),
            vec!["binance".to_string(), "wallet".to_string()]
        );
    }

    #[test]
    fn rebalance_weth_helpers_alias_to_eth_where_required() {
        assert_eq!(rebalance_cex_asset(WETH_SYMBOL), ETH_SYMBOL);
        assert_eq!(rebalance_cex_asset("link"), LINK_SYMBOL);
        assert_eq!(rebalance_pricing_asset(WETH_SYMBOL), ETH_SYMBOL);
        assert_eq!(rebalance_pricing_asset("link"), LINK_SYMBOL);
    }

    #[test]
    fn rebalance_asset_allowlist_accepts_weth_when_allowed() {
        let step =
            RebalanceStep::Withdraw(peanut_internship_rust::inventory::types::WithdrawStep {
                from_venue: Venue::Binance,
                to_venue: Venue::Wallet,
                asset: WETH_SYMBOL.to_string(),
                amount: Decimal::ONE,
                fee: Decimal::ZERO,
            });
        validate_rebalance_assets(
            &step,
            &[
                LINK_SYMBOL.to_string(),
                ETH_SYMBOL.to_string(),
                WETH_SYMBOL.to_string(),
            ],
        )
        .unwrap();
    }

    #[test]
    fn rebalance_native_balance_requirement_includes_gas_reserve() {
        let required = native_balance_required_for_value_and_gas(
            U256::from(1_000u64),
            U256::from(10u64),
            REBALANCE_WETH_WRAP_GAS_LIMIT,
        );
        assert_eq!(
            required,
            U256::from(1_000u64) + U256::from(10u64) * U256::from(REBALANCE_WETH_WRAP_GAS_LIMIT)
        );
    }

    #[test]
    fn rebalance_weth_transfer_config_accepts_arbitrum_weth() {
        let wallet = "0x1111111111111111111111111111111111111111";
        let mut token_book = HashMap::new();
        token_book.insert(
            ETH_SYMBOL.to_string(),
            AddressBookTokenConfig {
                address: Address::new(ARBITRUM_WETH_ADDRESS).unwrap(),
                decimals: ETH_DECIMALS,
            },
        );
        validate_rebalance_weth_transfer_config(
            &[WETH_SYMBOL.to_string()],
            &token_book,
            ARBITRUM_CHAIN_ID,
            wallet,
            wallet,
        )
        .unwrap();
    }

    #[test]
    fn rebalance_weth_transfer_config_rejects_non_signer_withdraw_address() {
        let mut token_book = HashMap::new();
        token_book.insert(
            WETH_SYMBOL.to_string(),
            AddressBookTokenConfig {
                address: Address::new(ARBITRUM_WETH_ADDRESS).unwrap(),
                decimals: ETH_DECIMALS,
            },
        );
        let err = validate_rebalance_weth_transfer_config(
            &[WETH_SYMBOL.to_string()],
            &token_book,
            ARBITRUM_CHAIN_ID,
            "0x1111111111111111111111111111111111111111",
            "0x2222222222222222222222222222222222222222",
        )
        .unwrap_err();
        assert!(err.contains("signer wallet"));
    }

    #[test]
    fn rebalance_weth_transfer_config_rejects_wrong_weth_address() {
        let wallet = "0x1111111111111111111111111111111111111111";
        let mut token_book = HashMap::new();
        token_book.insert(
            WETH_SYMBOL.to_string(),
            AddressBookTokenConfig {
                address: Address::new("0x3333333333333333333333333333333333333333").unwrap(),
                decimals: ETH_DECIMALS,
            },
        );
        let err = validate_rebalance_weth_transfer_config(
            &[WETH_SYMBOL.to_string()],
            &token_book,
            ARBITRUM_CHAIN_ID,
            wallet,
            wallet,
        )
        .unwrap_err();
        assert!(err.contains("does not match Arbitrum WETH"));
    }

    #[test]
    fn rebalance_asset_allowlist_rejects_unlisted_quote_asset() {
        let step = RebalanceStep::Trade(peanut_internship_rust::inventory::types::TradeStep {
            venue: Venue::Binance,
            symbol: "LINKUSDT".to_string(),
            side: "SELL".to_string(),
            base_asset: "LINK".to_string(),
            quote_asset: USDT_SYMBOL.to_string(),
            amount: Decimal::ONE,
            max_slippage_bps: Decimal::from(50),
        });
        let err =
            validate_rebalance_assets(&step, &[LINK_SYMBOL.to_string(), ETH_SYMBOL.to_string()])
                .unwrap_err();
        assert!(err.contains(USDT_SYMBOL));
    }

    #[test]
    fn rebalance_venue_allowlist_rejects_unlisted_venue() {
        let step =
            RebalanceStep::Withdraw(peanut_internship_rust::inventory::types::WithdrawStep {
                from_venue: Venue::Bybit,
                to_venue: Venue::Wallet,
                asset: "LINK".to_string(),
                amount: Decimal::ONE,
                fee: Decimal::ZERO,
            });
        let err = validate_rebalance_venues(&step, &["binance".to_string(), "wallet".to_string()])
            .unwrap_err();
        assert!(err.contains("bybit"));
    }

    #[test]
    fn dex_pool_fee_map_converts_v3_fee_tier_to_bps() {
        let file = write_address_book(
            r#"{
                "LINK/ETH": {
                    "base": "0x0000000000000000000000000000000000000001",
                    "base_decimals": 18,
                    "quote": "0x0000000000000000000000000000000000000002",
                    "quote_decimals": 18,
                    "pool": "0x0000000000000000000000000000000000000003",
                    "pool_type": "v3",
                    "fee": 3000
                },
                "ETH/USDC": {
                    "base": "0x0000000000000000000000000000000000000001",
                    "base_decimals": 18,
                    "quote": "0x0000000000000000000000000000000000000002",
                    "quote_decimals": 6,
                    "pool": "0x0000000000000000000000000000000000000004",
                    "pool_type": "v2"
                }
            }"#,
        );
        let fees = load_dex_pool_fee_bps_map(file.path().to_str().unwrap()).unwrap();
        assert_eq!(fees.get("LINK/ETH"), Some(&Decimal::from(30)));
        assert_eq!(fees.get("ETH/USDC"), Some(&Decimal::from(30)));
    }

    fn sample_capital_config(
        withdraw_enable: bool,
        withdraw_min: Decimal,
        withdraw_fee: Decimal,
    ) -> Vec<peanut_internship_rust::exchange::CapitalCoinConfig> {
        vec![peanut_internship_rust::exchange::CapitalCoinConfig {
            coin: LINK_SYMBOL.to_string(),
            name: Some("ChainLink".to_string()),
            networks: vec![peanut_internship_rust::exchange::CapitalNetworkConfig {
                network: DEFAULT_REBALANCE_CEX_WITHDRAW_NETWORK.to_string(),
                name: Some("Arbitrum One".to_string()),
                withdraw_enable,
                deposit_enable: true,
                withdraw_fee,
                withdraw_min,
            }],
        }]
    }

    fn sample_withdraw_step(
        fee: Decimal,
    ) -> peanut_internship_rust::inventory::types::WithdrawStep {
        peanut_internship_rust::inventory::types::WithdrawStep {
            from_venue: Venue::Binance,
            to_venue: Venue::Wallet,
            asset: "LINK".to_string(),
            amount: Decimal::from(5),
            fee,
        }
    }

    #[test]
    fn binance_withdrawal_capability_accepts_enabled_network() {
        let configs =
            sample_capital_config(true, Decimal::ONE, Decimal::from_str_exact("0.1").unwrap());
        let withdraw = sample_withdraw_step(Decimal::from_str_exact("0.1").unwrap());
        assert!(
            validate_binance_withdrawal_capability(
                &configs,
                &withdraw,
                Decimal::from_str_exact("4.9").unwrap(),
                DEFAULT_REBALANCE_CEX_WITHDRAW_NETWORK
            )
            .is_ok()
        );
    }

    #[test]
    fn binance_withdrawal_capability_rejects_wrong_network() {
        let configs = sample_capital_config(true, Decimal::ONE, Decimal::ZERO);
        let withdraw = sample_withdraw_step(Decimal::ZERO);
        let err = validate_binance_withdrawal_capability(
            &configs,
            &withdraw,
            Decimal::from(5),
            ETH_SYMBOL,
        )
        .unwrap_err();
        assert!(err.contains("does not support withdrawal network"));
    }

    #[test]
    fn binance_withdrawal_capability_rejects_disabled_network() {
        let configs = sample_capital_config(false, Decimal::ONE, Decimal::ZERO);
        let withdraw = sample_withdraw_step(Decimal::ZERO);
        let err = validate_binance_withdrawal_capability(
            &configs,
            &withdraw,
            Decimal::from(5),
            DEFAULT_REBALANCE_CEX_WITHDRAW_NETWORK,
        )
        .unwrap_err();
        assert!(err.contains("withdrawal disabled"));
    }

    #[test]
    fn binance_withdrawal_capability_rejects_minimum_and_fee() {
        let configs = sample_capital_config(true, Decimal::from(10), Decimal::ONE);
        let withdraw = sample_withdraw_step(Decimal::ZERO);
        let min_err = validate_binance_withdrawal_capability(
            &configs,
            &withdraw,
            Decimal::from(5),
            DEFAULT_REBALANCE_CEX_WITHDRAW_NETWORK,
        )
        .unwrap_err();
        assert!(min_err.contains("below minimum"));

        let fee_err = validate_binance_withdrawal_capability(
            &configs,
            &withdraw,
            Decimal::from(10),
            DEFAULT_REBALANCE_CEX_WITHDRAW_NETWORK,
        )
        .unwrap_err();
        assert!(fee_err.contains("below Binance fee"));
    }

    fn write_address_book(body: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut file, body.as_bytes()).unwrap();
        file
    }

    #[test]
    fn live_dex_validation_accepts_v2_pool() {
        let file = write_address_book(
            r#"{
                "ETH/USDC": {
                    "base": "0x0000000000000000000000000000000000000001",
                    "base_decimals": 18,
                    "quote": "0x0000000000000000000000000000000000000002",
                    "quote_decimals": 6,
                    "pool": "0x0000000000000000000000000000000000000003",
                    "pool_type": "v2"
                }
            }"#,
        );
        assert!(
            validate_live_dex_pool_compatibility(
                file.path().to_str().unwrap(),
                &["ETH/USDC".to_string()]
            )
            .is_ok()
        );
    }

    #[test]
    fn live_dex_validation_accepts_v3_pool_with_fee() {
        let file = write_address_book(
            r#"{
                "ETH/USDC": {
                    "base": "0x0000000000000000000000000000000000000001",
                    "base_decimals": 18,
                    "quote": "0x0000000000000000000000000000000000000002",
                    "quote_decimals": 6,
                    "pool": "0x0000000000000000000000000000000000000003",
                    "pool_type": "v3",
                    "fee": 500
                }
            }"#,
        );
        assert!(
            validate_live_dex_pool_compatibility(
                file.path().to_str().unwrap(),
                &["ETH/USDC".to_string()]
            )
            .is_ok()
        );
    }

    #[test]
    fn live_dex_validation_accepts_v3_pool_without_fee_for_discovery() {
        let file = write_address_book(
            r#"{
                "ETH/USDC": {
                    "base": "0x0000000000000000000000000000000000000001",
                    "base_decimals": 18,
                    "quote": "0x0000000000000000000000000000000000000002",
                    "quote_decimals": 6,
                    "pool": "0x0000000000000000000000000000000000000003",
                    "pool_type": "v3"
                }
            }"#,
        );
        assert!(
            validate_live_dex_pool_compatibility(
                file.path().to_str().unwrap(),
                &["ETH/USDC".to_string()],
            )
            .is_ok()
        );
    }

    #[test]
    fn load_live_pool_book_filters_to_tracked_pairs() {
        let file = write_address_book(
            r#"{
                "LINK/ETH": {
                    "base": "0x0000000000000000000000000000000000000001",
                    "base_decimals": 18,
                    "quote": "0x0000000000000000000000000000000000000002",
                    "quote_decimals": 18,
                    "pool": "0x0000000000000000000000000000000000000003",
                    "pool_type": "v3",
                    "fee": 3000
                },
                "GMX/ETH": {
                    "base": "0x0000000000000000000000000000000000000004",
                    "base_decimals": 18,
                    "quote": "0x0000000000000000000000000000000000000002",
                    "quote_decimals": 18,
                    "pool": "0x0000000000000000000000000000000000000005",
                    "pool_type": "v3",
                    "fee": 10000
                }
            }"#,
        );
        let tracked = vec!["LINK/ETH".to_string()];
        let pools = load_live_pool_book(file.path().to_str().unwrap(), Some(&tracked)).unwrap();
        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].pair_name, "LINK/ETH");
    }

    #[test]
    fn live_dex_validation_accepts_mixed_v2_v3_pools() {
        let file = write_address_book(
            r#"{
                "ETH/USDC": {
                    "base": "0x0000000000000000000000000000000000000001",
                    "base_decimals": 18,
                    "quote": "0x0000000000000000000000000000000000000002",
                    "quote_decimals": 6,
                    "pool": "0x0000000000000000000000000000000000000003",
                    "pool_type": "v2"
                },
                "WBTC/USDC": {
                    "base": "0x0000000000000000000000000000000000000004",
                    "base_decimals": 8,
                    "quote": "0x0000000000000000000000000000000000000002",
                    "quote_decimals": 6,
                    "pool": "0x0000000000000000000000000000000000000005",
                    "pool_type": "v3",
                    "fee": 500
                }
            }"#,
        );
        assert!(
            validate_live_dex_pool_compatibility(
                file.path().to_str().unwrap(),
                &["ETH/USDC".to_string(), "WBTC/USDC".to_string()],
            )
            .is_ok()
        );
    }

    #[test]
    fn live_dex_validation_rejects_v3_route_fee_mismatch() {
        let file = write_address_book(
            r#"{
                "ETH/USDC": {
                    "base": "0x0000000000000000000000000000000000000001",
                    "base_decimals": 18,
                    "quote": "0x0000000000000000000000000000000000000002",
                    "quote_decimals": 6,
                    "pool": "0x0000000000000000000000000000000000000003",
                    "pool_type": "v3",
                    "v3_path": [
                        "0x0000000000000000000000000000000000000001",
                        "0x0000000000000000000000000000000000000004",
                        "0x0000000000000000000000000000000000000002"
                    ],
                    "v3_fees": [500]
                }
            }"#,
        );
        let err = validate_live_dex_pool_compatibility(
            file.path().to_str().unwrap(),
            &["ETH/USDC".to_string()],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("v3_fees length"));
    }

    #[test]
    fn live_dex_validation_rejects_v3_route_endpoint_mismatch() {
        let file = write_address_book(
            r#"{
                "ETH/USDC": {
                    "base": "0x0000000000000000000000000000000000000001",
                    "base_decimals": 18,
                    "quote": "0x0000000000000000000000000000000000000002",
                    "quote_decimals": 6,
                    "pool": "0x0000000000000000000000000000000000000003",
                    "pool_type": "v3",
                    "v3_path": [
                        "0x0000000000000000000000000000000000000001",
                        "0x0000000000000000000000000000000000000004",
                        "0x0000000000000000000000000000000000000005"
                    ],
                    "v3_fees": [500, 3000]
                }
            }"#,
        );
        let err = validate_live_dex_pool_compatibility(
            file.path().to_str().unwrap(),
            &["ETH/USDC".to_string()],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("v3_path endpoints"));
    }

    #[test]
    fn live_asset_validation_rejects_symbol_address_collision() {
        let file = write_address_book(
            r#"{
                "ETH/USDC": {
                    "base": "0x0000000000000000000000000000000000000001",
                    "base_decimals": 18,
                    "quote": "0x0000000000000000000000000000000000000002",
                    "quote_decimals": 6,
                    "pool": "0x0000000000000000000000000000000000000003",
                    "pool_type": "v3",
                    "fee": 500
                },
                "ARB/USDC": {
                    "base": "0x0000000000000000000000000000000000000004",
                    "base_decimals": 18,
                    "quote": "0x0000000000000000000000000000000000000005",
                    "quote_decimals": 6,
                    "pool": "0x0000000000000000000000000000000000000006",
                    "pool_type": "v3",
                    "fee": 500
                }
            }"#,
        );
        let err = validate_live_asset_symbol_uniqueness(
            file.path().to_str().unwrap(),
            &["ETH/USDC".to_string(), "ARB/USDC".to_string()],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("maps to multiple token addresses"));
    }

    #[test]
    fn live_known_token_validation_rejects_arbitrum_usdc_e_alias() {
        let file = write_address_book(
            r#"{
                "ARB/USDC": {
                    "base": "0x912ce59144191c1204e64559fe8253a0e49e6548",
                    "base_decimals": 18,
                    "quote": "0xff970a61a04b1ca14834a43f5de4533ebddb5cc8",
                    "quote_decimals": 6,
                    "pool": "0xcda53b1f66614552f834ceef361a8d12a0b8dad8",
                    "pool_type": "v3",
                    "fee": 500
                }
            }"#,
        );
        let err = validate_live_known_token_symbols(
            file.path().to_str().unwrap(),
            &["ARB/USDC".to_string()],
            ARBITRUM_CHAIN_ID,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("USDC_E"));
    }

    #[test]
    fn live_cex_pair_validation_rejects_non_alphanumeric_asset_symbol() {
        let err = validate_live_cex_pair_symbols(&["ARB/USDC_E".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("CEX-incompatible"));
    }

    #[test]
    fn flashbots_mainnet_relay_rejected_for_arbitrum() {
        let cli = Cli::parse_from([
            "arb_bot",
            "--simulation=false",
            "--dry-run=false",
            "--dex-chain-id",
            &ARBITRUM_CHAIN_ID.to_string(),
        ]);
        let err = validate_flashbots_relay_chain(&cli)
            .unwrap_err()
            .to_string();
        assert!(err.contains("mainnet-only"));
    }

    #[test]
    fn live_dex_validation_rejects_missing_pair() {
        let file = write_address_book(
            r#"{
                "WBTC/USDC": {
                    "base": "0x0000000000000000000000000000000000000001",
                    "base_decimals": 8,
                    "quote": "0x0000000000000000000000000000000000000002",
                    "quote_decimals": 6,
                    "pool": "0x0000000000000000000000000000000000000003",
                    "pool_type": "v2"
                }
            }"#,
        );
        let err = validate_live_dex_pool_compatibility(
            file.path().to_str().unwrap(),
            &["ETH/USDC".to_string()],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("missing from --dex-address-book"));
    }

    #[test]
    fn live_dex_validation_rejects_missing_pool() {
        let file = write_address_book(
            r#"{
                "ETH/USDC": {
                    "base": "0x0000000000000000000000000000000000000001",
                    "base_decimals": 18,
                    "quote": "0x0000000000000000000000000000000000000002",
                    "quote_decimals": 6,
                    "pool_type": "v2"
                }
            }"#,
        );
        let err = validate_live_dex_pool_compatibility(
            file.path().to_str().unwrap(),
            &["ETH/USDC".to_string()],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("has no pool"));
    }
}
