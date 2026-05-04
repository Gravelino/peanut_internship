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
use rust_decimal::Decimal;
use serde::Deserialize;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use peanut_internship_rust::chain::{ChainClient, FlashbotsConfig, FlashbotsRelayClient};
use peanut_internship_rust::core::types::Address;
use peanut_internship_rust::core::wallet::WalletManager;
use peanut_internship_rust::exchange::client::ExchangeClient;
use peanut_internship_rust::exchange::config::BinanceConfig;
use peanut_internship_rust::exchange::{OrderBookSnapshot, subscribe_book_ticker_stream};
use peanut_internship_rust::executor::engine::{
    Executor, ExecutorConfig, LegExecutor, LiveLegs, SimulatedLegs,
};
use peanut_internship_rust::executor::queue::{QueueConfig, QueueWorker, SignalQueue};
use peanut_internship_rust::executor::{
    ChainReceiptProvider, DexSwapper, DexSwapperConfig, FlashbotsSwapper, PairAddressBook,
    PairTokens, PendingReconcile, ReconcileConfig, ReconcileStore, ReconcileWorker, TickOutcome,
    UniswapV2Swapper,
};
use peanut_internship_rust::inventory::pnl::{ArbRecord, PnLEngine, TradeJsonlLogger, TradeLeg};
use peanut_internship_rust::inventory::tracker::InventoryTracker;
use peanut_internship_rust::inventory::types::Venue;
use peanut_internship_rust::inventory::wallet::WalletBalanceFetcher;
use peanut_internship_rust::observability::{
    AlertEvent, AlertProvider, AlertRules, AlertSink, HaltCoordinator, NoopSink, WebhookSink,
    emit_best_effort, evaluate_execution, mask_webhook_url,
};
use peanut_internship_rust::pricing::{V3QuoterConfig, V3QuoterKind};
use peanut_internship_rust::safety::{
    DEFAULT_KILL_SWITCH_FILE, PreTradeValidator, RiskLimits, RiskManager,
};
use peanut_internship_rust::strategy::fees::FeeStructure;
use peanut_internship_rust::strategy::generator::{
    CexOrderBookSource, GeneratorConfig, SignalGenerator, StubPriceSource,
};
use peanut_internship_rust::strategy::live_price_source::{
    AnyPriceSource, LivePoolConfig, LivePoolKind, LivePriceSource,
};
use peanut_internship_rust::strategy::scorer::{ScorerConfig, SignalScorer};
use tokio::sync::{Mutex, mpsc, watch};

const ARBITRUM_UNISWAP_V3_QUOTER_V2: &str = "0x61fFE014bA17989E743c5F6cB21bF9697530B21e";

/// CLI arguments.
#[derive(Debug, Parser)]
#[command(name = "arb_bot", about = "Cross-venue arbitrage bot")]
struct Cli {
    /// Trading pairs to watch (repeatable, or comma-separated via env).
    #[arg(long, default_values_t = vec!["ETH/USDC".to_string()], env = "PAIR", value_delimiter = ',')]
    pair: Vec<String>,

    /// Use Binance production credentials/endpoints. Env `PRODUCTION=true`
    /// also enables this mode.
    #[arg(long, default_value_t = false, env = "PRODUCTION")]
    production: bool,

    /// Base-asset size per leg.
    #[arg(long, default_value = "0.1", env = "ARB_SIZE")]
    size: String,

    /// Minimum score (0..=100) required to execute a signal.
    #[arg(long, default_value_t = 60, env = "MIN_SCORE")]
    min_score: u32,

    /// Verbose logging: show all market probes even if no signal is found.
    #[arg(long, default_value_t = false, env = "VERBOSE")]
    verbose: bool,

    /// Loop interval in milliseconds.
    #[arg(long, default_value_t = 1000, env = "TICK_MS")]
    tick_ms: u64,

    /// Use the simulated leg backend instead of live exchange calls.
    #[arg(long, default_value_t = true, env = "SIMULATION")]
    simulation: bool,

    /// Port for the Prometheus `/metrics` endpoint. Set to 0 to disable.
    #[arg(long, default_value_t = 9090)]
    metrics_port: u16,

    /// Maximum concurrent executions. Default = 1 (safe; matches pre-queue
    /// behaviour). Safely raising above 1 requires inventory locking (see
    /// stretch goal S6 in `docs/STRETCH_GOALS.md`).
    #[arg(long, default_value_t = 1, env = "MAX_CONCURRENT")]
    max_concurrent_executions: usize,

    /// Maximum queue depth. When full, the weakest-score signal is evicted.
    #[arg(long, default_value_t = 256)]
    queue_max_size: usize,

    /// SQLite path for persistent replay protection. Leave empty for
    /// in-memory only (default, matches pre-S8 behaviour).
    #[arg(long, default_value = "")]
    replay_db: String,

    /// Replay protection TTL in seconds. Ignored when `--replay-db` is empty.
    #[arg(long, default_value_t = 60)]
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
    #[arg(long, default_value_t = 60, env = "BALANCE_SYNC_INTERVAL")]
    pub balance_sync_interval_secs: u64,

    /// Tolerance (%) for post-trade balance verification. If the absolute
    /// difference between tracked and actual CEX balance exceeds this
    /// percentage, the bot emits a `BalanceMismatch` alert and halts.
    /// Set to 0 to disable verification.
    #[arg(long, default_value_t = 1.0)]
    balance_verify_tolerance_pct: f64,

    /// Webhook URL for alerting. Empty = alerts disabled (uses NoopSink).
    /// For Telegram, use `https://api.telegram.org/bot<TOKEN>/sendMessage`.
    #[arg(long, default_value = "")]
    alert_webhook_url: String,

    /// Webhook payload format: `telegram` or `generic` (default).
    #[arg(long, default_value = "generic")]
    alert_provider: String,

    /// Telegram chat ID for alerts. Required when `--alert-provider telegram`.
    #[arg(long, default_value = "")]
    alert_telegram_chat_id: String,

    /// Absolute-value loss (quote-asset units) that triggers a
    /// `LargeLoss` alert. Default 100 — tune to portfolio size.
    #[arg(long, default_value_t = 100)]
    alert_large_loss: u64,

    /// SQLite path for the reconcile store. When set, LEG2_TIMEOUT events
    /// with a known tx_hash are pushed into this database, and a background
    /// worker polls for receipts. Leave empty to disable (pre-S3 behaviour).
    #[arg(long, default_value = "")]
    reconcile_db: String,

    /// Reconcile worker polling interval in seconds.
    #[arg(long, default_value_t = 10)]
    reconcile_poll_secs: u64,

    /// Maximum age (seconds) of a pending reconcile entry before it expires
    /// and is flagged for manual review.
    #[arg(long, default_value_t = 3600)]
    reconcile_max_age_secs: u64,

    /// Path to a JSON file mapping pair symbols to on-chain token addresses.
    /// Enables live DEX execution via UniswapV2Swapper. When absent in live
    /// mode, the DEX leg returns NotImplemented. Shape:
    ///   `{"ETH/USDC": {"base": "0x...", "base_decimals": 18, "quote": "0x...", "quote_decimals": 6}}`
    #[arg(long, default_value = "", env = "DEX_ADDRESS_BOOK")]
    dex_address_book: String,

    /// Slippage tolerance in basis points for DEX swaps.
    #[arg(long, default_value_t = 50)]
    dex_slippage_bps: u64,

    /// DEX tx deadline in seconds from submission.
    #[arg(long, default_value_t = 60)]
    dex_deadline_secs: u64,

    /// Maximum EIP-1559 maxFeePerGas for live DEX transactions, in gwei.
    /// Set to 0 to disable the cap.
    #[arg(long, default_value_t = 0)]
    max_gas_gwei: u64,

    /// Environment variable name for the wallet private key used to sign
    /// DEX transactions. Only read when `--dex-address-book` is set.
    #[arg(long, default_value = "WALLET_PRIVATE_KEY")]
    wallet_key_env: String,

    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    use_flashbots: bool,

    #[arg(long, default_value = "https://relay.flashbots.net")]
    flashbots_relay_url: String,

    #[arg(long, default_value = "FLASHBOTS_AUTH_PRIVATE_KEY")]
    flashbots_auth_key_env: String,

    #[arg(long, default_value_t = 1)]
    flashbots_target_block_offset: u64,

    #[arg(long, default_value_t = 3)]
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
    #[arg(long, default_value = "5", env = "MIN_PROFIT_USD")]
    min_profit_usd: String,

    /// Minimum spread in basis points required to consider an opportunity.
    /// Overrides `GeneratorConfig::default().min_spread_bps` (50).
    #[arg(long, default_value_t = 50, env = "MIN_SPREAD_BPS")]
    min_spread_bps: u64,

    /// CEX taker fee in basis points. Drives both pre-trade profitability
    /// gating and post-trade realised PnL accounting (shared
    /// [`FeeStructure`]). Binance spot default = 10 bps.
    #[arg(long, default_value_t = 10, env = "FEE_CEX_TAKER_BPS")]
    fee_cex_taker_bps: u64,

    /// DEX swap fee in basis points. Uniswap V2 = 30 bps; Uniswap V3 tiers
    /// vary (5 / 30 / 100 / 1000 bps). Used in both generator and executor.
    #[arg(long, default_value_t = 30, env = "FEE_DEX_SWAP_BPS")]
    fee_dex_swap_bps: u64,

    /// Flat on-chain gas cost in USD per execution. Amortised per trade
    /// inside `FeeStructure::total_fee_bps` — small notionals pay a
    /// disproportionately higher %-ge. Default $5.
    #[arg(long, default_value = "5", env = "FEE_GAS_USD")]
    fee_gas_usd: String,

    /// Append-only structured trade log path. Completed executions are
    /// written as one JSON object per line. Leave empty to disable.
    #[arg(long, default_value = "trades.jsonl", env = "TRADE_LOG_PATH")]
    trade_log_path: String,

    /// Maximum time to wait for queued/in-flight executions to finish during
    /// graceful shutdown.
    #[arg(long, default_value_t = 30)]
    shutdown_drain_secs: u64,

    /// Maximum cumulative daily loss (USD) before the bot auto-halts.
    /// Set to 0 to disable. Default $100.
    #[arg(long, default_value = "100", env = "MAX_DAILY_LOSS")]
    max_daily_loss_usd: String,

    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    dry_run: bool,

    #[arg(long, default_value = "100", env = "INITIAL_CAPITAL_USD")]
    initial_capital_usd: String,

    #[arg(long, default_value = "5", env = "RISK_MAX_TRADE_USD")]
    risk_max_trade_usd: String,

    #[arg(long, default_value = "10", env = "RISK_MAX_DAILY_LOSS_USD")]
    risk_max_daily_loss_usd: String,

    #[arg(long, default_value_t = 20, env = "RISK_MAX_TRADES_PER_HOUR")]
    risk_max_trades_per_hour: u32,

    #[arg(long, default_value_t = 3, env = "RISK_CONSECUTIVE_LOSS_LIMIT")]
    risk_consecutive_loss_limit: u32,

    #[arg(long, default_value = "logs", env = "LOG_DIR")]
    log_dir: String,

    /// Path to a watchdog halt file. When this file exists, the bot halts
    /// immediately. Useful for emergency stops via `touch STOP`. Leave
    /// empty to disable.
    #[arg(long, default_value = "/tmp/arb_bot_kill", env = "HALT_FILE")]
    halt_file_path: String,
}

struct WsCexOrderBookSource {
    exchange: Arc<ExchangeClient>,
    snapshots: Arc<RwLock<HashMap<String, OrderBookSnapshot>>>,
    max_age: Duration,
}

impl WsCexOrderBookSource {
    async fn new(exchange: Arc<ExchangeClient>, ws_url: String, pairs: &[String]) -> Self {
        let snapshots = Arc::new(RwLock::new(HashMap::new()));
        for pair in pairs {
            match exchange.fetch_order_book(pair, 20).await {
                Ok(snapshot) => {
                    snapshots.write().await.insert(pair.clone(), snapshot);
                    info!(pair, "CEX order book seeded from REST snapshot");
                }
                Err(error) => {
                    warn!(pair, error = %error, "failed to seed CEX order book from REST");
                }
            }

            spawn_book_ticker_cache(ws_url.clone(), pair.clone(), Arc::clone(&snapshots));
        }

        Self {
            exchange,
            snapshots,
            max_age: Duration::from_secs(60),
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
        if let Some(snapshot) = self.snapshots.read().await.get(pair).cloned() {
            if now.saturating_sub(snapshot.timestamp) <= self.max_age.as_millis() as u64 {
                return Ok(snapshot);
            }
            warn!(pair, "CEX WS snapshot stale; falling back to REST");
        }

        Ok(self.exchange.fetch_order_book(pair, limit).await?)
    }
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
                            Some((ask_price - bid_price) / mid * Decimal::from(10_000))
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
            tokio::time::sleep(Duration::from_secs(5)).await;
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
            let mut interval = tokio::time::interval(Duration::from_millis(250));
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

    let production = cli.production || peanut_internship_rust::config::env_production_enabled();
    if production {
        warn!("PRODUCTION MODE - REAL MONEY");
    } else {
        info!("Testnet mode - fake money");
    }

    let exchange_cfg = BinanceConfig::from_env_for(production)?;
    info!(
        sandbox = exchange_cfg.sandbox,
        base_url = %exchange_cfg.base_url,
        ws_url = %exchange_cfg.ws_url,
        "Binance configuration loaded"
    );
    let cex_ws_url = exchange_cfg.ws_url.clone();
    let exchange = Arc::new(ExchangeClient::new(exchange_cfg)?);
    let mut tracked_pairs = cli.pair.clone();
    for p in &cli.pair {
        if p.ends_with("/ETH") && !tracked_pairs.contains(&"ETH/USDC".to_string()) {
            tracked_pairs.push("ETH/USDC".to_string());
        }
    }

    let cex_order_books: Arc<dyn CexOrderBookSource> = Arc::new(
        WsCexOrderBookSource::new(Arc::clone(&exchange), cex_ws_url, &tracked_pairs).await,
    );

    // Inventory tracker (empty; will be populated by sync_balances).
    let inventory = Arc::new(RwLock::new(InventoryTracker::new(vec![
        Venue::Binance,
        Venue::Wallet,
    ])));

    // Optional synthetic seed so simulation demos work without live balance
    // endpoints. Applied BEFORE the first tick so the inventory pre-check
    // in `SignalGenerator` has something to approve.
    if !cli.seed_inventory.is_empty() {
        if !cli.simulation {
            warn!(
                "--seed-inventory used outside --simulation; synthetic balances will be overwritten by the next sync"
            );
        }
        let mut guard = inventory.write().await;
        for spec in &cli.seed_inventory {
            match parse_seed_spec(spec) {
                Ok((venue, balances)) => {
                    let count = balances.len();
                    info!(venue = %venue, assets = count, "seeding inventory");
                    guard.update_from_wallet(venue, balances);
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
    // When `--dex-address-book` contains a `pool` address for a pair AND a
    // chain RPC is configured, we read live Uniswap V2 reserves instead of
    // synthesising DEX prices from CEX mid. Otherwise we fall back to the
    // deterministic `StubPriceSource` (demo-friendly, no RPC load).
    let price_source: Arc<AnyPriceSource> = {
        let live_pools = if !cli.dex_address_book.is_empty() {
            load_live_pool_book(&cli.dex_address_book)?
        } else {
            Vec::new()
        };
        let rpc_urls = resolve_rpc_urls(&cli);
        match (live_pools.is_empty(), rpc_urls) {
            (false, Some(rpc_urls)) => {
                let client = ChainClient::new(rpc_urls, 30, 3)?;
                log_rpc_health("live price source", &client).await;
                let live = LivePriceSource::new_with_order_books(
                    Arc::clone(&cex_order_books),
                    client,
                    live_pools,
                )
                .await?;
                // Start the background WS block feed so DEX prices update
                // on every new block (same real-time pattern as CEX bookTicker).
                let size = Decimal::from_str_exact(&cli.size).unwrap_or(Decimal::ONE);
                if let Some(ws_url) = resolve_ws_url(&resolve_rpc_urls(&cli).unwrap_or_default()) {
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
            _ => {
                info!("price source: stub (synthetic DEX prices)");
                Arc::new(AnyPriceSource::Stub(StubPriceSource::new_with_order_books(
                    Arc::clone(&cex_order_books),
                )))
            }
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
    let fees = FeeStructure {
        cex_taker_bps: Decimal::from(cli.fee_cex_taker_bps),
        dex_swap_bps: Decimal::from(cli.fee_dex_swap_bps),
        gas_cost_usd: Decimal::from_str_exact(&cli.fee_gas_usd)
            .map_err(|e| format!("invalid --fee-gas-usd: {e}"))?,
    };
    info!(
        min_profit_usd = %generator_config.min_profit_usd,
        min_spread_bps = %generator_config.min_spread_bps,
        cex_taker_bps = %fees.cex_taker_bps,
        dex_swap_bps = %fees.dex_swap_bps,
        gas_cost_usd = %fees.gas_cost_usd,
        "generator thresholds + fee model"
    );
    let mut generator = SignalGenerator::new(
        price_source,
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
    let mut effective_use_flashbots = cli.use_flashbots;
    let legs: Arc<dyn peanut_internship_rust::executor::engine::LegExecutor> = if cli.simulation {
        Arc::new(SimulatedLegs::default())
    } else {
        if !cli.dex_address_book.is_empty() {
            let rpc_urls = resolve_wallet_config(&cli)
                .map(|(rpc, _)| rpc)
                .ok_or("live DEX execution requires --eth-rpc-url or ETH_RPC_URL")?;
            let chain_client = ChainClient::new(rpc_urls, 30, 3)?;
            log_rpc_health("live dex", &chain_client).await;
            let wallet = WalletManager::from_env(&cli.wallet_key_env)?;
            let recipient = Address::new(wallet.address())?;
            let address_book = Arc::new(load_address_book(&cli.dex_address_book)?);
            let dex_config = DexSwapperConfig {
                slippage_bps: cli.dex_slippage_bps,
                deadline_secs: cli.dex_deadline_secs,
                max_gas_gwei: (cli.max_gas_gwei > 0).then_some(cli.max_gas_gwei),
                ..DexSwapperConfig::default()
            };
            let base_swapper =
                UniswapV2Swapper::new(chain_client.clone(), wallet.clone(), dex_config.clone());
            let swapper: Arc<dyn DexSwapper> = if cli.use_flashbots {
                match WalletManager::from_env(&cli.flashbots_auth_key_env) {
                    Ok(auth_wallet) => {
                        let flashbots_config = FlashbotsConfig {
                            relay_url: cli.flashbots_relay_url.clone(),
                            target_block_offset: cli.flashbots_target_block_offset,
                            max_blocks_to_try: cli.flashbots_max_blocks_to_try,
                            ..FlashbotsConfig::default()
                        };
                        let relay = Arc::new(FlashbotsRelayClient::new(
                            flashbots_config.clone(),
                            auth_wallet,
                        ));
                        info!(
                            relay = %flashbots_config.relay_url,
                            target_block_offset = flashbots_config.target_block_offset,
                            max_blocks_to_try = flashbots_config.max_blocks_to_try,
                            "live mode: DEX leg wired via FlashbotsSwapper"
                        );
                        Arc::new(FlashbotsSwapper::new(base_swapper, relay, flashbots_config))
                    }
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
                        Arc::new(base_swapper)
                    }
                }
            } else {
                Arc::new(base_swapper)
            };
            info!(
                address_book = %cli.dex_address_book,
                slippage_bps = cli.dex_slippage_bps,
                max_gas_gwei = ?dex_config.max_gas_gwei,
                private_dex = effective_use_flashbots,
                "live mode: DEX leg configured"
            );
            Arc::new(LiveLegs::new(Arc::clone(&exchange)).with_dex(
                swapper,
                address_book,
                dex_config,
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
    let mut executor_builder = Executor::with_replay(legs, executor_config, replay).with_fees(fees);
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
                let mut parts = signal.pair.split('/');
                let base = parts.next().unwrap();
                let quote = parts.next().unwrap();

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
                let done = ctx.state.is_filled();
                scorer.lock().await.record_result(&pair_str, done);
                if done {
                    if let Some(net) = ctx.actual_net_pnl {
                        info!(pair = pair_str, pnl = %net, state = %ctx.state, "SUCCESS");
                        // Feed PnL breaker — trips halt when daily loss exceeded.
                        let mut pb = pnl_breaker.lock().await;
                        let was_halted = pb.is_halted();
                        pb.record_pnl(net);
                        if pb.is_halted() && !was_halted {
                            let ev = AlertEvent::DailyLossHalt {
                                cumulative_pnl: pb.cumulative_pnl().to_string(),
                                max_daily_loss: pb.max_daily_loss().to_string(),
                            };
                            drop(pb); // release lock before async emit
                            emit_best_effort(&alert_sink, &ev).await;
                            halt_coordinator.halt("daily PnL loss threshold exceeded");
                        }
                        peanut_internship_rust::observability::metrics_handle()
                            .set_pnl_breaker_halted(pnl_breaker.lock().await.is_halted());
                        risk_manager.lock().await.record_trade(net);
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
            match ChainClient::new(rpc_urls, 30, 3) {
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
                info!(wallet = %addr, rpc_endpoints = rpc_urls.len(), "wallet balance fetcher enabled");
                Some(f)
            }
            Err(e) => {
                warn!(error = %e, "failed to init WalletBalanceFetcher; wallet sync disabled");
                None
            }
        },
        None => None,
    };

    let mode_str = if production {
        "production"
    } else if cli.dry_run {
        "dry-run"
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
    info!(
        pairs = ?cli.pair,
        size = %trade_size,
        max_concurrent = cli.max_concurrent_executions,
        wallet_sync = wallet_fetcher.is_some(),
        balance_sync_secs = cli.balance_sync_interval_secs,
        "bot starting"
    );

    let preserve_seeded_inventory = cli.simulation && !cli.seed_inventory.is_empty();
    if preserve_seeded_inventory {
        info!("simulation seed inventory active; skipping balance sync");
    } else {
        sync_cex_balance(&exchange, &inventory).await;
        if let Some(ref f) = wallet_fetcher {
            sync_wallet_balance(f, &inventory).await;
        }
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
        pre_trade_validator: Arc::clone(&pre_trade_validator),
        cex_order_books: Arc::clone(&cex_order_books),
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
            break;
        }
        if let Err(e) = tick_result {
            error!("tick error: {e}");
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }
        // Gate balance syncs by the configured interval — tight `--tick-ms`
        // loops must not hammer the exchange `/account` or RPC endpoints.
        if last_sync.elapsed() >= sync_interval {
            if !preserve_seeded_inventory {
                sync_cex_balance(&exchange, &inventory).await;
                // Post-trade balance verification: compare tracked vs actual.
                if cli.balance_verify_tolerance_pct > 0.0 {
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
    pre_trade_validator: Arc<PreTradeValidator>,
    cex_order_books: Arc<dyn CexOrderBookSource>,
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
    let stdout_layer = tracing_subscriber::fmt::layer().with_writer(io::stdout);
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(file_writer)
        .with_ansi(false);

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

async fn tick(
    pairs: &[String],
    size: Decimal,
    min_score: Decimal,
    generator: &mut SignalGenerator<AnyPriceSource>,
    deps: &TickDeps,
) -> Result<(), Box<dyn std::error::Error>> {
    // Fast path: skip the tick entirely when the breaker is open. The queue
    // worker also pre-flight-checks the breaker via `Executor::execute`, but
    // filtering here avoids burning cycles on signal generation we know we
    // won't act on.
    let cb = deps.executor.circuit_breaker();
    {
        let mut cb_guard = cb.lock().await;
        if cb_guard.is_open() {
            let remaining = cb_guard.time_until_reset();
            warn!(remaining_s = remaining.as_secs(), "circuit breaker open");
            return Ok(());
        }
    }

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
                        if deps.verbose {
                            info!(
                                pair,
                                size = %m.size,
                                cex_bid = %m.cex_bid,
                                cex_ask = %m.cex_ask,
                                dex_buy = %m.dex_buy,
                                dex_sell = %m.dex_sell,
                                buy_cex_bps = %m.spread_buy_cex_bps,
                                buy_dex_bps = %m.spread_buy_dex_bps,
                                "DRY_RUN no_signal"
                            );
                        } else {
                            debug!(pair, size = %m.size, "DRY_RUN no_signal");
                        }
                    } else {
                        if deps.verbose {
                            info!(pair, size = %size, "DRY_RUN no_signal (no market info)");
                        }
                    }
                }
                continue;
            }
        };

        let validation = deps.pre_trade_validator.validate_signal(&signal);
        if !validation.allowed() {
            warn!(
                pair,
                reason = validation.reason(),
                spread_bps = %signal.spread_bps,
                "pre-trade validation failed"
            );
            continue;
        }

        let risk_decision = deps.risk_manager.lock().await.check_pre_trade(&signal);
        if !risk_decision.allowed() {
            warn!(
                pair,
                reason = risk_decision.reason(),
                spread_bps = %signal.spread_bps,
                "risk check failed"
            );
            continue;
        }

        // Score + threshold. Acquire scorer lock briefly; don't hold across
        // the queue push below (push takes its own lock internally).
        let skews = generator_inventory_skews(generator).await;
        let book_result = deps.cex_order_books.fetch_order_book(pair, 20).await;
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
                spread_bps = %signal.spread_bps,
                size = %signal.size,
                score = %signal.score,
                "skipped: score below threshold"
            );
            continue;
        }

        if deps.dry_run {
            info!(
                pair,
                direction = %signal.direction,
                size = %signal.size,
                cex_price = %signal.cex_price,
                dex_price = %signal.dex_price,
                spread_bps = %signal.spread_bps,
                expected_gross_pnl = %signal.expected_gross_pnl,
                expected_fees = %signal.expected_fees,
                expected_net_pnl = %signal.expected_net_pnl,
                score = %signal.score,
                "DRY_RUN would_trade"
            );
            continue;
        }

        info!(
            pair,
            spread_bps = %signal.spread_bps,
            score = %signal.score,
            direction = %signal.direction,
            "enqueue"
        );

        // Enqueue for the worker to pick up. Push returns `false` when the
        // signal lost backpressure (queue full and score was the weakest).
        if !deps.queue.push(signal).await {
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
) {
    match exchange.fetch_balance().await {
        Ok(bals) => {
            inventory
                .write()
                .await
                .update_from_cex(Venue::Binance, bals);
        }
        Err(e) => warn!("fetch_balance failed: {e}"),
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
    // Build a simple total-balance map from the NormalizedBalance values.
    let actual: HashMap<String, Decimal> = fresh
        .iter()
        .map(|(asset, bal)| (asset.clone(), bal.free + bal.locked))
        .collect();
    let mismatches = inventory
        .read()
        .await
        .verify_balances(Venue::Binance, &actual, tolerance_pct);
    Some(mismatches)
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

/// JSON shape for each entry in the `--dex-address-book` file.
#[derive(Deserialize)]
struct AddressBookEntry {
    base: String,
    base_decimals: u8,
    quote: String,
    quote_decimals: u8,
    /// Optional Uniswap V2 pool address. When present and a chain RPC is
    /// configured, the bot reads live reserves from this pool for signal
    /// generation (`LivePriceSource`). Absent -> falls back to stub prices.
    #[serde(default)]
    pool: Option<String>,
    #[serde(default = "default_pool_type")]
    pool_type: String,
    #[serde(default)]
    quoter: Option<String>,
    #[serde(default = "default_quoter_type")]
    quoter_type: String,
}

fn default_pool_type() -> String {
    "v2".to_string()
}

fn default_quoter_type() -> String {
    "quoter_v2".to_string()
}

fn load_address_book(path: &str) -> Result<PairAddressBook, Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(path)?;
    let raw: std::collections::HashMap<String, AddressBookEntry> = serde_json::from_str(&content)?;
    let mut book = PairAddressBook::new();
    for (pair, entry) in raw {
        book.insert(
            pair,
            PairTokens {
                base: Address::new(&entry.base)?,
                base_decimals: entry.base_decimals,
                quote: Address::new(&entry.quote)?,
                quote_decimals: entry.quote_decimals,
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
fn load_live_pool_book(path: &str) -> Result<Vec<LivePoolEntry>, Box<dyn std::error::Error>> {
    use peanut_internship_rust::core::types::Token;
    let content = std::fs::read_to_string(path)?;
    let raw: std::collections::HashMap<String, AddressBookEntry> = serde_json::from_str(&content)?;
    let mut out = Vec::new();
    for (pair, entry) in raw {
        let Some(pool_str) = entry.pool.as_deref() else {
            continue;
        };
        let (base_symbol, quote_symbol) = pair.split_once('/').ok_or_else(|| {
            format!("pair '{pair}' missing '/' separator; cannot infer token symbols")
        })?;
        let pool = Address::new(pool_str)?;
        let kind = match entry.pool_type.to_ascii_lowercase().as_str() {
            "v2" => LivePoolKind::V2,
            "v3" => LivePoolKind::V3,
            other => {
                return Err(format!(
                    "pair '{pair}' has unsupported pool_type '{other}', expected 'v2' or 'v3'"
                )
                .into());
            }
        };
        let quoter = match kind {
            LivePoolKind::V2 => None,
            LivePoolKind::V3 => {
                let quoter_address = entry
                    .quoter
                    .as_deref()
                    .unwrap_or(ARBITRUM_UNISWAP_V3_QUOTER_V2);
                let quoter_kind = match entry.quoter_type.to_ascii_lowercase().as_str() {
                    "v2" | "quoter_v2" => V3QuoterKind::QuoterV2,
                    other => {
                        return Err(format!(
                            "pair '{pair}' has unsupported quoter_type '{other}', expected 'quoter_v2'"
                        )
                        .into());
                    }
                };
                Some(V3QuoterConfig {
                    address: Address::new(quoter_address)?,
                    kind: quoter_kind,
                })
            }
        };
        let base = Token {
            address: Address::new(&entry.base)?,
            symbol: base_symbol.to_string(),
            decimals: entry.base_decimals,
        };
        let quote = Token {
            address: Address::new(&entry.quote)?,
            symbol: quote_symbol.to_string(),
            decimals: entry.quote_decimals,
        };
        out.push(LivePoolConfig {
            pair_name: pair,
            address: pool,
            base,
            quote,
            kind,
            quoter,
        });
    }
    Ok(out)
}

/// Resolves `(rpc_url, wallet_address)` from CLI first, then env vars.
/// Returns `None` when either is empty — caller skips wallet sync.
fn resolve_wallet_config(cli: &Cli) -> Option<(Vec<String>, String)> {
    let rpc_urls = resolve_rpc_urls(cli).unwrap_or_default();
    let addr = if cli.wallet_address.is_empty() {
        std::env::var("WALLET_ADDRESS").unwrap_or_default()
    } else {
        cli.wallet_address.clone()
    };
    if rpc_urls.is_empty() || addr.is_empty() {
        None
    } else {
        Some((rpc_urls, addr))
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
                let size = Decimal::from_str_exact(&entry.leg1_fill_size).unwrap_or(Decimal::ZERO);
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
    let parts: Vec<&str> = signal.pair.split('/').collect();
    let quote = parts
        .get(1)
        .copied()
        .map(|q| q.to_string())
        .unwrap_or_else(|| {
            warn!(pair = %signal.pair, "malformed pair in arb record; defaulting fee_asset to USDT");
            "USDT".to_string()
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
    let (buy_size, buy_price, sell_size, sell_price) = if buy_is_leg1 {
        (
            ctx.leg1_fill_size,
            ctx.leg1_fill_price,
            ctx.leg2_fill_size,
            ctx.leg2_fill_price,
        )
    } else {
        (
            ctx.leg2_fill_size,
            ctx.leg2_fill_price,
            ctx.leg1_fill_size,
            ctx.leg1_fill_price,
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

    let buy_leg = TradeLeg {
        id: format!("{}_buy", signal.signal_id),
        timestamp: started,
        venue: buy_venue,
        symbol: signal.pair.clone(),
        side: "buy".into(),
        amount: buy_size.unwrap_or(Decimal::ZERO),
        price: buy_price.unwrap_or(Decimal::ZERO),
        fee: Decimal::ZERO,
        fee_asset: quote.clone(),
    };
    let sell_leg = TradeLeg {
        id: format!("{}_sell", signal.signal_id),
        timestamp: finished,
        venue: sell_venue,
        symbol: signal.pair.clone(),
        side: "sell".into(),
        amount: sell_size.unwrap_or(Decimal::ZERO),
        price: sell_price.unwrap_or(Decimal::ZERO),
        fee: Decimal::ZERO,
        fee_asset: quote,
    };

    ArbRecord {
        id: signal.signal_id.clone(),
        timestamp: started,
        buy_leg,
        sell_leg,
        gas_cost_usd: Decimal::ZERO,
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

#[cfg(test)]
mod seed_tests {
    use super::*;

    #[test]
    fn parses_binance_single_asset() {
        let (venue, map) = parse_seed_spec("binance:USDT=10000").unwrap();
        assert_eq!(venue, Venue::Binance);
        assert_eq!(map.get("USDT"), Some(&Decimal::from(10000)));
    }

    #[test]
    fn parses_wallet_multi_with_case_insensitive_venue_and_asset() {
        let (venue, map) = parse_seed_spec("Wallet: eth=2.5 , usdc=100").unwrap();
        assert_eq!(venue, Venue::Wallet);
        assert_eq!(map.get("ETH"), Some(&Decimal::new(25, 1)));
        assert_eq!(map.get("USDC"), Some(&Decimal::from(100)));
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
}
