# Peanut Internship - Rust Project

An arbitrage detection and execution system spanning DEX (Uniswap V2) and CEX (Binance) venues, built entirely in Rust.

Scope covers the full pipeline: order-book + pool monitoring → signal generation and scoring → dual-leg execution (CEX-first or DEX-first) with timeouts, race-on-cancel, partial-fill handling, unwind, circuit breaker, persistent replay protection and reconciliation → Prometheus metrics and webhook alerts.

### Quick Start
1. `cp .env.example .env`
2. Fill in API keys in `.env`
3. `make run` (or `cargo run --bin arb_bot -- --simulation` for a safe dry run)
4. `make test`

### Environment
Exchange / chain basics:
- `PRIVATE_KEY`: hex-encoded Ethereum private key for local/testnet use
- `SEPOLIA_RPC_URL`: Sepolia RPC endpoint for integration work
- `MAINNET_RPC_URL`: optional mainnet RPC endpoint for analysis tooling
- `BINANCE_TESTNET_API_KEY` / `BINANCE_TESTNET_SECRET`: Binance testnet credentials

Live DEX execution (`arb_bot --dex-address-book ...`):
- `ETH_RPC_URL`: mainnet (or fork) RPC endpoint used by `UniswapV2Swapper`
- `WALLET_ADDRESS`: 0x-address that signs and receives DEX output
- `WALLET_PRIVATE_KEY`: signing key (env-var name is configurable via `--wallet-key-env`)

### Health Check
Run the environment and RPC checker before integration work:

`cargo run --bin check_env_and_rpc`

It verifies:
- wallet key is loadable
- derived wallet address
- Sepolia RPC connectivity, chain id, block height
- balance, pending nonce, and gas snapshot

### Integration Test
Full Sepolia lifecycle test — loads wallet, checks balance, builds a transfer,
estimates gas, signs, verifies signature locally, sends to testnet,
waits for confirmation, and analyzes the receipt:

```sh
cargo run --bin integration_test
```

Required env vars: `PRIVATE_KEY`, `SEPOLIA_RPC_URL`.
The wallet must hold at least **0.001 Sepolia ETH** (use https://sepoliafaucet.com).

### Analyzer CLI
Analyze any Ethereum transaction hash:

```sh
cargo run --bin analyzer -- <tx_hash> --rpc <url>
cargo run --bin analyzer -- <tx_hash> --format json
```

### Week 3: Exchange & Inventory CLIs

```sh
# Fetch and analyze order book
cargo run --bin orderbook_cli -- ETH/USDT --depth 20

# Check inventory skew and rebalance plans
cargo run --bin rebalancer_cli -- check
cargo run --bin rebalancer_cli -- plan ETH

# PnL summary dashboard
cargo run --bin pnl_cli

# End-to-end arbitrage check
cargo run --bin arb_checker_cli -- ETH/USDT --size 2.0
```

### Running `arb_bot`

`arb_bot` is the end-to-end runtime: it ticks the signal generator, scores opportunities, executes them via the dual-leg pipeline, persists state for reconciliation, and fans out metrics / alerts.

```sh
# Safe dry-run: stubbed DEX prices, simulated legs.
cargo run --bin arb_bot -- --simulation

# Live CEX + live DEX execution with persistent replay + reconcile.
cargo run --bin arb_bot -- \
    --pair ETH/USDC \
    --eth-rpc-url "$ETH_RPC_URL" \
    --wallet-address "$WALLET_ADDRESS" \
    --dex-address-book configs/address_book.json \
    --dex-slippage-bps 50 \
    --replay-db data/replay.db \
    --reconcile-db data/reconcile.db \
    --alert-webhook-url "$SLACK_WEBHOOK" --alert-provider slack \
    --metrics-addr 0.0.0.0:9090
```

Key flags:
- **`--simulation`** — in-process `SimulatedLegs`; never touches the exchange or chain.
- **`--dex-address-book <path>`** — JSON of `{pair: {base, base_decimals, quote, quote_decimals}}`; enables live DEX via `UniswapV2Swapper`. Without it, live mode logs a warning and the DEX leg returns `NotImplemented`.
- **`--replay-db <path>`** — SQLite journal so the signal-id replay window survives restarts (stretch S8).
- **`--reconcile-db <path>`** — SQLite store of `LEG2_TIMEOUT` entries. A background worker polls each entry's receipt, marks it `Resolved` / `Reverted` / `Expired`, and on revert fires `LegExecutor::unwind_position` to flatten leg 1 (stretch S3 + A2).
- **`--alert-webhook-url` / `--alert-provider`** — Generic, Slack, or Discord adapter. The full URL is **never** logged — `mask_webhook_url` keeps only host + first path segment.
- **`--metrics-addr`** — Prometheus exporter (S7a). Scrape `http://<addr>/metrics`.

The bot respects a circuit breaker (N failures in a rolling window → cool-off), a replay window that REJECTS duplicate signal ids, and a configurable `max_concurrent_executions` cap. A `ctrl-c` cleanly shuts down the reconcile worker loop.

### Architecture

```mermaid
flowchart TB
    subgraph Foundations
        A[core/types] --> B[core/wallet]
        C[chain/client] --> D[chain/builder]
    end

    subgraph Pricing
        E[pricing/amm] --> F[pricing/router]
        F --> G[pricing/engine]
        H[pricing/mempool] --> G
    end

    subgraph Exchange_Inventory [Exchange + Inventory]
        K[exchange/client] --> M[exchange/orderbook]
        N[exchange/rate_limiter] --> K
        O[inventory/tracker] --> P[inventory/rebalancer]
        Q[inventory/pnl]
        W[inventory/wallet<br/>on-chain sync] --> O
    end

    subgraph Strategy
        S1[strategy/generator] --> S2[strategy/scorer]
        S3[strategy/fees]
    end

    subgraph Executor_Runtime [Executor + Runtime]
        X1[executor/queue<br/>priority + concurrency] --> X2[executor/engine<br/>Executor + LegExecutor]
        X2 --> X3[executor/dex_swapper<br/>UniswapV2Swapper]
        X2 --> X4[executor/recovery<br/>breaker + replay]
        X2 --> X5[executor/reconcile<br/>LEG2_TIMEOUT worker]
    end

    subgraph Observability
        Y1[observability/metrics<br/>Prometheus] --> Y3[observability/server]
        Y2[observability/alerts<br/>Slack / Discord / Generic]
    end

    G --> S1
    M --> S1
    S2 --> X1
    K --> X2
    X3 --> C
    X5 --> C
    X2 --> Q
    X2 --> Y1
    X2 --> Y2
```

Execution pipeline (per signal):
```mermaid
flowchart LR
    IN[Signal] --> QQ[SignalQueue<br/>priority heap]
    QQ --> EX[Executor.execute]
    EX --> PF{pre-flight<br/>breaker + replay}
    PF -->|rejected| OUT1[REJECTED]
    PF -->|ok| L1[Leg 1: CEX or DEX]
    L1 -->|timeout| RC[Race-on-cancel<br/>CancelOutcome]
    L1 -->|partial| FC[classify_fill<br/>Full / ProceedReduced / AbortUnwind / Dust]
    L1 -->|ok| L2[Leg 2]
    L2 -->|filled| OUT2[DONE_PROFIT / DONE_LOSS]
    L2 -->|reverted| UW[Unwind leg 1] --> OUT3[FAILED]
    L2 -->|timeout| RS[Reconcile store<br/>SQLite] --> OUT4[LEG2_TIMEOUT]
    RS -.poll.-> WK[Reconcile worker]
    WK -->|reverted| UW
    WK -->|success| OUT2
    WK -->|expired| OUT5[MANUAL_REVIEW]
```

### Repository Architecture
- `src/core/`: Base types (Address, TokenAmount, Token), WalletManager, CanonicalSerializer
- `src/chain/`: ChainClient (RPC + retry), TransactionBuilder, TransactionAnalyzer
- `src/pricing/`: AMM math, router, mempool monitor, fork simulator, pricing engine
- `src/exchange/`: Binance client, order book analyzer, rate limiter, price oracle
- `src/inventory/`: Position tracker, rebalance planner, PnL engine, on-chain wallet sync
- `src/strategy/`: Signal generator (from venue prices), scorer (weighted 4-component), fee model
- `src/executor/`: Queue (priority), `Executor` state machine, `LegExecutor` trait with `SimulatedLegs` / `LiveLegs`, `UniswapV2Swapper`, circuit breaker, replay protection, reconciliation worker
- `src/observability/`: Prometheus metrics exporter, webhook alert sinks (Generic / Slack / Discord)
- `src/integration/`: `ArbChecker` — end-to-end arbitrage pipeline helper
- `src/bin/`: CLI binaries (`arb_bot`, `orderbook_cli`, `pnl_cli`, `analyzer`, `integration_test`, etc.)
- `tests/`: Unit and integration tests
- `scripts/`: Automation scripts
- `configs/`: Non-secret configuration
- `docs/`: Module technical guides

### Module Details

#### exchange/
| File | Purpose |
|------|---------|
| `config.rs` | Binance testnet config from env vars |
| `client.rs` | REST API client (order book, balance, orders, fees) |
| `orderbook.rs` | Walk-the-book, depth analysis, spread, imbalance |
| `rate_limiter.rs` | Token-bucket rate limiter (1200 weight/min) |
| `types.rs` | OrderBookSnapshot, OrderResult, NormalizedBalance, etc. |
| `errors.rs` | ExchangeError enum |

#### inventory/
| File | Purpose |
|------|---------|
| `tracker.rs` | Multi-venue position tracking, can_execute, skew detection |
| `rebalancer.rs` | Threshold-based rebalance planning with fee accounting |
| `pnl.rs` | Per-trade and aggregate PnL tracking, CSV export |
| `types.rs` | Venue enum, TransferPlan, fee constants |
| `errors.rs` | InventoryError enum |

#### integration/
| File | Purpose |
|------|---------|
| `arb_checker.rs` | End-to-end arb check: DEX price + CEX book + inventory + costs |

#### strategy/
| File | Purpose |
|------|---------|
| `signal.rs` | `Signal` / `SignalParams`, direction, TTL, deterministic `signal_id` |
| `generator.rs` | `SignalGenerator` — converts venue prices into Signals, cooldown + inventory pre-filter |
| `scorer.rs` | 4-component weighted scorer (spread, liquidity, inventory, history) |
| `fees.rs` | Fee model shared between scoring and PnL |

#### executor/
| File | Purpose |
|------|---------|
| `engine.rs` | `Executor` state machine, `LegExecutor` trait (`SimulatedLegs`, `LiveLegs` with injected `UniswapV2Swapper`), CEX-first + DEX-first flows, race-on-cancel, partial-fill classifier, unwind |
| `queue.rs` | Priority-scored `SignalQueue` + `QueueWorker` with `max_concurrent_executions` semaphore |
| `dex_swapper.rs` | `DexSwapper` trait + production `UniswapV2Swapper` (`ensure_allowance` + `swapExactTokensForTokens` via `TransactionBuilder`) |
| `recovery.rs` | `CircuitBreaker` + `ReplayProtection` (in-memory or SQLite-backed journal) |
| `reconcile.rs` | `ReconcileStore` (SQLite) + `ReconcileWorker` — receipt polling, async `spawn_blocking` I/O, transitions Pending→Resolved/Reverted/Expired |
| `errors.rs` | `ExecutorError`, `ExecutorResult` |

#### observability/
| File | Purpose |
|------|---------|
| `metrics.rs` | Prometheus counters / histograms (`init_metrics`, `metrics_handle`) |
| `server.rs` | Hyper-based `/metrics` exporter (`serve_metrics`) |
| `alerts.rs` | `AlertEvent`, `AlertSink` (`Noop` / `Logging` / `Webhook`), provider adapters (Generic / Slack / Discord), `mask_webhook_url`, `evaluate_execution` rule engine |

### Generating Documentation

```sh
cargo doc --no-deps --open
```

### PricingEngine Example

```rust
use peanut_internship_rust::{Address, ChainClient, PricingEngine, Token};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = ChainClient::new(vec!["http://127.0.0.1:8545".into()], 10, 1);
    let mut engine = PricingEngine::new(
        client,
        "http://127.0.0.1:8545",
        "ws://127.0.0.1:8545",
    )?;

    let pools = vec![
        Address::new("0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc")?,
        Address::new("0x0d4a11d5EEaaC28EC3F61d100daF4d40471f1852")?,
    ];
    engine.load_pools(&pools).await?;

    let weth = Token {
        address: Address::new("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")?,
        symbol: "WETH".into(),
        decimals: 18,
    };
    let usdc = Token {
        address: Address::new("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")?,
        symbol: "USDC".into(),
        decimals: 6,
    };

    let quote = engine
        .get_quote(
            &weth,
            &usdc,
            1_000_000_000_000_000_000,
            25,
            Address::new("0x00000000000000000000000000000000000000aa")?,
        )
        .await?;

    println!("Quote valid: {}", quote.is_valid());
    println!("Expected out: {}", quote.expected_output);
    println!("Simulated out: {}", quote.simulated_output);
    Ok(())
}
```

### Running Tests

```sh
# all tests
make test

# specific test suites
cargo test --test unit_core
cargo test --test unit_analyzer
cargo test --test unit_client
cargo test --test unit_builder
cargo test --test integration_wallet_security
cargo test --test integration_arb_flow   # Week 3 integration tests
```

### Test Coverage

**507 lib tests** (`cargo test --lib`), plus integration suites under `tests/`. Highlights:

- **core / chain / pricing**: address validation, wallet secrecy (Debug/Display never expose the private key), AMM math, router choice, fork-simulated quotes.
- **exchange / inventory**: order-book walking, spread/slippage, rate limiter, inventory skew + rebalance, PnL aggregation + CSV export.
- **strategy**: signal generation (direction / TTL / cooldown), weighted scoring, fee model.
- **executor**: full state machine (accepted → filled / rejected / failed / leg2_timeout / manual_review), race-on-cancel all branches, LEG1_PARTIAL classifier (Full / ProceedReduced / AbortUnwind / Dust), unwind on revert, reconcile worker lifecycle (receipt success / revert / pending / expired / RPC error), SQLite persistence across reopens.
- **recovery**: circuit breaker open/close + cool-off, replay protection in-memory and journaled.
- **observability**: Prometheus counter increments, webhook URL masking (Slack / Discord / bad input), alert rule evaluation per terminal state.
- **property tests (proptest)**: AMM invariants, router net-math consistency.
