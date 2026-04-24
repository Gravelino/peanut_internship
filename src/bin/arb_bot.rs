//! End-to-end arbitrage bot: detects, scores, and executes opportunities.
//!
//! This binary wires Week 1-4 modules together:
//! - [`ExchangeClient`] for CEX market data
//! - [`StubPriceSource`] for DEX prices (falls back without a fork URL)
//! - [`InventoryTracker`] / [`PnLEngine`] for state and accounting
//! - [`SignalGenerator`] + [`SignalScorer`] for opportunity detection
//! - [`Executor`] for CEX/DEX leg coordination (simulation_mode by default)

use std::sync::Arc;
use std::time::Duration;

use chrono::DateTime;
use clap::Parser;
use rust_decimal::Decimal;
use serde::Deserialize;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use peanut_internship_rust::chain::ChainClient;
use peanut_internship_rust::core::types::Address;
use peanut_internship_rust::core::wallet::WalletManager;
use peanut_internship_rust::exchange::client::ExchangeClient;
use peanut_internship_rust::exchange::config::BinanceConfig;
use peanut_internship_rust::executor::engine::{
    Executor, ExecutorConfig, LegExecutor, LiveLegs, SimulatedLegs,
};
use peanut_internship_rust::executor::queue::{QueueConfig, QueueWorker, SignalQueue};
use peanut_internship_rust::executor::{
    ChainReceiptProvider, DexSwapperConfig, PairAddressBook, PairTokens, PendingReconcile,
    ReconcileConfig, ReconcileStore, ReconcileWorker, TickOutcome, UniswapV2Swapper,
};
use peanut_internship_rust::inventory::pnl::{ArbRecord, PnLEngine, TradeLeg};
use peanut_internship_rust::inventory::tracker::InventoryTracker;
use peanut_internship_rust::inventory::types::Venue;
use peanut_internship_rust::inventory::wallet::WalletBalanceFetcher;
use peanut_internship_rust::observability::{
    AlertEvent, AlertProvider, AlertRules, AlertSink, NoopSink, WebhookSink, emit_best_effort,
    evaluate_execution, mask_webhook_url,
};
use peanut_internship_rust::strategy::fees::FeeStructure;
use peanut_internship_rust::strategy::generator::{
    GeneratorConfig, SignalGenerator, StubPriceSource,
};
use peanut_internship_rust::strategy::scorer::SignalScorer;
use tokio::sync::{Mutex, mpsc};

/// CLI arguments.
#[derive(Debug, Parser)]
#[command(name = "arb_bot", about = "Cross-venue arbitrage bot")]
struct Cli {
    /// Trading pairs to watch (repeatable).
    #[arg(long, default_values_t = vec!["ETH/USDT".to_string()])]
    pair: Vec<String>,

    /// Base-asset size per leg.
    #[arg(long, default_value = "0.1")]
    size: String,

    /// Minimum score (0..=100) required to execute a signal.
    #[arg(long, default_value_t = 60)]
    min_score: u32,

    /// Loop interval in milliseconds.
    #[arg(long, default_value_t = 1000)]
    tick_ms: u64,

    /// Use the simulated leg backend instead of live exchange calls.
    #[arg(long, default_value_t = true)]
    simulation: bool,

    /// Port for the Prometheus `/metrics` endpoint. Set to 0 to disable.
    #[arg(long, default_value_t = 9090)]
    metrics_port: u16,

    /// Maximum concurrent executions. Default = 1 (safe; matches pre-queue
    /// behaviour). Safely raising above 1 requires inventory locking (see
    /// stretch goal S6 in `docs/STRETCH_GOALS.md`).
    #[arg(long, default_value_t = 1)]
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
    #[arg(long, default_value = "")]
    eth_rpc_url: String,

    /// Wallet address to monitor for on-chain ERC-20 + ETH balances. Leave
    /// empty to skip (matches pre-S6 behaviour).
    /// Falls back to the `WALLET_ADDRESS` environment variable if empty.
    #[arg(long, default_value = "")]
    wallet_address: String,

    /// Minimum interval (seconds) between full balance re-syncs. Prevents
    /// RPC / CEX hammering on tight tick intervals.
    #[arg(long, default_value_t = 30)]
    balance_sync_interval_secs: u64,

    /// Webhook URL for alerting. Empty = alerts disabled (uses NoopSink).
    /// Supports Slack / Discord incoming-webhook shapes and a generic
    /// JSON format (see `--alert-provider`).
    #[arg(long, default_value = "")]
    alert_webhook_url: String,

    /// Webhook payload format: `slack`, `discord`, or `generic` (default).
    #[arg(long, default_value = "generic")]
    alert_provider: String,

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
    #[arg(long, default_value = "")]
    dex_address_book: String,

    /// Slippage tolerance in basis points for DEX swaps.
    #[arg(long, default_value_t = 50)]
    dex_slippage_bps: u64,

    /// Deadline offset in seconds for DEX swaps.
    #[arg(long, default_value_t = 60)]
    dex_deadline_secs: u64,

    /// Environment variable name for the wallet private key used to sign
    /// DEX transactions. Only read when `--dex-address-book` is set.
    #[arg(long, default_value = "WALLET_PRIVATE_KEY")]
    wallet_key_env: String,

    /// Seed the inventory tracker with synthetic balances before the first
    /// tick. Intended for demos / integration tests running in
    /// `--simulation` mode, where `sync_cex_balance` would otherwise
    /// require Binance testnet credentials.
    ///
    /// Format: `venue:ASSET=AMOUNT[,ASSET=AMOUNT]...` where venue is
    /// `binance` or `wallet`. Repeatable. Example:
    ///   `--seed-inventory binance:USDT=10000,ETH=5 --seed-inventory wallet:ETH=2`
    #[arg(long)]
    seed_inventory: Vec<String>,

    /// Minimum net profit in quote-asset units required for the generator
    /// to emit a signal. Overrides `GeneratorConfig::default().min_profit_usd`
    /// (which is 5). Lower this for small-notional demos where default
    /// fees consume more than the achievable spread.
    #[arg(long, default_value = "5")]
    min_profit_usd: String,

    /// Minimum spread in basis points required to consider an opportunity.
    /// Overrides `GeneratorConfig::default().min_spread_bps` (50).
    #[arg(long, default_value_t = 50)]
    min_spread_bps: u64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().init();

    let cli = Cli::parse();
    let trade_size =
        Decimal::from_str_exact(&cli.size).map_err(|e| format!("invalid --size: {e}"))?;
    let min_score = Decimal::from(cli.min_score);

    // Prometheus metrics: init global registry + spawn /metrics server.
    // When `--metrics-port 0`, we still init the registry (so instrumented
    // code records observations) but skip the HTTP endpoint.
    let _metrics = peanut_internship_rust::observability::init_metrics(
        peanut_internship_rust::observability::Metrics::new(),
    );
    if cli.metrics_port != 0 {
        let addr = std::net::SocketAddr::from(([0, 0, 0, 0], cli.metrics_port));
        let m = _metrics.clone();
        tokio::spawn(async move {
            if let Err(e) = peanut_internship_rust::observability::serve_metrics(addr, m).await {
                error!(error = %e, "metrics server terminated");
            }
        });
        info!(port = cli.metrics_port, "metrics endpoint /metrics enabled");
    }

    // Exchange client (Binance testnet credentials from .env).
    let exchange_cfg = BinanceConfig::from_env()?;
    let exchange = Arc::new(ExchangeClient::new(exchange_cfg)?);

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
    let price_source = Arc::new(StubPriceSource::new(Arc::clone(&exchange)));
    let generator_config = GeneratorConfig {
        min_profit_usd: Decimal::from_str_exact(&cli.min_profit_usd)
            .map_err(|e| format!("invalid --min-profit-usd: {e}"))?,
        min_spread_bps: Decimal::from(cli.min_spread_bps),
        ..GeneratorConfig::default()
    };
    info!(
        min_profit_usd = %generator_config.min_profit_usd,
        min_spread_bps = %generator_config.min_spread_bps,
        "generator thresholds"
    );
    let mut generator = SignalGenerator::new(
        price_source,
        Arc::clone(&inventory),
        FeeStructure::default(),
        generator_config,
    );
    let scorer = SignalScorer::default();

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
    let legs: Arc<dyn peanut_internship_rust::executor::engine::LegExecutor> = if cli.simulation {
        Arc::new(SimulatedLegs::default())
    } else {
        if !cli.dex_address_book.is_empty() {
            let rpc_url = resolve_wallet_config(&cli)
                .map(|(rpc, _)| rpc)
                .ok_or("live DEX execution requires --eth-rpc-url or ETH_RPC_URL")?;
            let chain_client = ChainClient::new(vec![rpc_url], 30, 3)?;
            let wallet = WalletManager::from_env(&cli.wallet_key_env)?;
            let recipient = Address::new(wallet.address())?;
            let address_book = Arc::new(load_address_book(&cli.dex_address_book)?);
            let dex_config = DexSwapperConfig {
                slippage_bps: cli.dex_slippage_bps,
                deadline_secs: cli.dex_deadline_secs,
                ..DexSwapperConfig::default()
            };
            let swapper = Arc::new(UniswapV2Swapper::new(
                chain_client,
                wallet,
                dex_config.clone(),
            ));
            info!(
                address_book = %cli.dex_address_book,
                slippage_bps = cli.dex_slippage_bps,
                "live mode: DEX leg wired via UniswapV2Swapper"
            );
            Arc::new(LiveLegs::new(Arc::clone(&exchange)).with_dex(
                swapper,
                address_book,
                dex_config,
                recipient,
            ))
        } else {
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

    let mut executor_builder = Executor::with_replay(legs, ExecutorConfig::default(), replay);
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
    let worker = QueueWorker::new(
        Arc::clone(&queue),
        Arc::clone(&executor),
        cli.max_concurrent_executions,
        Duration::from_millis(50),
    )
    .with_sink(completion_tx);

    tokio::spawn(async move { worker.run().await });

    // Alert sink: WebhookSink when a URL is configured, else NoopSink. The
    // sink is cheap to clone (wraps an `Arc<reqwest::Client>`), so we share
    // it across the completion consumer and the breaker watcher.
    let alert_sink: Arc<dyn AlertSink> = if cli.alert_webhook_url.is_empty() {
        Arc::new(NoopSink)
    } else {
        let provider = AlertProvider::parse(&cli.alert_provider);
        // SECURITY: never log the raw webhook URL — Slack / Discord embed
        // auth tokens in the URL path. Always use `mask_webhook_url`.
        info!(
            url = %mask_webhook_url(&cli.alert_webhook_url),
            provider = ?provider,
            "alerts enabled"
        );
        Arc::new(WebhookSink::new(
            cli.alert_webhook_url.clone(),
            provider,
            Duration::from_secs(5),
        ))
    };
    let alert_rules = AlertRules {
        large_loss_threshold: Decimal::from(cli.alert_large_loss),
    };

    // Completion consumer: update scorer history + ledger, and fire alerts
    // derived from the terminal state via `evaluate_execution`.
    {
        let scorer = Arc::clone(&scorer);
        let pnl = Arc::clone(&pnl);
        let alert_sink = Arc::clone(&alert_sink);
        let alert_rules = alert_rules.clone();
        tokio::spawn(async move {
            while let Some(ctx) = completion_rx.recv().await {
                let pair_str = ctx.signal.pair.clone();
                let done = ctx.state.is_filled();
                scorer.lock().await.record_result(&pair_str, done);
                if done {
                    if let Some(net) = ctx.actual_net_pnl {
                        info!(pair = pair_str, pnl = %net, state = %ctx.state, "SUCCESS");
                    }
                    pnl.lock().await.record(execution_to_arb_record(&ctx));
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
        let rpc_url = resolve_wallet_config(&cli).map(|(rpc, _)| rpc);
        if let Some(rpc) = rpc_url {
            match ChainClient::new(vec![rpc.clone()], 30, 3) {
                Ok(chain_client) => {
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
        Some((rpc, addr)) => match WalletBalanceFetcher::new(rpc.clone(), &addr) {
            Ok(f) => {
                info!(wallet = %addr, "wallet balance fetcher enabled");
                Some(f)
            }
            Err(e) => {
                warn!(error = %e, "failed to init WalletBalanceFetcher; wallet sync disabled");
                None
            }
        },
        None => None,
    };

    info!(
        pairs = ?cli.pair,
        size = %trade_size,
        max_concurrent = cli.max_concurrent_executions,
        wallet_sync = wallet_fetcher.is_some(),
        balance_sync_secs = cli.balance_sync_interval_secs,
        "bot starting"
    );

    // Initial sync.
    sync_cex_balance(&exchange, &inventory).await;
    if let Some(ref f) = wallet_fetcher {
        sync_wallet_balance(f, &inventory).await;
    }

    let sync_interval = Duration::from_secs(cli.balance_sync_interval_secs.max(1));
    let mut last_sync = std::time::Instant::now();

    loop {
        if let Err(e) = tick(
            &cli.pair,
            trade_size,
            min_score,
            &mut generator,
            Arc::clone(&scorer),
            Arc::clone(&executor),
            Arc::clone(&queue),
        )
        .await
        {
            error!("tick error: {e}");
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }
        // Gate balance syncs by the configured interval — tight `--tick-ms`
        // loops must not hammer the exchange `/account` or RPC endpoints.
        if last_sync.elapsed() >= sync_interval {
            sync_cex_balance(&exchange, &inventory).await;
            if let Some(ref f) = wallet_fetcher {
                sync_wallet_balance(f, &inventory).await;
            }
            last_sync = std::time::Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(cli.tick_ms)).await;
    }
}

async fn tick(
    pairs: &[String],
    size: Decimal,
    min_score: Decimal,
    generator: &mut SignalGenerator<StubPriceSource>,
    scorer: Arc<Mutex<SignalScorer>>,
    executor: Arc<Executor>,
    queue: Arc<SignalQueue>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Fast path: skip the tick entirely when the breaker is open. The queue
    // worker also pre-flight-checks the breaker via `Executor::execute`, but
    // filtering here avoids burning cycles on signal generation we know we
    // won't act on.
    let cb = executor.circuit_breaker();
    {
        let mut cb_guard = cb.lock().await;
        if cb_guard.is_open() {
            let remaining = cb_guard.time_until_reset();
            warn!(remaining_s = remaining.as_secs(), "circuit breaker open");
            return Ok(());
        }
    }

    for pair in pairs {
        let maybe_signal = match generator.generate(pair, size).await {
            Ok(s) => s,
            Err(e) => {
                warn!(pair, "signal generation failed: {e}");
                continue;
            }
        };
        let mut signal = match maybe_signal {
            Some(s) => s,
            None => continue,
        };

        // Score + threshold. Acquire scorer lock briefly; don't hold across
        // the queue push below (push takes its own lock internally).
        let skews = generator_inventory_skews(generator).await;
        {
            let scorer_guard = scorer.lock().await;
            // TODO(S5-wiring): pass the CEX order book snapshot here so the
            // liquidity sub-score reflects real book depth. For now we pass
            // `None`, which triggers the scorer's `fallback_liquidity`.
            signal.score = scorer_guard.score(&signal, &skews, None);
        }

        if signal.score < min_score {
            info!(
                pair,
                spread_bps = %signal.spread_bps,
                score = %signal.score,
                "skipped: score below threshold"
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
        if !queue.push(signal).await {
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
    generator: &SignalGenerator<StubPriceSource>,
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

/// Resolves `(rpc_url, wallet_address)` from CLI first, then env vars.
/// Returns `None` when either is empty — caller skips wallet sync.
fn resolve_wallet_config(cli: &Cli) -> Option<(String, String)> {
    let rpc = if cli.eth_rpc_url.is_empty() {
        std::env::var("ETH_RPC_URL").unwrap_or_default()
    } else {
        cli.eth_rpc_url.clone()
    };
    let addr = if cli.wallet_address.is_empty() {
        std::env::var("WALLET_ADDRESS").unwrap_or_default()
    } else {
        cli.wallet_address.clone()
    };
    if rpc.is_empty() || addr.is_empty() {
        None
    } else {
        Some((rpc, addr))
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
    // every DEX-first DONE_PROFIT row in the PnL ledger.
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
}
