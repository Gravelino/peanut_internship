use chrono::{TimeZone, Utc};
use clap::Parser;
use rust_decimal::Decimal;

use peanut_internship_rust::exchange::{BinanceConfig, ExchangeClient, PriceOracle};
use peanut_internship_rust::inventory::PnLChartExporter;
use peanut_internship_rust::inventory::pnl::{ArbRecord, PnLEngine, TradeLeg};
use peanut_internship_rust::inventory::types::Venue;

/// Formats a Unix-millisecond timestamp as a human-readable UTC string.
fn format_unix_millis(millis: u64) -> String {
    chrono::DateTime::from_timestamp_millis(millis as i64)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| format!("{millis} (raw epoch ms)"))
}

#[derive(Parser)]
#[command(name = "pnl")]
#[command(about = "PnL engine for tracking arbitrage trade performance")]
struct Cli {
    #[arg(long)]
    chart: Option<String>,

    #[arg(long, default_value = "html")]
    chart_format: String,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let config = match BinanceConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Config error: {e}");
            eprintln!("Set BINANCE_TESTNET_API_KEY and BINANCE_TESTNET_SECRET in .env");
            std::process::exit(1);
        }
    };

    let client = match ExchangeClient::new(config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Client init error: {e}");
            std::process::exit(1);
        }
    };

    match client.health_check().await {
        Ok(t) => println!(
            "Connected to Binance testnet (server time: {})",
            format_unix_millis(t)
        ),
        Err(e) => {
            eprintln!("Connection check failed: {e}");
            std::process::exit(1);
        }
    }

    let pairs = ["ETH/USDT", "BTC/USDT"];
    let mut engine = PnLEngine::new();
    let mut total_fetched = 0;

    for pair in pairs {
        match client.fetch_my_trades(pair, 50).await {
            Ok(trades) => {
                if trades.is_empty() {
                    continue;
                }
                total_fetched += trades.len();

                let oracle = PriceOracle::new(client.config().base_url.clone());
                let eth_price = match oracle.fetch_binance_ticker("ETH/USDT").await {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!(
                            "Warning: Could not fetch ETH price from oracle: {e}, using 2000 as fallback"
                        );
                        Decimal::from(2000)
                    }
                };

                let mut buy_legs: Vec<&peanut_internship_rust::exchange::types::MyTrade> =
                    Vec::new();
                let mut sell_legs: Vec<&peanut_internship_rust::exchange::types::MyTrade> =
                    Vec::new();

                for t in &trades {
                    match t.side.as_str() {
                        "buy" => buy_legs.push(t),
                        "sell" => sell_legs.push(t),
                        _ => {}
                    }
                }

                let paired_count = buy_legs.len().min(sell_legs.len());
                for i in 0..paired_count {
                    let buy = buy_legs[i];
                    let sell = sell_legs[i];

                    let buy_fee_usd = if buy.fee_asset == "USDT" {
                        buy.fee
                    } else {
                        buy.fee * eth_price
                    };
                    let sell_fee_usd = if sell.fee_asset == "USDT" {
                        sell.fee
                    } else {
                        sell.fee * eth_price
                    };

                    let buy_ts = Utc
                        .timestamp_millis_opt(buy.timestamp as i64)
                        .single()
                        .expect("valid buy timestamp from Binance");
                    let sell_ts = Utc
                        .timestamp_millis_opt(sell.timestamp as i64)
                        .single()
                        .expect("valid sell timestamp from Binance");
                    let trade = ArbRecord {
                        id: format!("{}_{}", pair.replace('/', ""), i),
                        timestamp: buy_ts,
                        buy_leg: TradeLeg {
                            id: buy.id.clone(),
                            timestamp: buy_ts,
                            venue: Venue::Binance,
                            symbol: pair.to_string(),
                            side: "buy".into(),
                            amount: buy.qty,
                            price: buy.price,
                            fee: buy_fee_usd,
                            fee_asset: "USDT".into(),
                        },
                        sell_leg: TradeLeg {
                            id: sell.id.clone(),
                            timestamp: sell_ts,
                            venue: Venue::Binance,
                            symbol: pair.to_string(),
                            side: "sell".into(),
                            amount: sell.qty,
                            price: sell.price,
                            fee: sell_fee_usd,
                            fee_asset: "USDT".into(),
                        },
                        gas_cost_usd: Decimal::ZERO,
                    };
                    engine.record(trade);
                }
            }
            Err(e) => {
                eprintln!("Warning: Could not fetch trades for {}: {e}", pair);
            }
        }
    }

    let summary = engine.summary();

    println!();
    println!("PnL Summary");
    println!("═══════════════════════════════════════════");
    if summary.total_trades == 0 {
        println!("No trades found on Binance testnet account.");
        println!();
        println!("To create test trades, place orders via:");
        println!("  cargo run --bin orderbook_cli -- ETH/USDT");
        println!("  Then buy and sell to generate trade history.");
        println!();
        println!("Fetched {} raw trades from API.", total_fetched);
    } else {
        println!(
            "Trades fetched:     {} raw / {} arb pairs",
            total_fetched, summary.total_trades
        );
        println!("Win Rate:            {:.1}%", summary.win_rate * 100.0);
        println!("Total PnL:           ${:.2}", summary.total_pnl_usd);
        println!("Total Fees:          ${:.2}", summary.total_fees_usd);
        println!("Avg PnL/Trade:       ${:.2}", summary.avg_pnl_per_trade);
        println!("Avg PnL (bps):       {} bps", summary.avg_pnl_bps.round());
        println!("Best Trade:          ${:.2}", summary.best_trade_pnl);
        println!("Worst Trade:         ${:.2}", summary.worst_trade_pnl);
        println!("Total Notional:      ${:.2}", summary.total_notional);
        println!("Sharpe Estimate:     {:.2}", summary.sharpe_estimate);
        println!();

        println!("Recent Trades:");
        let recent = engine.recent(10);
        for t in &recent {
            let icon = if t.profitable { "✅" } else { "❌" };
            println!(
                "  {} {} {}/{}  ${:+.2} ({:.1} bps) {}",
                t.timestamp.format("%H:%M UTC"),
                t.symbol,
                t.buy_venue,
                t.sell_venue,
                t.net_pnl,
                t.net_pnl_bps,
                icon,
            );
        }

        let csv_path = std::env::temp_dir().join("pnl_export.csv");
        if let Ok(()) = engine.export_csv(csv_path.to_str().unwrap()) {
            println!("\nCSV exported to: {}", csv_path.display());
        }

        if let Some(chart_path) = &cli.chart {
            let trades = engine.trades();
            let result = match cli.chart_format.as_str() {
                "svg" => PnLChartExporter::export_svg(trades, chart_path),
                _ => PnLChartExporter::export_html(trades, chart_path),
            };
            match result {
                Ok(()) => println!("Chart exported to: {}", chart_path),
                Err(e) => eprintln!("Chart export failed: {e}"),
            }
        }
    }
}
