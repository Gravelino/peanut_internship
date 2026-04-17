use std::collections::HashMap;

use clap::{Args, Parser, Subcommand};
use rust_decimal::Decimal;

use peanut_internship_rust::exchange::{BinanceConfig, ExchangeClient};
use peanut_internship_rust::inventory::{
    InventoryTracker, RebalancePlanner, Venue, WalletBalanceFetcher,
};

#[derive(Parser)]
#[command(name = "rebalancer")]
#[command(about = "Check inventory skew and plan rebalances")]
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
}

#[derive(Subcommand)]
enum Commands {
    Check(SharedArgs),
    Plan {
        asset: String,

        #[command(flatten)]
        shared: SharedArgs,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Check(shared) => {
            let tracker = match build_tracker(shared.wallet_address.as_deref(), &shared.rpc_url)
                .await
            {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("Failed to fetch Binance balances: {e}");
                    eprintln!(
                        "Check your .env has valid BINANCE_TESTNET_API_KEY and BINANCE_TESTNET_SECRET"
                    );
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
        Commands::Plan { asset, shared } => {
            let tracker =
                match build_tracker(shared.wallet_address.as_deref(), &shared.rpc_url).await {
                    Ok(t) => t,
                    Err(e) => {
                        eprintln!("Failed to fetch Binance balances: {e}");
                        std::process::exit(1);
                    }
                };
            let planner = RebalancePlanner::new(tracker, 30.0);

            let plans = planner.plan(&asset);
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
    }
}

async fn build_tracker(
    wallet_address: Option<&str>,
    rpc_url: &str,
) -> Result<InventoryTracker, peanut_internship_rust::exchange::errors::ExchangeError> {
    let mut tracker = InventoryTracker::new(vec![Venue::Binance, Venue::Wallet]);

    let config = BinanceConfig::from_env()?;
    let client = ExchangeClient::new(config)?;

    client.health_check().await?;
    let balances = client.fetch_balance().await?;
    tracker.update_from_cex(Venue::Binance, balances);

    if let Some(addr) = wallet_address {
        println!("Fetching on-chain wallet balances for {}...", addr);
        match WalletBalanceFetcher::new(rpc_url.to_string(), addr) {
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

fn default_wallet_bals() -> HashMap<String, Decimal> {
    let mut bals = HashMap::new();
    bals.insert("ETH".into(), Decimal::ZERO);
    bals.insert("USDT".into(), Decimal::ZERO);
    bals
}
