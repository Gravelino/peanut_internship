use clap::{Parser, Subcommand};
use rust_decimal::Decimal;

use peanut_internship_rust::exchange::{BinanceConfig, ExchangeClient};

#[derive(Parser)]
#[command(name = "trader")]
#[command(about = "Place and manage orders on Binance testnet")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Buy {
        symbol: String,
        #[arg(short, long)]
        amount: String,
        #[arg(short, long)]
        price: Option<String>,
        #[arg(long)]
        gtc: bool,
    },
    Sell {
        symbol: String,
        #[arg(short, long)]
        amount: String,
        #[arg(short, long)]
        price: Option<String>,
        #[arg(long)]
        gtc: bool,
    },
    Status {
        symbol: String,
        #[arg(short, long)]
        order_id: String,
    },
    Cancel {
        symbol: String,
        #[arg(short, long)]
        order_id: String,
    },
    Fees {
        symbol: String,
    },
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
            eprintln!("Connection failed: {e}");
            std::process::exit(1);
        }
    }

    match cli.command {
        Commands::Buy {
            symbol,
            amount,
            price,
            gtc,
        } => {
            let qty: f64 = amount.parse().unwrap_or(0.0);
            if qty <= 0.0 {
                eprintln!("Amount must be positive");
                std::process::exit(1);
            }
            match price {
                Some(p) => {
                    let px: f64 = p.parse().unwrap_or(0.0);
                    if gtc {
                        println!("Placing LIMIT GTC BUY {} @ ${}", symbol, px);
                        match client.create_limit_gtc_order(&symbol, "BUY", qty, px).await {
                            Ok(result) => display_order(result),
                            Err(e) => eprintln!("Order failed: {e}"),
                        }
                    } else {
                        println!("Placing LIMIT IOC BUY {} @ ${}", symbol, px);
                        match client.create_limit_ioc_order(&symbol, "BUY", qty, px).await {
                            Ok(result) => display_order(result),
                            Err(e) => eprintln!("Order failed: {e}"),
                        }
                    }
                }
                None => {
                    println!("Placing MARKET BUY {} qty={}", symbol, qty);
                    match client.create_market_order(&symbol, "BUY", qty).await {
                        Ok(result) => display_order(result),
                        Err(e) => eprintln!("Order failed: {e}"),
                    }
                }
            }
        }
        Commands::Sell {
            symbol,
            amount,
            price,
            gtc,
        } => {
            let qty: f64 = amount.parse().unwrap_or(0.0);
            if qty <= 0.0 {
                eprintln!("Amount must be positive");
                std::process::exit(1);
            }
            match price {
                Some(p) => {
                    let px: f64 = p.parse().unwrap_or(0.0);
                    if gtc {
                        println!("Placing LIMIT GTC SELL {} @ ${}", symbol, px);
                        match client
                            .create_limit_gtc_order(&symbol, "SELL", qty, px)
                            .await
                        {
                            Ok(result) => display_order(result),
                            Err(e) => eprintln!("Order failed: {e}"),
                        }
                    } else {
                        println!("Placing LIMIT IOC SELL {} @ ${}", symbol, px);
                        match client
                            .create_limit_ioc_order(&symbol, "SELL", qty, px)
                            .await
                        {
                            Ok(result) => display_order(result),
                            Err(e) => eprintln!("Order failed: {e}"),
                        }
                    }
                }
                None => {
                    println!("Placing MARKET SELL {} qty={}", symbol, qty);
                    match client.create_market_order(&symbol, "SELL", qty).await {
                        Ok(result) => display_order(result),
                        Err(e) => eprintln!("Order failed: {e}"),
                    }
                }
            }
        }
        Commands::Status { symbol, order_id } => {
            println!("Checking order {} on {}", order_id, symbol);
            match client.fetch_order_status(&order_id, &symbol).await {
                Ok(result) => display_order(result),
                Err(e) => eprintln!("Status check failed: {e}"),
            }
        }
        Commands::Cancel { symbol, order_id } => {
            println!("Cancelling order {} on {}", order_id, symbol);
            match client.cancel_order(&order_id, &symbol).await {
                Ok(result) => display_order(result),
                Err(e) => eprintln!("Cancel failed: {e}"),
            }
        }
        Commands::Fees { symbol } => match client.get_trading_fees(&symbol).await {
            Ok(fees) => {
                println!("Trading fees for {}:", symbol);
                println!("  Maker: {}%", fees.maker * Decimal::from(100));
                println!("  Taker: {}%", fees.taker * Decimal::from(100));
            }
            Err(e) => eprintln!("Fee check failed: {e}"),
        },
    }
}

fn display_order(result: peanut_internship_rust::exchange::types::OrderResult) {
    println!();
    println!("Order Result:");
    println!("  ID:          {}", result.id);
    println!("  Symbol:      {}", result.symbol);
    println!("  Side:        {}", result.side);
    println!("  Type:        {}", result.order_type);
    println!("  Status:      {}", result.status);
    println!("  Requested:   {}", result.amount_requested);
    println!("  Filled:      {}", result.amount_filled);
    println!("  Avg Price:   ${}", result.avg_fill_price);
    println!("  Fee:         {} {}", result.fee, result.fee_asset);
    if result.time_in_force != "GTC" && !result.time_in_force.is_empty() {
        println!("  TiF:         {}", result.time_in_force);
    }
}
