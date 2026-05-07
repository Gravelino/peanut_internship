use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use chrono::{DateTime, Duration, Utc};
use clap::Parser;
use ethers::providers::{Http, Middleware, Provider};
use ethers::types::U256;
use peanut_internship_rust::chain::ChainClient;
use peanut_internship_rust::core::types::{Address, BlockId, TransactionRequest};
use rusqlite::Connection;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const ARBITRUM_CHAIN_ID: u64 = 42161;
const ARBITRUM_UNISWAP_V3_SWAP_ROUTER: &str = "0xE592427A0AEce92De3Edee1F18E0157C05861564";
const BALANCE_OF_SELECTOR: [u8; 4] = [0x70, 0xa0, 0x82, 0x31];
const ALLOWANCE_SELECTOR: [u8; 4] = [0xdd, 0x62, 0xed, 0x3e];
const ZERO_ADDRESS: &str = "0x0000000000000000000000000000000000000000";

#[derive(Debug, Parser)]
#[command(
    name = "live_readiness",
    about = "Read-only live trading pre-flight checks"
)]
struct Cli {
    #[arg(long)]
    config: PathBuf,

    #[arg(long)]
    reconcile_db: Option<PathBuf>,

    #[arg(long)]
    events: Option<PathBuf>,

    #[arg(long)]
    metrics_snapshot: Option<PathBuf>,

    #[arg(long, default_value = "", env = "ETH_RPC_URL", value_delimiter = ',')]
    eth_rpc_url: Vec<String>,

    #[arg(long, default_value = "", env = "WALLET_ADDRESS")]
    wallet_address: String,

    #[arg(long, default_value_t = ARBITRUM_CHAIN_ID, env = "DEX_CHAIN_ID")]
    expected_chain_id: u64,

    #[arg(long, default_value = ARBITRUM_UNISWAP_V3_SWAP_ROUTER, env = "DEX_ROUTER")]
    router: String,

    #[arg(long, default_value = "0.01")]
    min_native_balance_eth: String,

    #[arg(long, default_value_t = true, env = "SIMULATION", action = clap::ArgAction::Set)]
    simulation: bool,

    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    dry_run: bool,

    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    use_flashbots: bool,

    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    require_private_dex: bool,

    #[arg(long, default_value_t = 0)]
    max_gas_gwei: u64,

    #[arg(long, default_value = "100", env = "INITIAL_CAPITAL_USD")]
    initial_capital_usd: String,

    #[arg(long, default_value = "5", env = "RISK_MAX_TRADE_USD")]
    risk_max_trade_usd: String,

    #[arg(long, default_value = "10", env = "RISK_MAX_DAILY_LOSS_USD")]
    risk_max_daily_loss_usd: String,

    #[arg(long, default_value_t = 20, env = "RISK_MAX_TRADES_PER_HOUR")]
    risk_max_trades_per_hour: u32,

    #[arg(long, default_value_t = 3, env = "RISK_CONSECUTIVE_LOSS_LIMIT")]
    risk_consecutive_loss_limit: u32,

    #[arg(long, default_value = "/tmp/arb_bot_kill", env = "HALT_FILE")]
    halt_file_path: String,

    #[arg(long, default_value_t = 60)]
    recent_window_minutes: i64,

    #[arg(long, default_value = "markdown", value_parser = ["markdown", "json"])]
    format: String,

    #[arg(long)]
    output: Option<PathBuf>,

    #[arg(long, default_value_t = false)]
    no_fail: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "UPPERCASE")]
enum CheckStatus {
    Ok,
    Skip,
    Warn,
    Fail,
}

impl CheckStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::Skip => "SKIP",
            Self::Warn => "WARN",
            Self::Fail => "FAIL",
        }
    }
}

#[derive(Debug, Serialize)]
struct ReadinessCheck {
    name: String,
    status: CheckStatus,
    detail: String,
}

#[derive(Debug, Serialize)]
struct ReadinessReport {
    generated_at: DateTime<Utc>,
    overall: CheckStatus,
    fail_count: usize,
    warn_count: usize,
    checks: Vec<ReadinessCheck>,
}

#[derive(Debug, Deserialize)]
struct AddressBookEntry {
    base: String,
    base_decimals: u8,
    quote: String,
    quote_decimals: u8,
    pool: Option<String>,
    #[serde(default)]
    pool_type: String,
    #[serde(default, alias = "fee")]
    v3_fee: Option<u32>,
    quoter: Option<String>,
}

#[derive(Debug)]
struct ParsedPair {
    pair: String,
    base_symbol: String,
    quote_symbol: String,
    base: Address,
    base_decimals: u8,
    quote: Address,
    quote_decimals: u8,
    pool: Option<Address>,
    pool_type: String,
    v3_fee: Option<u32>,
    quoter: Option<Address>,
}

#[derive(Debug, Deserialize)]
struct EventLine {
    ts: DateTime<Utc>,
    event_type: String,
    #[serde(default)]
    fields: Value,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();
    let cli = Cli::parse();
    let report = build_report(&cli).await;
    let body = match cli.format.as_str() {
        "json" => serde_json::to_string_pretty(&report)?,
        _ => render_markdown(&report),
    };
    if let Some(path) = &cli.output {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, body)?;
    } else {
        print!("{body}");
    }
    if report.overall == CheckStatus::Fail && !cli.no_fail {
        std::process::exit(2);
    }
    Ok(())
}

async fn build_report(cli: &Cli) -> ReadinessReport {
    let mut checks = Vec::new();
    let address_book = match load_address_book(&cli.config) {
        Ok(pairs) => {
            checks.push(check(
                "router/token address sanity",
                CheckStatus::Ok,
                format!(
                    "loaded {} pair entries from {}",
                    pairs.len(),
                    cli.config.display()
                ),
            ));
            pairs
        }
        Err(error) => {
            checks.push(check(
                "router/token address sanity",
                CheckStatus::Fail,
                format!("{}: {error}", cli.config.display()),
            ));
            Vec::new()
        }
    };
    checks.extend(address_book_checks(&address_book, &cli.router));

    let rpc_urls = normalize_rpc_urls(&cli.eth_rpc_url);
    let wallet = parse_optional_wallet(&cli.wallet_address);
    checks.extend(live_flag_checks(cli));

    if rpc_urls.is_empty() {
        checks.push(check(
            "chain id",
            CheckStatus::Skip,
            "ETH_RPC_URL/--eth-rpc-url not set; skipped live RPC checks",
        ));
        checks.push(check(
            "balances",
            CheckStatus::Skip,
            "ETH_RPC_URL/--eth-rpc-url not set; skipped wallet balance checks",
        ));
        checks.push(check(
            "allowances",
            CheckStatus::Skip,
            "ETH_RPC_URL/--eth-rpc-url not set; skipped allowance checks",
        ));
    } else {
        checks.extend(chain_checks(&rpc_urls, cli.expected_chain_id).await);
        match wallet {
            Ok(Some(wallet)) => {
                let min_native =
                    Decimal::from_str_exact(&cli.min_native_balance_eth).unwrap_or(Decimal::ZERO);
                checks.extend(balance_checks(&rpc_urls, &wallet, &address_book, min_native).await);
                checks
                    .extend(allowance_checks(&rpc_urls, &wallet, &address_book, &cli.router).await);
            }
            Ok(None) => {
                checks.push(check(
                    "wallet address",
                    CheckStatus::Skip,
                    "WALLET_ADDRESS/--wallet-address not set; skipped balance and allowance checks",
                ));
            }
            Err(error) => checks.push(check("wallet address", CheckStatus::Fail, error)),
        }
    }

    if let Some(path) = &cli.reconcile_db {
        checks.extend(reconcile_checks(path));
    } else {
        checks.push(check(
            "reconcile DB open entries",
            CheckStatus::Skip,
            "--reconcile-db not provided",
        ));
    }

    if let Some(path) = &cli.events {
        checks.extend(event_checks(path, cli.recent_window_minutes));
    } else {
        checks.push(check(
            "recent bot events",
            CheckStatus::Skip,
            "--events not provided",
        ));
    }

    if let Some(path) = &cli.metrics_snapshot {
        checks.extend(metrics_checks(path));
    } else {
        checks.push(check(
            "metrics health snapshot",
            CheckStatus::Skip,
            "--metrics-snapshot not provided",
        ));
    }

    let overall = overall_status(&checks);
    let fail_count = checks
        .iter()
        .filter(|c| c.status == CheckStatus::Fail)
        .count();
    let warn_count = checks
        .iter()
        .filter(|c| c.status == CheckStatus::Warn)
        .count();
    ReadinessReport {
        generated_at: Utc::now(),
        overall,
        fail_count,
        warn_count,
        checks,
    }
}

fn check(
    name: impl Into<String>,
    status: CheckStatus,
    detail: impl Into<String>,
) -> ReadinessCheck {
    ReadinessCheck {
        name: name.into(),
        status,
        detail: detail.into(),
    }
}

fn overall_status(checks: &[ReadinessCheck]) -> CheckStatus {
    if checks.iter().any(|c| c.status == CheckStatus::Fail) {
        CheckStatus::Fail
    } else if checks.iter().any(|c| c.status == CheckStatus::Warn) {
        CheckStatus::Warn
    } else {
        CheckStatus::Ok
    }
}

fn load_address_book(path: &Path) -> Result<Vec<ParsedPair>, Box<dyn std::error::Error>> {
    let raw: BTreeMap<String, AddressBookEntry> = serde_json::from_reader(File::open(path)?)?;
    let mut out = Vec::new();
    for (pair, entry) in raw {
        let (base_symbol, quote_symbol) = pair
            .split_once('/')
            .ok_or_else(|| format!("pair '{pair}' missing '/' separator"))?;
        let base_symbol = base_symbol.to_ascii_uppercase();
        let quote_symbol = quote_symbol.to_ascii_uppercase();
        let pool = entry.pool.as_deref().map(Address::new).transpose()?;
        let quoter = entry.quoter.as_deref().map(Address::new).transpose()?;
        out.push(ParsedPair {
            pair,
            base_symbol,
            quote_symbol,
            base: Address::new(&entry.base)?,
            base_decimals: entry.base_decimals,
            quote: Address::new(&entry.quote)?,
            quote_decimals: entry.quote_decimals,
            pool,
            pool_type: entry.pool_type.to_ascii_lowercase(),
            v3_fee: entry.v3_fee,
            quoter,
        });
    }
    Ok(out)
}

fn address_book_checks(pairs: &[ParsedPair], router: &str) -> Vec<ReadinessCheck> {
    let mut checks = Vec::new();
    match Address::new(router) {
        Ok(addr) if addr.lower() != ZERO_ADDRESS => checks.push(check(
            "router address",
            CheckStatus::Ok,
            format!("router {addr}"),
        )),
        Ok(_) => checks.push(check(
            "router address",
            CheckStatus::Fail,
            "router is zero address",
        )),
        Err(error) => checks.push(check(
            "router address",
            CheckStatus::Fail,
            error.to_string(),
        )),
    }
    if pairs.is_empty() {
        checks.push(check(
            "address book pairs",
            CheckStatus::Fail,
            "no pairs loaded",
        ));
        return checks;
    }
    let mut token_addresses = BTreeMap::new();
    for pair in pairs {
        let mut issues = Vec::new();
        if pair.base.lower() == ZERO_ADDRESS || pair.quote.lower() == ZERO_ADDRESS {
            issues.push("zero token address".to_string());
        }
        if pair.base == pair.quote {
            issues.push("base and quote are identical".to_string());
        }
        if pair.base_decimals > 36 || pair.quote_decimals > 36 {
            issues.push("token decimals exceed sanity bound".to_string());
        }
        if !matches!(pair.pool_type.as_str(), "v2" | "v3") {
            issues.push(format!("unsupported pool_type '{}'", pair.pool_type));
        }
        if pair.pool.is_none() {
            issues.push("missing pool address".to_string());
        }
        if pair.pool_type == "v3" && pair.quoter.is_none() {
            issues.push("V3 pair missing quoter".to_string());
        }
        token_addresses.insert(
            format!("{}:{}", pair.base_symbol, pair.base.lower()),
            pair.base_decimals,
        );
        token_addresses.insert(
            format!("{}:{}", pair.quote_symbol, pair.quote.lower()),
            pair.quote_decimals,
        );
        checks.push(check(
            format!("address book {}", pair.pair),
            if issues.is_empty() {
                CheckStatus::Ok
            } else {
                CheckStatus::Fail
            },
            if issues.is_empty() {
                let fee_detail = if pair.pool_type == "v3" {
                    pair.v3_fee
                        .map(|fee| format!(", v3_fee={fee}"))
                        .unwrap_or_else(|| ", v3_fee=discover_from_pool".to_string())
                } else {
                    String::new()
                };
                format!(
                    "{} {} / {} {}, pool_type={}{}",
                    pair.base_symbol,
                    pair.base,
                    pair.quote_symbol,
                    pair.quote,
                    pair.pool_type,
                    fee_detail
                )
            } else {
                issues.join("; ")
            },
        ));
    }
    checks.push(check(
        "token universe",
        CheckStatus::Ok,
        format!(
            "{} unique symbol/address/decimals entries",
            token_addresses.len()
        ),
    ));
    checks
}

fn normalize_rpc_urls(raw: &[String]) -> Vec<String> {
    raw.iter()
        .flat_map(|item| item.split(','))
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn parse_optional_wallet(raw: &str) -> Result<Option<Address>, String> {
    if raw.trim().is_empty() {
        Ok(None)
    } else {
        Address::new(raw)
            .map(Some)
            .map_err(|error| format!("invalid wallet address: {error}"))
    }
}

async fn chain_checks(rpc_urls: &[String], expected_chain_id: u64) -> Vec<ReadinessCheck> {
    let mut checks = Vec::new();
    match Provider::<Http>::try_from(rpc_urls[0].as_str()) {
        Ok(provider) => match provider.get_chainid().await {
            Ok(chain_id) if chain_id.as_u64() == expected_chain_id => checks.push(check(
                "chain id",
                CheckStatus::Ok,
                format!("primary RPC chain_id={expected_chain_id}"),
            )),
            Ok(chain_id) => checks.push(check(
                "chain id",
                CheckStatus::Fail,
                format!("expected {expected_chain_id}, got {}", chain_id.as_u64()),
            )),
            Err(error) => checks.push(check("chain id", CheckStatus::Fail, error.to_string())),
        },
        Err(error) => checks.push(check("chain id", CheckStatus::Fail, error.to_string())),
    }
    match ChainClient::new(rpc_urls.to_vec(), 30, 2) {
        Ok(client) => {
            let health = client.health_check().await;
            let healthy = health.iter().filter(|rpc| rpc.healthy).count();
            let status = if healthy > 0 {
                CheckStatus::Ok
            } else {
                CheckStatus::Fail
            };
            checks.push(check(
                "RPC health",
                status,
                format!("{healthy}/{} endpoints healthy", health.len()),
            ));
        }
        Err(error) => checks.push(check("RPC health", CheckStatus::Fail, error.to_string())),
    }
    checks
}

async fn balance_checks(
    rpc_urls: &[String],
    wallet: &Address,
    pairs: &[ParsedPair],
    min_native_balance: Decimal,
) -> Vec<ReadinessCheck> {
    let mut checks = Vec::new();
    let client = match ChainClient::new(rpc_urls.to_vec(), 30, 2) {
        Ok(client) => client,
        Err(error) => {
            checks.push(check("balances", CheckStatus::Fail, error.to_string()));
            return checks;
        }
    };
    match client.get_balance(wallet).await {
        Ok(amount) => {
            let human = amount.human().unwrap_or(Decimal::ZERO);
            checks.push(check(
                "native balance",
                if human >= min_native_balance {
                    CheckStatus::Ok
                } else {
                    CheckStatus::Fail
                },
                format!("wallet {wallet} has {human} ETH; minimum {min_native_balance}"),
            ));
        }
        Err(error) => checks.push(check(
            "native balance",
            CheckStatus::Fail,
            error.to_string(),
        )),
    }
    for (symbol, token, decimals) in unique_tokens(pairs) {
        match erc20_balance(&client, &token, wallet, decimals).await {
            Ok(balance) => checks.push(check(
                format!("token balance {symbol}"),
                if balance > Decimal::ZERO {
                    CheckStatus::Ok
                } else {
                    CheckStatus::Warn
                },
                format!("{symbol} {token}: {balance}"),
            )),
            Err(error) => checks.push(check(
                format!("token balance {symbol}"),
                CheckStatus::Warn,
                error,
            )),
        }
    }
    checks
}

async fn allowance_checks(
    rpc_urls: &[String],
    wallet: &Address,
    pairs: &[ParsedPair],
    router: &str,
) -> Vec<ReadinessCheck> {
    let mut checks = Vec::new();
    let router = match Address::new(router) {
        Ok(router) => router,
        Err(error) => {
            checks.push(check("allowances", CheckStatus::Fail, error.to_string()));
            return checks;
        }
    };
    let client = match ChainClient::new(rpc_urls.to_vec(), 30, 2) {
        Ok(client) => client,
        Err(error) => {
            checks.push(check("allowances", CheckStatus::Fail, error.to_string()));
            return checks;
        }
    };
    for (symbol, token, _) in unique_tokens(pairs) {
        match erc20_allowance(&client, &token, wallet, &router).await {
            Ok(allowance) => checks.push(check(
                format!("allowance {symbol}"),
                if allowance > U256::zero() {
                    CheckStatus::Ok
                } else {
                    CheckStatus::Warn
                },
                format!("{symbol} {token} allowance to {router}: {allowance}"),
            )),
            Err(error) => checks.push(check(
                format!("allowance {symbol}"),
                CheckStatus::Warn,
                error,
            )),
        }
    }
    checks
}

async fn erc20_balance(
    client: &ChainClient,
    token: &Address,
    wallet: &Address,
    decimals: u8,
) -> Result<Decimal, String> {
    let mut data = BALANCE_OF_SELECTOR.to_vec();
    data.extend_from_slice(&[0u8; 12]);
    data.extend_from_slice(&wallet.as_eth_address().0);
    let req = TransactionRequest::contract_call(token.clone(), data, ARBITRUM_CHAIN_ID);
    let raw = client
        .call(&req, BlockId::Latest)
        .await
        .map_err(|error| format!("balanceOf {token}: {error}"))?;
    decode_u256(&raw)
        .and_then(|value| u256_to_decimal(value, decimals))
        .map_err(|error| format!("balanceOf {token}: {error}"))
}

async fn erc20_allowance(
    client: &ChainClient,
    token: &Address,
    owner: &Address,
    spender: &Address,
) -> Result<U256, String> {
    let mut data = ALLOWANCE_SELECTOR.to_vec();
    data.extend_from_slice(&[0u8; 12]);
    data.extend_from_slice(&owner.as_eth_address().0);
    data.extend_from_slice(&[0u8; 12]);
    data.extend_from_slice(&spender.as_eth_address().0);
    let req = TransactionRequest::contract_call(token.clone(), data, ARBITRUM_CHAIN_ID);
    let raw = client
        .call(&req, BlockId::Latest)
        .await
        .map_err(|error| format!("allowance {token}: {error}"))?;
    decode_u256(&raw).map_err(|error| format!("allowance {token}: {error}"))
}

fn decode_u256(raw: &[u8]) -> Result<U256, String> {
    if raw.len() < 32 {
        return Err(format!("expected 32 bytes, got {}", raw.len()));
    }
    Ok(U256::from_big_endian(&raw[raw.len() - 32..]))
}

fn u256_to_decimal(value: U256, decimals: u8) -> Result<Decimal, String> {
    let raw = Decimal::from_str(&value.to_string()).map_err(|error| error.to_string())?;
    let mut scale = Decimal::ONE;
    for _ in 0..decimals {
        scale *= Decimal::from(10u64);
    }
    Ok(raw / scale)
}

fn unique_tokens(pairs: &[ParsedPair]) -> Vec<(String, Address, u8)> {
    let mut tokens = BTreeMap::new();
    for pair in pairs {
        tokens.entry(pair.base.lower()).or_insert_with(|| {
            (
                pair.base_symbol.clone(),
                pair.base.clone(),
                pair.base_decimals,
            )
        });
        tokens.entry(pair.quote.lower()).or_insert_with(|| {
            (
                pair.quote_symbol.clone(),
                pair.quote.clone(),
                pair.quote_decimals,
            )
        });
    }
    tokens.into_values().collect()
}

fn reconcile_checks(path: &Path) -> Vec<ReadinessCheck> {
    let mut checks = Vec::new();
    let conn = match Connection::open(path) {
        Ok(conn) => conn,
        Err(error) => {
            checks.push(check(
                "reconcile DB open entries",
                CheckStatus::Fail,
                format!("{}: {error}", path.display()),
            ));
            return checks;
        }
    };
    let mut counts = BTreeMap::<String, usize>::new();
    match conn.prepare("SELECT status, COUNT(*) FROM pending_reconcile GROUP BY status") {
        Ok(mut stmt) => {
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            });
            match rows {
                Ok(rows) => {
                    for row in rows.flatten() {
                        counts.insert(row.0, row.1.max(0) as usize);
                    }
                }
                Err(error) => checks.push(check(
                    "reconcile DB open entries",
                    CheckStatus::Fail,
                    error.to_string(),
                )),
            }
        }
        Err(error) => {
            checks.push(check(
                "reconcile DB open entries",
                CheckStatus::Fail,
                format!("pending_reconcile query failed: {error}"),
            ));
            return checks;
        }
    }
    let pending = counts.get("pending").copied().unwrap_or(0);
    let expired = counts.get("expired").copied().unwrap_or(0);
    let errored = counts.get("errored").copied().unwrap_or(0);
    let reverted = counts.get("reverted").copied().unwrap_or(0);
    let status = if expired + errored + reverted > 0 {
        CheckStatus::Fail
    } else if pending > 0 {
        CheckStatus::Warn
    } else {
        CheckStatus::Ok
    };
    checks.push(check(
        "reconcile DB open entries",
        status,
        format!("pending={pending}, expired={expired}, errored={errored}, reverted={reverted}"),
    ));
    checks
}

fn event_checks(path: &Path, recent_window_minutes: i64) -> Vec<ReadinessCheck> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return vec![check(
                "recent bot events",
                CheckStatus::Warn,
                format!("{} missing; no recent event history yet", path.display()),
            )];
        }
        Err(error) => {
            return vec![check(
                "recent bot events",
                CheckStatus::Fail,
                format!("{}: {error}", path.display()),
            )];
        }
    };
    let cutoff = Utc::now() - Duration::minutes(recent_window_minutes.max(1));
    let mut recent = 0usize;
    let mut parse_errors = 0usize;
    let mut risky = BTreeMap::<String, usize>::new();
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else {
            parse_errors += 1;
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<EventLine>(&line) {
            Ok(event) => {
                if event.ts >= cutoff {
                    recent += 1;
                    if is_risky_event(&event) {
                        *risky.entry(event.event_type).or_default() += 1;
                    }
                }
            }
            Err(_) => parse_errors += 1,
        }
    }
    let risky_total: usize = risky.values().sum();
    let status = if parse_errors > 0 || risky_total > 0 || recent == 0 {
        CheckStatus::Warn
    } else {
        CheckStatus::Ok
    };
    vec![check(
        "recent bot events",
        status,
        format!(
            "recent={recent}, risky_recent={risky_total}, parse_errors={parse_errors}, window_minutes={recent_window_minutes}"
        ),
    )]
}

fn is_risky_event(event: &EventLine) -> bool {
    matches!(
        event.event_type.as_str(),
        "bot_stopped"
            | "dex_pending_timeout"
            | "dex_cancel_outcome"
            | "reconcile_inspect_error"
            | "reconcile_enqueued"
    ) || event
        .fields
        .get("outcome")
        .and_then(Value::as_str)
        .is_some_and(|outcome| matches!(outcome, "error" | "unknown" | "expired" | "reverted"))
}

fn metrics_checks(path: &Path) -> Vec<ReadinessCheck> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) => {
            return vec![check(
                "metrics health snapshot",
                CheckStatus::Fail,
                format!("{}: {error}", path.display()),
            )];
        }
    };
    let mut totals = BTreeMap::<String, f64>::new();
    let mut parse_errors = 0usize;
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else {
            parse_errors += 1;
            continue;
        };
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match parse_prometheus_sample(line) {
            Some((metric, value)) if metric.starts_with("peanut_") => {
                *totals.entry(metric).or_default() += value;
            }
            Some(_) => {}
            None => parse_errors += 1,
        }
    }
    let relay_errors = metric_total(&totals, "peanut_flashbots_relay_errors_total");
    let not_included = metric_total(&totals, "peanut_flashbots_bundles_not_included_total");
    let expired = metric_total(&totals, "peanut_reconcile_expired_total");
    let cancel_errors = metric_total(&totals, "peanut_dex_cancel_outcomes_total");
    let status = if expired > 0.0 {
        CheckStatus::Fail
    } else if parse_errors > 0 || relay_errors > 0.0 || not_included > 0.0 || cancel_errors > 0.0 {
        CheckStatus::Warn
    } else {
        CheckStatus::Ok
    };
    vec![check(
        "metrics health snapshot",
        status,
        format!(
            "relay_errors={relay_errors}, bundles_not_included={not_included}, reconcile_expired={expired}, dex_cancel_errors={cancel_errors}, parse_errors={parse_errors}"
        ),
    )]
}

fn parse_prometheus_sample(line: &str) -> Option<(String, f64)> {
    let (sample, value_raw) = line.rsplit_once(char::is_whitespace)?;
    let value = value_raw.parse::<f64>().ok()?;
    let metric = sample
        .split_once('{')
        .map(|(name, _)| name)
        .unwrap_or(sample)
        .to_string();
    Some((metric, value))
}

fn metric_total(totals: &BTreeMap<String, f64>, name: &str) -> f64 {
    totals.get(name).copied().unwrap_or(0.0)
}

fn live_flag_checks(cli: &Cli) -> Vec<ReadinessCheck> {
    let mut checks = Vec::new();
    checks.push(check(
        "live mode flag",
        if cli.simulation {
            CheckStatus::Fail
        } else {
            CheckStatus::Ok
        },
        format!("simulation={}", cli.simulation),
    ));
    checks.push(check(
        "dry run flag",
        if cli.dry_run {
            CheckStatus::Warn
        } else {
            CheckStatus::Ok
        },
        format!("dry_run={}", cli.dry_run),
    ));
    checks.push(check(
        "private DEX flag",
        if cli.require_private_dex && !cli.use_flashbots {
            CheckStatus::Fail
        } else if cli.use_flashbots {
            CheckStatus::Ok
        } else {
            CheckStatus::Warn
        },
        format!(
            "use_flashbots={}, require_private_dex={}",
            cli.use_flashbots, cli.require_private_dex
        ),
    ));
    checks.push(check(
        "gas cap",
        if cli.max_gas_gwei == 0 {
            CheckStatus::Warn
        } else {
            CheckStatus::Ok
        },
        format!("max_gas_gwei={}", cli.max_gas_gwei),
    ));
    checks.extend(risk_cap_checks(cli));
    checks.push(check(
        "halt file",
        if cli.halt_file_path.is_empty() {
            CheckStatus::Warn
        } else if Path::new(&cli.halt_file_path).exists() {
            CheckStatus::Fail
        } else {
            CheckStatus::Ok
        },
        if cli.halt_file_path.is_empty() {
            "HALT_FILE disabled".to_string()
        } else {
            format!(
                "{} exists={}",
                cli.halt_file_path,
                Path::new(&cli.halt_file_path).exists()
            )
        },
    ));
    checks
}

fn risk_cap_checks(cli: &Cli) -> Vec<ReadinessCheck> {
    let initial = Decimal::from_str_exact(&cli.initial_capital_usd);
    let max_trade = Decimal::from_str_exact(&cli.risk_max_trade_usd);
    let max_daily_loss = Decimal::from_str_exact(&cli.risk_max_daily_loss_usd);
    let mut checks = Vec::new();
    match (initial, max_trade, max_daily_loss) {
        (Ok(initial), Ok(max_trade), Ok(max_daily_loss)) => {
            let mut issues = Vec::new();
            if initial <= Decimal::ZERO {
                issues.push("initial_capital_usd must be positive");
            }
            if max_trade <= Decimal::ZERO {
                issues.push("risk_max_trade_usd must be positive");
            }
            if max_daily_loss <= Decimal::ZERO {
                issues.push("risk_max_daily_loss_usd must be positive");
            }
            if max_trade > initial {
                issues.push("risk_max_trade_usd exceeds initial_capital_usd");
            }
            if max_daily_loss > initial {
                issues.push("risk_max_daily_loss_usd exceeds initial_capital_usd");
            }
            if cli.risk_max_trades_per_hour == 0 {
                issues.push("risk_max_trades_per_hour is zero");
            }
            if cli.risk_consecutive_loss_limit == 0 {
                issues.push("risk_consecutive_loss_limit is zero");
            }
            checks.push(check(
                "live flags/risk caps",
                if issues.is_empty() { CheckStatus::Ok } else { CheckStatus::Fail },
                if issues.is_empty() {
                    format!(
                        "initial_capital_usd={initial}, max_trade_usd={max_trade}, max_daily_loss_usd={max_daily_loss}, max_trades_per_hour={}, consecutive_loss_limit={}",
                        cli.risk_max_trades_per_hour, cli.risk_consecutive_loss_limit
                    )
                } else {
                    issues.join("; ")
                },
            ));
        }
        _ => checks.push(check(
            "live flags/risk caps",
            CheckStatus::Fail,
            "risk cap decimal parsing failed",
        )),
    }
    checks
}

fn render_markdown(report: &ReadinessReport) -> String {
    let mut out = String::new();
    out.push_str("# Live Readiness\n\n");
    out.push_str(&format!("Generated: `{}`\n\n", report.generated_at));
    out.push_str(&format!("Overall: **{}**\n\n", report.overall.as_str()));
    out.push_str(&format!(
        "Failures: `{}` | Warnings: `{}`\n\n",
        report.fail_count, report.warn_count
    ));
    out.push_str("| Check | Status | Detail |\n|---|---:|---|\n");
    for check in &report.checks {
        out.push_str(&format!(
            "| {} | {} | {} |\n",
            escape_md(&check.name),
            check.status.as_str(),
            escape_md(&check.detail)
        ));
    }
    out
}

fn escape_md(input: &str) -> String {
    input.replace('|', "\\|").replace('\n', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_prometheus_metric_name_with_labels() {
        let parsed =
            parse_prometheus_sample("peanut_flashbots_relay_errors_total{relay=\"x\"} 2").unwrap();
        assert_eq!(parsed.0, "peanut_flashbots_relay_errors_total");
        assert_eq!(parsed.1, 2.0);
    }

    #[test]
    fn overall_fails_when_any_check_fails() {
        let checks = vec![
            check("a", CheckStatus::Ok, "ok"),
            check("b", CheckStatus::Fail, "bad"),
        ];
        assert_eq!(overall_status(&checks), CheckStatus::Fail);
    }

    #[test]
    fn normalizes_comma_separated_rpc_urls() {
        let urls = normalize_rpc_urls(&["http://a, http://b".into(), "".into()]);
        assert_eq!(urls, vec!["http://a", "http://b"]);
    }

    #[test]
    fn zero_wallet_is_valid_address_but_present() {
        assert!(parse_optional_wallet("").unwrap().is_none());
        assert!(parse_optional_wallet(ZERO_ADDRESS).unwrap().is_some());
    }
}
