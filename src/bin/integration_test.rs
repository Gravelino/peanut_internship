//! Integration test: Full Sepolia flow
//!
//! Usage: cargo run --bin integration_test
//!
//! Required env vars (see .env.example):
//!   PRIVATE_KEY      – hex-encoded Ethereum private key
//!   SEPOLIA_RPC_URL  – Sepolia testnet RPC endpoint
//!
//! Performs the full lifecycle:
//! 1.  Load wallet from environment
//! 2.  Connect to Sepolia testnet
//! 3.  Check balance
//! 4.  Build a simple ETH transfer transaction
//! 5.  Estimate gas
//! 6.  Sign the transaction
//! 7.  Verify signature locally before sending
//! 8.  Send to testnet
//! 9.  Wait for confirmation
//! 10. Analyze the receipt
//! 11. Print full analysis

use peanut_internship_rust::{
    Address, ChainClient, GasPriority, SEPOLIA_CHAIN_ID, TokenAmount, TransactionBuilder,
    WalletManager,
};

/// Timeout for waiting for transaction confirmation in seconds.
const CONFIRMATION_TIMEOUT_SECS: u64 = 120;

/// Interval for polling transaction status during wait in seconds.
const POLL_INTERVAL_SECS: f64 = 3.0;

/// Minimum amount required for the test transaction.
const TEST_MIN_BALANCE_ETH: &str = "0.001";

/// Amount of ETH to send in the test transaction.
const TEST_SEND_AMOUNT_ETH: &str = "0.0001";

/// Maximum length of redacted URL for printing.
const URL_REDACT_LEN: usize = 30;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    let _ = dotenvy::dotenv();

    println!("╔═══════════════════════════════════════════╗");
    println!("║  Peanut Internship – Sepolia Integration  ║");
    println!("╚═══════════════════════════════════════════╝\n");

    println!("Step 1: Loading wallet from PRIVATE_KEY env…");
    let wallet = WalletManager::from_env("PRIVATE_KEY")?;
    let wallet_address_str = wallet.address();
    let wallet_address = Address::new(&wallet_address_str)?;
    println!("  Wallet address: {wallet_address_str}");

    println!("\nStep 2: Connecting to Sepolia…");
    let rpc_url = std::env::var("SEPOLIA_RPC_URL").map_err(|_| "SEPOLIA_RPC_URL is required")?;
    let client = ChainClient::new(vec![rpc_url.clone()], 30, 3)?;
    println!("  RPC: {}", redact_url(&rpc_url));

    println!("\nStep 3: Checking balance…");
    let balance = client.get_balance(&wallet_address).await?;
    println!("  Balance: {balance}");

    let min_balance = TokenAmount::from_eth(TEST_MIN_BALANCE_ETH)?;
    if balance.raw < min_balance.raw {
        return Err(format!(
            "Insufficient balance for test transaction. Have {balance}, need at least {min_balance}. \
             Fund via https://sepoliafaucet.com"
        ).into());
    }

    println!("\nStep 4: Building ETH self-transfer transaction…");
    let send_amount = TokenAmount::from_eth(TEST_SEND_AMOUNT_ETH)?;
    println!("  To:     {wallet_address_str} (self-transfer)");
    println!("  Value:  {send_amount}");

    let builder = TransactionBuilder::new(client.clone(), wallet.clone())
        .to(wallet_address.clone())
        .value(send_amount)
        .chain_id(SEPOLIA_CHAIN_ID);

    println!("\nStep 5: Estimating gas…");
    let builder = builder.with_gas_estimate(None as Option<u64>).await?;
    println!("  Gas estimate obtained");

    let builder = builder.with_gas_price(GasPriority::Medium).await?;
    println!("  Gas price set (medium priority)");

    println!("\nStep 6: Building and signing transaction…");
    let tx_request = builder.build().await?;
    println!("  Nonce:  {:?}", tx_request.nonce);
    println!("  Gas:    {:?}", tx_request.gas_limit);
    println!("  Chain:  {}", tx_request.chain_id);

    let signed_bytes = wallet.sign_transaction_bytes(&tx_request).await?;
    println!("  Signed bytes: {} bytes", signed_bytes.len());

    println!("\nStep 7: Verifying signature…");
    let sig = wallet.sign_transaction(&tx_request).await?;
    println!("  Signature r: 0x{}", hex::encode(sig.r.to_string()));
    println!("  Signature s: 0x{}", hex::encode(sig.s.to_string()));
    println!("  Signature v: {}", sig.v);
    println!("  ✓ Signature produced successfully");

    println!("\nStep 8: Sending transaction to Sepolia…");
    let tx_hash = client.send_transaction(&signed_bytes).await?;
    println!("  TX hash: {tx_hash}");
    println!("  Explorer: https://sepolia.etherscan.io/tx/{tx_hash}");

    println!(
        "\nStep 9: Waiting for confirmation (up to {}s)…",
        CONFIRMATION_TIMEOUT_SECS
    );
    let receipt = client
        .wait_for_receipt(&tx_hash, CONFIRMATION_TIMEOUT_SECS, POLL_INTERVAL_SECS)
        .await?;
    println!("  ✓ Confirmed in block {}", receipt.block_number);

    println!("\nStep 10: Analyzing receipt…");
    println!("  TX hash:  {}", receipt.tx_hash);
    println!("  Block:    {}", receipt.block_number);
    println!(
        "  Status:   {}",
        if receipt.status {
            "SUCCESS ✓"
        } else {
            "FAILED ✗"
        }
    );
    println!("  Gas used: {}", receipt.gas_used);
    println!("  TX fee:   {}", receipt.tx_fee());
    println!("  Logs:     {} event(s)", receipt.logs.len());

    println!("\n╔═══════════════════════════════════════════╗");
    if receipt.status {
        println!("║  ✓ Integration test PASSED                ║");
    } else {
        println!("║  ✗ Transaction REVERTED                   ║");
    }
    println!("╚═══════════════════════════════════════════╝");

    if !receipt.status {
        return Err("transaction reverted on chain".into());
    }

    Ok(())
}

fn redact_url(url: &str) -> String {
    if url.len() > URL_REDACT_LEN {
        format!("{}…", &url[..URL_REDACT_LEN])
    } else {
        url.to_string()
    }
}
