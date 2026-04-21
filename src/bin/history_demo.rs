use peanut_internship_rust::{
    Address, ChainClient, HistoricalImpactAnalyzer, PricingEngine, Token,
};
use std::env;

const PAIR_WETH_USDC: &str = "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Historical Price Impact Analysis Demo ===\n");

    demo_local_history()?;

    println!("\n--- Live historical impact tracking ---");
    let rpc_url = env::var("MAINNET_RPC_URL")
        .or_else(|_| env::var("SEPOLIA_RPC_URL"))
        .map_err(|_| "MAINNET_RPC_URL or SEPOLIA_RPC_URL is required")?;
    let ws_url =
        env::var("WS_RPC_URL").unwrap_or_else(|_| "wss://ethereum-rpc.publicnode.com".to_string());

    let client = ChainClient::new(vec![rpc_url.clone()], 3, 1)?;
    let mut engine = PricingEngine::new(client, &rpc_url, &ws_url)?;

    println!("Loading pool…");
    let addr = Address::new(PAIR_WETH_USDC)?;
    engine.load_pools(std::slice::from_ref(&addr)).await?;
    let pool = &engine.pools()[&addr];
    println!(
        "Loaded pool: {} (r0={}, r1={})",
        addr, pool.reserve0, pool.reserve1
    );

    let weth = pool.token0.clone();
    let usdc = pool.token1.clone();
    let mut analyzer = HistoricalImpactAnalyzer::new(weth.clone(), usdc.clone());

    println!("Starting price feed on {ws_url}…");
    let mut rx = engine.start_price_feed(&ws_url).await?;
    println!("Collecting observations (Ctrl+C to stop)…\n");

    let max_ticks: usize = env::var("MAX_TICKS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);

    let mut count = 0;
    while let Some(tick) = rx.recv().await {
        if tick.token_in != weth.address || tick.token_out != usdc.address {
            continue;
        }

        match analyzer.record_tick(&tick) {
            Ok(()) => {
                count += 1;
                let obs = &analyzer.observations()[analyzer.observations().len() - 1];
                print!(
                    "[blk={}] price={}  impacts:",
                    tick.block_number,
                    tick.price.round_dp(2),
                );
                for si in &obs.impacts {
                    print!(" {}bps={:.4}%", si.size_bps, si.price_impact_pct);
                }
                println!();

                if count >= max_ticks {
                    break;
                }
            }
            Err(e) => eprintln!("Record error: {e}"),
        }
    }

    if !analyzer.observations().is_empty() {
        let summary = analyzer.summarize();
        println!("\n=== Impact Summary ===");
        println!("Observations: {}", summary.num_observations);
        println!("Price change: {:.4}%", summary.price_change_pct.round_dp(4));
        println!("Max impact observed: {:.4}%", summary.max_impact_observed);
        println!("\nAvg impact by trade size:");
        for stat in &summary.avg_impact_by_size {
            println!(
                "  {}bps of reserve: avg={:.4}%  min={:.4}%  max={:.4}%",
                stat.size_bps,
                stat.avg_impact_pct.round_dp(4),
                stat.min_impact_pct.round_dp(4),
                stat.max_impact_pct.round_dp(4),
            );
        }
    }

    Ok(())
}

fn demo_local_history() -> Result<(), Box<dyn std::error::Error>> {
    println!("--- Local simulation with synthetic reserve changes ---\n");

    let weth = Token {
        address: Address::new("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")?,
        symbol: "WETH".into(),
        decimals: 18,
    };
    let usdc = Token {
        address: Address::new("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")?,
        symbol: "USDC".into(),
        decimals: 18,
    };

    let mut analyzer = HistoricalImpactAnalyzer::new(weth.clone(), usdc.clone());

    let reserves = [
        (
            10_000_000_000_000_000_000_000u128,
            20_000_000_000_000_000_000_000u128,
        ),
        (
            9_500_000_000_000_000_000_000u128,
            21_000_000_000_000_000_000_000u128,
        ),
        (
            8_000_000_000_000_000_000_000u128,
            25_000_000_000_000_000_000_000u128,
        ),
        (
            11_000_000_000_000_000_000_000u128,
            19_000_000_000_000_000_000_000u128,
        ),
        (
            10_000_000_000_000_000_000_000u128,
            20_000_000_000_000_000_000_000u128,
        ),
    ];

    for (i, (r_in, r_out)) in reserves.iter().enumerate() {
        analyzer.record_manual(
            Address::new("0x1000000000000000000000000000000000000000")?,
            *r_in,
            *r_out,
            (18_000_000 + i) as u64,
            1_700_000_000_000u64 + i as u64 * 12,
        )?;
    }

    println!("Recorded {} observations", analyzer.observation_count());
    println!();

    for obs in analyzer.observations() {
        print!(
            "blk={}: price={:.2}  impact:",
            obs.block_number,
            obs.price.round_dp(2)
        );
        for si in &obs.impacts {
            print!(
                " {}bps={:.4}%",
                si.size_bps,
                si.price_impact_pct.round_dp(4)
            );
        }
        println!();
    }

    let summary = analyzer.summarize();
    println!("\nSummary:");
    println!(
        "  Price change: {:.4}%",
        summary.price_change_pct.round_dp(4)
    );
    println!(
        "  Max impact:   {:.4}%",
        summary.max_impact_observed.round_dp(4)
    );
    for stat in &summary.avg_impact_by_size {
        println!(
            "  {}bps: avg={:.4}% min={:.4}% max={:.4}%",
            stat.size_bps,
            stat.avg_impact_pct.round_dp(4),
            stat.min_impact_pct.round_dp(4),
            stat.max_impact_pct.round_dp(4),
        );
    }

    Ok(())
}
