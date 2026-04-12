use std::env;

use ethers::providers::{Http, Middleware, Provider};
use peanut_internship_rust::{Address, BlockId, ChainClient, WalletManager};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    let _ = dotenvy::dotenv();

    let wallet = WalletManager::from_env("PRIVATE_KEY")?;
    let wallet_address = wallet.address();
    let address = Address::new(&wallet_address)?;

    println!("Environment check");
    println!("=================");
    println!("Wallet address: {wallet_address}");

    let sepolia_url = required_env("SEPOLIA_RPC_URL")?;
    println!("Sepolia RPC: configured");
    run_chain_checks("Sepolia", &sepolia_url, &address).await?;

    match env::var("MAINNET_RPC_URL") {
        Ok(mainnet_url) if !mainnet_url.trim().is_empty() => {
            println!("\nMainnet RPC: configured");
            run_chain_checks("Mainnet", &mainnet_url, &address).await?;
        }
        _ => {
            println!("\nMainnet RPC: not configured (skipped)");
        }
    }

    Ok(())
}

async fn run_chain_checks(
    name: &str,
    rpc_url: &str,
    address: &Address,
) -> Result<(), Box<dyn std::error::Error>> {
    let chain_client = ChainClient::new(vec![rpc_url.to_string()], 20, 2)?;

    let provider = Provider::<Http>::try_from(rpc_url)?;
    let chain_id = provider.get_chainid().await?;
    let block_number = provider.get_block_number().await?;

    let balance = chain_client.get_balance(address).await?;
    let nonce = chain_client.get_nonce(address, BlockId::Pending).await?;
    let gas = chain_client.get_gas_price().await?;

    println!("\n{name} check");
    println!("-----------");
    println!("Chain ID: {}", chain_id);
    println!("Latest block: {}", block_number);
    println!("Balance: {}", balance);
    println!("Pending nonce: {}", nonce);
    println!("Base fee (wei): {}", gas.base_fee);
    println!("Priority fee medium (wei): {}", gas.priority_fee_medium);

    Ok(())
}

fn required_env(name: &str) -> Result<String, Box<dyn std::error::Error>> {
    let value = env::var(name)?;
    if value.trim().is_empty() {
        return Err(format!("environment variable {name} is empty").into());
    }

    Ok(value)
}
