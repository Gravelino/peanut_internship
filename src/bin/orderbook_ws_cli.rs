use clap::Parser;
use peanut_internship_rust::exchange::OrderBookAnalyzer;
use peanut_internship_rust::exchange::config::BINANCE_TESTNET_WS_URL;
use peanut_internship_rust::exchange::ws::{DepthEvent, LocalOrderBook};

#[derive(Parser)]
#[command(name = "orderbook_ws")]
#[command(about = "Live WebSocket order book viewer")]
struct Cli {
    /// Trading pair (e.g. ETHUSDT)
    pair: String,

    #[arg(long, default_value = BINANCE_TESTNET_WS_URL)]
    ws_url: String,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let mut rx =
        match peanut_internship_rust::exchange::ws::subscribe_depth_stream(&cli.ws_url, &cli.pair)
            .await
        {
            Ok(rx) => rx,
            Err(e) => {
                eprintln!("Failed to connect to depth stream: {e}");
                std::process::exit(1);
            }
        };

    let mut book = LocalOrderBook::new(&cli.pair);

    println!(
        "Subscribed to {} depth stream. Press Ctrl+C to stop.",
        cli.pair
    );
    println!();

    while let Some(event) = rx.recv().await {
        match event {
            DepthEvent::Snapshot(snap) => {
                book.apply_snapshot(snap);
            }
            DepthEvent::Update(update) => {
                use peanut_internship_rust::exchange::ws::SequenceStatus;
                match book.apply_update(update) {
                    SequenceStatus::Applied => {}
                    SequenceStatus::Stale => continue,
                    SequenceStatus::NeedsReconnect => {
                        eprintln!("Sequence gap detected, reconnecting...");
                        break;
                    }
                }
            }
        }

        let ob = book.snapshot();
        let analyzer = OrderBookAnalyzer::new(ob);

        let best_bid = analyzer
            .orderbook()
            .best_bid
            .map(|(p, _)| format!("{p}"))
            .unwrap_or_else(|| "N/A".into());
        let best_ask = analyzer
            .orderbook()
            .best_ask
            .map(|(p, _)| format!("{p}"))
            .unwrap_or_else(|| "N/A".into());
        let mid = analyzer
            .orderbook()
            .mid_price
            .map(|m| format!("{m}"))
            .unwrap_or_else(|| "N/A".into());
        let spread = analyzer
            .orderbook()
            .spread_bps
            .map(|s| format!("{s:.2} bps"))
            .unwrap_or_else(|| "N/A".into());
        let imb = analyzer.imbalance(10);

        let arrow = if imb > 0.1 {
            "▲"
        } else if imb < -0.1 {
            "▼"
        } else {
            "─"
        };

        println!(
            "[{}] {} | bid: {} | ask: {} | mid: {} | spread: {} | imbalance: {arrow}{imb:.2}",
            cli.pair,
            chrono::Utc::now().format("%H:%M:%S UTC"),
            best_bid,
            best_ask,
            mid,
            spread,
        );
    }
}
