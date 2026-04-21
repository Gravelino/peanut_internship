use std::collections::HashMap;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};
use rust_decimal::Decimal;

use peanut_internship_rust::exchange::{BinanceConfig, BybitConfig, ExchangeClient};
use peanut_internship_rust::inventory::pnl::PnLEngine;
use peanut_internship_rust::inventory::tracker::InventoryTracker;
use peanut_internship_rust::inventory::types::{
    ExecutorConfig, RebalanceStatus, RebalanceStep, Venue,
};
use peanut_internship_rust::inventory::{
    RebalanceExecutor, RebalancePlanner, WalletBalanceFetcher,
};

#[derive(Parser)]
#[command(name = "rebalancer")]
#[command(about = "Check inventory skew, plan, and execute rebalances")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Args)]
struct SharedArgs {
    #[arg(long)]
    wallet_address: Option<String>,

    #[arg(long, default_value = "http://127.0.0.1:8545")]
    rpc_url: String,

    /// Include Bybit testnet (needs BYBIT_API_KEY + BYBIT_SECRET in .env)
    #[arg(long)]
    with_bybit: bool,
}

#[derive(Args)]
struct ExecuteArgs {
    /// Asset to rebalance (e.g. ETH, USDT)
    asset: String,

    /// Quote asset for trading (default: USDT)
    #[arg(long, default_value = "USDT")]
    quote: String,

    /// Dry-run mode: log steps but do not place orders
    #[arg(long)]
    dry_run: bool,

    /// Maximum slippage in bps per trade (default: 50 = 0.5%)
    #[arg(long, default_value_t = 50)]
    max_slippage_bps: u64,

    #[command(flatten)]
    shared: SharedArgs,
}

#[derive(Subcommand)]
enum Commands {
    Check(SharedArgs),
    Plan {
        asset: String,

        #[command(flatten)]
        shared: SharedArgs,
    },
    PlanExec(ExecuteArgs),
    Execute(ExecuteArgs),
    ExecuteAll {
        /// Quote asset for trading (default: USDT)
        #[arg(long, default_value = "USDT")]
        quote: String,

        /// Dry-run mode
        #[arg(long)]
        dry_run: bool,

        /// Maximum slippage in bps per trade
        #[arg(long, default_value_t = 50)]
        max_slippage_bps: u64,

        #[command(flatten)]
        shared: SharedArgs,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Check(shared) => cmd_check(&shared).await,
        Commands::Plan { asset, shared } => cmd_plan(&asset, &shared).await,
        Commands::PlanExec(args) => cmd_plan_exec(&args).await,
        Commands::Execute(args) => cmd_execute(&args).await,
        Commands::ExecuteAll {
            quote,
            dry_run,
            max_slippage_bps,
            shared,
        } => cmd_execute_all(&quote, dry_run, max_slippage_bps, &shared).await,
    }
}

async fn cmd_check(shared: &SharedArgs) {
    let tracker = match build_tracker(shared).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Failed to fetch balances: {e}");
            std::process::exit(1);
        }
    };
    let planner = RebalancePlanner::new(tracker, 30.0);

    println!("Inventory Skew Report");
    println!("═══════════════════════════════════════════");

    let skews = planner.tracker().get_skews();
    let focus_assets = ["ETH", "BTC", "USDT", "USDC", "FDUSD", "BNB"];
    let focused: Vec<_> = skews
        .iter()
        .filter(|s| focus_assets.contains(&s.asset.as_str()))
        .collect();

    let display_skews = if focused.is_empty() {
        &skews.iter().collect::<Vec<_>>()
    } else {
        &focused
    };

    for skew in display_skews {
        let has_real_balance = skew.venues.values().any(|v| v.amount > Decimal::ZERO);
        if !has_real_balance {
            continue;
        }
        println!("Asset: {}", skew.asset);
        for (venue_name, vs) in &skew.venues {
            if vs.amount == Decimal::ZERO && skew.venues.len() > 2 {
                continue;
            }
            println!(
                "  {}:  {} {}  ({:.0}%)   {} deviation: {:+.0}%",
                venue_name,
                vs.amount,
                skew.asset,
                vs.pct,
                if vs.deviation_pct.abs() > 30.0 {
                    "⚠️"
                } else {
                    "  "
                },
                vs.deviation_pct
            );
        }
        let status = if skew.needs_rebalance {
            "⚠️  NEEDS REBALANCE"
        } else {
            "✅  OK"
        };
        println!("  Status: {}", status);
        println!();
    }
    println!("═══════════════════════════════════════════");
}

async fn cmd_plan(asset: &str, shared: &SharedArgs) {
    let tracker = match build_tracker(shared).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Failed to fetch balances: {e}");
            std::process::exit(1);
        }
    };
    let planner = RebalancePlanner::new(tracker, 30.0);

    let plans = planner.plan(asset);
    if plans.is_empty() {
        println!("No rebalance needed for {}", asset);
        return;
    }

    println!("Rebalance Plan: {}", asset);
    println!("───────────────────────────────────────────");
    for (i, plan) in plans.iter().enumerate() {
        println!("Transfer {}:", i + 1);
        println!("  From:     {}", plan.from_venue);
        println!("  To:       {}", plan.to_venue);
        println!("  Amount:   {} {}", plan.amount, asset);
        println!("  Fee:      {} {}", plan.estimated_fee, asset);
        println!("  ETA:      ~{} min", plan.estimated_time_min);
        println!("  Net amt:  {} {}", plan.net_amount(), asset);
        println!();
    }

    let prices = HashMap::from([
        ("ETH".into(), Decimal::from(2000)),
        ("USDT".into(), Decimal::ONE),
        ("USDC".into(), Decimal::ONE),
    ]);
    let cost = planner.estimate_cost(&plans, &prices);
    println!("Estimated total cost: ${:.2}", cost.total_fees_usd);
}

async fn cmd_plan_exec(args: &ExecuteArgs) {
    let tracker = match build_tracker(&args.shared).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Failed to fetch balances: {e}");
            std::process::exit(1);
        }
    };
    let planner = RebalancePlanner::new(tracker, 30.0);
    let max_slippage = Decimal::from(args.max_slippage_bps);
    let steps = planner.plan_executable(&args.asset, &args.quote, max_slippage);

    if steps.is_empty() {
        println!("No executable rebalance steps for {}", args.asset);
        return;
    }

    println!("Executable Rebalance Steps: {}", args.asset);
    println!("───────────────────────────────────────────");
    for (i, step) in steps.iter().enumerate() {
        match step {
            RebalanceStep::Trade(t) => {
                println!("Step {} [TRADE]:", i + 1);
                println!("  Venue:    {}", t.venue);
                println!("  Symbol:   {}", t.symbol);
                println!("  Side:     {}", t.side);
                println!("  Amount:   {} {}", t.amount, t.base_asset);
                println!("  Max slip: {} bps", t.max_slippage_bps);
            }
            RebalanceStep::Withdraw(w) => {
                println!("Step {} [WITHDRAW]:", i + 1);
                println!("  From:     {}", w.from_venue);
                println!("  To:       {}", w.to_venue);
                println!("  Amount:   {} {}", w.amount, w.asset);
                println!("  Fee:      {} {}", w.fee, w.asset);
            }
        }
        println!();
    }
    println!("Use `execute {}` to run these steps.", args.asset);
}

async fn cmd_execute(args: &ExecuteArgs) {
    let shared = &args.shared;
    let tracker = match build_tracker(shared).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Failed to fetch balances: {e}");
            std::process::exit(1);
        }
    };

    let planner = RebalancePlanner::new(tracker, 30.0);
    let max_slippage = Decimal::from(args.max_slippage_bps);
    let steps = planner.plan_executable(&args.asset, &args.quote, max_slippage);

    if steps.is_empty() {
        println!("No executable rebalance steps for {}", args.asset);
        return;
    }

    let clients = match build_clients(shared).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to create exchange clients: {e}");
            std::process::exit(1);
        }
    };

    let tracker_arc = Arc::new(tokio::sync::Mutex::new(planner.tracker().clone()));
    let pnl_arc = Arc::new(tokio::sync::Mutex::new(PnLEngine::new()));

    let config = ExecutorConfig {
        dry_run: args.dry_run,
        max_slippage_bps: max_slippage,
        ..ExecutorConfig::default()
    };

    let executor = RebalanceExecutor::new(clients, tracker_arc, pnl_arc, config);
    let results = executor.execute_plan(&steps).await;

    print_results(&results);

    if args.dry_run {
        println!();
        println!("[DRY RUN] No orders were placed. Remove --dry-run to execute.");
    }
}

async fn cmd_execute_all(quote: &str, dry_run: bool, max_slippage_bps: u64, shared: &SharedArgs) {
    let tracker = match build_tracker(shared).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Failed to fetch balances: {e}");
            std::process::exit(1);
        }
    };

    let planner = RebalancePlanner::new(tracker, 30.0);
    let max_slippage = Decimal::from(max_slippage_bps);
    let all_steps = planner.plan_executable_all(quote, max_slippage);

    if all_steps.is_empty() {
        println!("No assets need rebalancing.");
        return;
    }

    let clients = match build_clients(shared).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to create exchange clients: {e}");
            std::process::exit(1);
        }
    };

    let tracker_arc = Arc::new(tokio::sync::Mutex::new(planner.tracker().clone()));
    let pnl_arc = Arc::new(tokio::sync::Mutex::new(PnLEngine::new()));

    let config = ExecutorConfig {
        dry_run,
        max_slippage_bps: max_slippage,
        ..ExecutorConfig::default()
    };

    let executor = RebalanceExecutor::new(clients, tracker_arc, pnl_arc, config);

    for (asset, steps) in &all_steps {
        println!("Rebalancing {}...", asset);
        let results = executor.execute_plan(steps).await;
        print_results(&results);
        println!();
    }

    if dry_run {
        println!("[DRY RUN] No orders were placed. Remove --dry-run to execute.");
    }
}

fn print_results(results: &[peanut_internship_rust::inventory::types::RebalanceResult]) {
    for result in results {
        let status_icon = match result.status {
            RebalanceStatus::Executed => "✅",
            RebalanceStatus::PartiallyFilled => "⚠️",
            RebalanceStatus::DryRun => "🔍",
            _ => "❌",
        };
        println!(
            "  {} {:?}: filled={} avg_price={} fee={}{} status={:?}",
            status_icon,
            match &result.step {
                RebalanceStep::Trade(t) => format!("TRADE {} {} {}", t.venue, t.side, t.symbol),
                RebalanceStep::Withdraw(w) => format!("WITHDRAW {}→{}", w.from_venue, w.to_venue),
            },
            result.amount_filled,
            result.avg_price,
            result.fee,
            if result.fee_asset.is_empty() {
                String::new()
            } else {
                format!(" {}", result.fee_asset)
            },
            result.status,
        );
        if let Some(id) = &result.order_id {
            println!("     order_id={id}");
        }
    }
}

async fn build_tracker(
    shared: &SharedArgs,
) -> Result<InventoryTracker, peanut_internship_rust::exchange::errors::ExchangeError> {
    let mut venues = vec![Venue::Binance];
    if shared.with_bybit {
        venues.push(Venue::Bybit);
    }
    venues.push(Venue::Wallet);
    let mut tracker = InventoryTracker::new(venues);

    let config = BinanceConfig::from_env()?;
    let client = ExchangeClient::new(config)?;
    client.health_check().await?;
    let balances = client.fetch_balance().await?;
    tracker.update_from_cex(Venue::Binance, balances);

    if shared.with_bybit {
        if let Ok(bybit_config) = BybitConfig::from_env()
            && let Ok(bybit_client) = ExchangeClient::bybit(bybit_config)
            && let Ok(bybit_balances) = bybit_client.fetch_balance().await
        {
            tracker.update_from_cex(Venue::Bybit, bybit_balances);
            println!("Loaded Bybit balances.");
        } else {
            eprintln!("Warning: BYBIT_API_KEY/SECRET not set, skipping Bybit.");
        }
    }

    if let Some(addr) = &shared.wallet_address {
        println!("Fetching on-chain wallet balances for {addr}...");
        match WalletBalanceFetcher::new(shared.rpc_url.clone(), addr) {
            Ok(fetcher) => match fetcher.fetch_balances().await {
                Ok(wallet_bals) => {
                    if wallet_bals.is_empty() {
                        println!("  (no balances found or RPC unavailable)");
                    }
                    tracker.update_from_wallet(Venue::Wallet, wallet_bals);
                }
                Err(e) => {
                    eprintln!("Warning: Could not fetch wallet balances: {e}");
                    tracker.update_from_wallet(Venue::Wallet, default_wallet_bals());
                }
            },
            Err(e) => {
                eprintln!("Warning: Invalid wallet address: {e}");
                tracker.update_from_wallet(Venue::Wallet, default_wallet_bals());
            }
        }
    } else {
        tracker.update_from_wallet(Venue::Wallet, default_wallet_bals());
    }

    Ok(tracker)
}

async fn build_clients(
    shared: &SharedArgs,
) -> Result<HashMap<Venue, ExchangeClient>, peanut_internship_rust::exchange::errors::ExchangeError>
{
    let mut clients = HashMap::new();

    let config = BinanceConfig::from_env()?;
    clients.insert(Venue::Binance, ExchangeClient::new(config)?);

    if shared.with_bybit
        && let Ok(bybit_config) = BybitConfig::from_env()
        && let Ok(client) = ExchangeClient::bybit(bybit_config)
    {
        clients.insert(Venue::Bybit, client);
    }

    Ok(clients)
}

fn default_wallet_bals() -> HashMap<String, Decimal> {
    let mut bals = HashMap::new();
    bals.insert("ETH".into(), Decimal::ZERO);
    bals.insert("USDT".into(), Decimal::ZERO);
    bals
}
