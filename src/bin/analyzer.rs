use std::env;

use peanut_internship_rust::chain::analyzer::analyze_transaction;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    let _ = dotenvy::dotenv();

    let args: Vec<String> = env::args().skip(1).collect();

    if args.is_empty() {
        eprintln!("Usage: cargo run --bin analyzer -- <tx_hash> [--rpc <URL>] [--format json]");
        std::process::exit(1);
    }

    let tx_hash = &args[0];
    let mut rpc_arg: Option<String> = None;
    let mut format_json = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--rpc" => {
                i += 1;
                rpc_arg = args.get(i).cloned();
            }
            "--format" => {
                i += 1;
                if args.get(i).map(|s| s.as_str()) == Some("json") {
                    format_json = true;
                }
            }
            _ => {}
        }
        i += 1;
    }

    let rpc_url = rpc_arg
        .or_else(|| env::var("MAINNET_RPC_URL").ok())
        .ok_or("missing RPC URL; provide --rpc or set MAINNET_RPC_URL")?;

    let result = analyze_transaction(&rpc_url, tx_hash).await?;

    if format_json {
        println!("{}", serde_json::to_string_pretty(&result.to_json())?);
    } else {
        println!("{}", result.to_text());
    }

    Ok(())
}
