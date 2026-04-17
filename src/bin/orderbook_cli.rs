use clap::Parser;
use rust_decimal::Decimal;

use peanut_internship_rust::exchange::{BinanceConfig, ExchangeClient, OrderBookAnalyzer};

#[derive(Parser)]
#[command(name = "orderbook")]
#[command(about = "Fetch and analyze Binance order books")]
struct Cli {
    symbol: String,

    #[arg(short, long, default_value = "20")]
    depth: u32,
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
        Ok(t) => println!("Connected to Binance testnet (server time: {t})"),
        Err(e) => {
            eprintln!("Connection check failed: {e}");
            std::process::exit(1);
        }
    }

    let ob = match client.fetch_order_book(&cli.symbol, cli.depth).await {
        Ok(ob) => ob,
        Err(e) => {
            eprintln!("Failed to fetch order book: {e}");
            std::process::exit(1);
        }
    };

    let analyzer = OrderBookAnalyzer::new(ob);
    let ob = analyzer.orderbook();

    println!();
    println!("╔══════════════════════════════════════════════════════╗");
    println!("║  {} Order Book Analysis", pad_right(&ob.symbol, 34));
    println!("║  Timestamp: {}", ob.timestamp);
    println!("╠══════════════════════════════════════════════════════╣");

    match ob.best_bid {
        Some((p, q)) => println!(
            "║  Best Bid:    ${} × {} {}",
            fmt_dec(p),
            fmt_dec(q),
            base_asset(&ob.symbol)
        ),
        None => println!("║  Best Bid:    N/A"),
    }
    match ob.best_ask {
        Some((p, q)) => println!(
            "║  Best Ask:    ${} × {} {}",
            fmt_dec(p),
            fmt_dec(q),
            base_asset(&ob.symbol)
        ),
        None => println!("║  Best Ask:    N/A"),
    }
    match ob.mid_price {
        Some(mp) => println!("║  Mid Price:   ${}", fmt_dec(mp)),
        None => println!("║  Mid Price:   N/A"),
    }
    println!(
        "║  Spread:      {} ({} bps)",
        match (ob.best_bid, ob.best_ask) {
            (Some((bid, _)), Some((ask, _))) => fmt_spread(ask - bid),
            _ => "N/A".into(),
        },
        ob.spread_bps.map(fmt_dec).unwrap_or_else(|| "N/A".into())
    );

    println!("╠══════════════════════════════════════════════════════╣");

    let bid_depth = analyzer
        .depth_at_bps("bid", Decimal::from(10))
        .unwrap_or_else(|e| {
            eprintln!("  Warning: bid depth calculation failed: {e}");
            Decimal::ZERO
        });
    let ask_depth = analyzer
        .depth_at_bps("ask", Decimal::from(10))
        .unwrap_or_else(|e| {
            eprintln!("  Warning: ask depth calculation failed: {e}");
            Decimal::ZERO
        });
    println!("║  Depth (within 10 bps):");
    println!(
        "║    Bids: {} {}",
        fmt_dec(bid_depth),
        base_asset(&ob.symbol)
    );
    println!(
        "║    Asks: {} {}",
        fmt_dec(ask_depth),
        base_asset(&ob.symbol)
    );

    let imb = analyzer.imbalance(10);
    let imb_label = if imb > 0.05 {
        "buy pressure"
    } else if imb < -0.05 {
        "sell pressure"
    } else {
        "balanced"
    };
    println!("║  Imbalance: {:+.2} ({})", imb, imb_label);

    println!("╠══════════════════════════════════════════════════════╣");

    for size in [Decimal::from(2), Decimal::from(10)] {
        let walk = analyzer.walk_the_book("buy", size).unwrap();
        println!(
            "║  Walk-the-book ({} {} buy):",
            fmt_dec(size),
            base_asset(&ob.symbol)
        );
        println!("║    Avg price:  ${}", fmt_dec(walk.avg_price));
        println!("║    Slippage:   {} bps", fmt_dec(walk.slippage_bps));
        println!("║    Levels:     {}", walk.levels_consumed);
        if !walk.fully_filled {
            println!("║    ⚠️  Insufficient liquidity");
        }
    }

    let eff_spread = analyzer
        .effective_spread(Decimal::from(2))
        .unwrap_or_else(|e| {
            eprintln!("  Warning: effective spread calculation failed: {e}");
            Decimal::ZERO
        });
    println!("╠══════════════════════════════════════════════════════╣");
    println!(
        "║  Effective spread (2 {} round-trip): {} bps",
        base_asset(&ob.symbol),
        fmt_dec(eff_spread)
    );
    println!("╚══════════════════════════════════════════════════════╝");
}

fn fmt_dec(d: Decimal) -> String {
    format!("{d:.2}")
}

fn fmt_spread(d: Decimal) -> String {
    format!("${d:.2}")
}

fn pad_right(s: &str, width: usize) -> String {
    let mut out = s.to_string();
    while out.len() < width {
        out.push(' ');
    }
    out
}

fn base_asset(symbol: &str) -> &str {
    symbol.split('/').next().unwrap_or("???")
}
