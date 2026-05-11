# Peanut Internship - Rust Project

An arbitrage detection and execution system spanning DEX (Uniswap V2/V3 on Arbitrum and EVM forks) and CEX (Binance) venues, built entirely in Rust.

Scope covers the full pipeline: WebSocket-backed order-book + pool monitoring → signal generation and scoring → fee/gas-aware profitability checks → dual-leg execution (CEX-first public DEX or DEX-first private bundle) with timeouts, cancel/replacement, partial-fill handling, unwind, nonce management, circuit breaker, persistent replay protection and reconciliation → inventory rebalance, Prometheus metrics, JSONL event logs, daily reports, live-readiness checks, and webhook/Telegram alerts.

### Quick Start
1. `cp .env.example .env`
2. Fill in API keys in `.env`
3. `make run` (or `cargo run --bin arb_bot -- --simulation=true --dry-run=true` for a safe dry run)
4. `make test`

### Environment
Exchange / chain basics:
- `PRIVATE_KEY`: hex-encoded Ethereum private key for local/testnet use
- `SEPOLIA_RPC_URL`: Sepolia RPC endpoint for integration work
- `MAINNET_RPC_URL`: optional mainnet RPC endpoint for analysis tooling
- `BINANCE_TESTNET_API_KEY` / `BINANCE_TESTNET_SECRET`: Binance testnet credentials

Live DEX execution (`arb_bot --dex-address-book ...`):
- `ETH_RPC_URL`: Arbitrum/mainnet/fork RPC endpoint used for live DEX pricing, quotes, gas estimation, nonce sync, and transaction submission
- `WALLET_ADDRESS`: 0x-address that signs and receives DEX output
- `WALLET_PRIVATE_KEY`: signing key (env-var name is configurable via `--wallet-key-env`)
- `FLASHBOTS_AUTH_PRIVATE_KEY`: private relay auth signing key used when private DEX mode is enabled (env-var name is configurable via `--flashbots-auth-key-env`)
- `DEX_ADDRESS_BOOK`: address-book JSON with token metadata plus optional `pool`, `pool_type`, V3 `fee`, `quoter`, `v3_path`, and `v3_fees`
- `ANVIL_FORK_URL`: optional fork RPC used by `--fee-gas-mode anvil`

Binance:
- `BINANCE_TESTNET_API_KEY` / `BINANCE_TESTNET_SECRET`: Binance testnet credentials
- `BINANCE_API_KEY` / `BINANCE_SECRET`: Binance production credentials when `--production=true` or `PRODUCTION=true`
- Signed Binance REST requests generate timestamps/signatures immediately before send, after local rate-limit waits, to avoid stale `recvWindow` signatures.

Observability / operations:
- `TRADE_LOG_PATH`: append-only trade JSONL path, default `trades.jsonl`
- `EVENT_LOG_PATH`: append-only observability JSONL path, default `events.jsonl`
- `TELEGRAM_WEBHOOK`, `TELEGRAM_CHAT_ID`, `ALERT_PROVIDER`: webhook alert configuration
- `HALT_FILE`: watchdog kill-switch file, default `/tmp/arb_bot_kill`

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
cargo run --bin arb_bot -- --simulation=true --dry-run=true

# Live Arbitrum CEX + public V3 DEX pricing/execution with persistent replay + reconcile.
cargo run --bin arb_bot -- \
    --pair LINK/ETH \
    --simulation=false \
    --dry-run=false \
    --eth-rpc-url "$ETH_RPC_URL" \
    --wallet-address "$WALLET_ADDRESS" \
    --dex-address-book configs/address_book_arbitrum.json \
    --fee-dex-swap-bps 0 \
    --fee-gas-mode estimate \
    --dex-slippage-bps 50 \
    --use-flashbots=false \
    --require-private-dex=false \
    --replay-db data/replay.db \
    --reconcile-db data/reconcile.db \
    --alert-webhook-url "$SLACK_WEBHOOK" --alert-provider slack \
    --metrics-port 9090 \
    --event-log-path logs/events.jsonl
```

Key flags:
- **`--simulation`** — in-process `SimulatedLegs`; never touches the exchange or chain.
- **`--dry-run`** — live CEX/DEX pricing with simulated legs. Safe for first-day production-data tests; seeded inventory is preserved in simulation/dry-run.
- **`--dex-address-book <path>`** — JSON of `{pair: {base, base_decimals, quote, quote_decimals, pool, pool_type, fee, quoter, v3_path, v3_fees}}`; enables live DEX pricing/execution via V2/V3 swappers. In live execution (`simulation=false`, `dry-run=false`) it is required and selected pairs must have compatible pools.
- **`--fee-dex-swap-bps 0`** — required when `--dex-address-book` is set, because live V2/V3 DEX quotes already include pool fees. Non-zero values fail fast instead of double-counting fees.
- **`--fee-gas-mode fixed|rpc|estimate|anvil`** — chooses gas fee modelling. `fixed` uses `--fee-gas-usd`; `rpc` uses current gas price with `--fee-gas-units`; `estimate` builds exact swap calldata and calls `eth_estimateGas`; `anvil` simulates exact calldata against `--anvil-fork-url` / `ANVIL_FORK_URL`.
- **`--use-flashbots true|false`** — when true, live DEX uses `FlashbotsSwapper` and the executor runs DEX-first. When false, live DEX uses public mempool submission and the executor runs CEX-first.
- **`--flashbots-relay-url <url>`** — Flashbots-compatible relay endpoint. Defaults to `https://relay.flashbots.net`.
- **`--flashbots-auth-key-env <name>`** — environment variable containing the Flashbots relay auth private key. Defaults to `FLASHBOTS_AUTH_PRIVATE_KEY`.
- **`--flashbots-target-block-offset <n>`** — first target block offset from the current head. Defaults to `1`.
- **`--flashbots-max-blocks-to-try <n>`** — number of consecutive target blocks to submit the signed bundle for. Defaults to `3`.
- **`--require-private-dex true|false`** — when true, missing Flashbots auth or missing DEX address book is a startup error. Set false to allow fallback to public DEX / CEX-first mode.
- **`--replay-db <path>`** — SQLite journal so the signal-id replay window survives restarts.
- **`--reconcile-db <path>`** — SQLite store of `LEG2_TIMEOUT` entries. Required for live execution so pending DEX outcomes survive restarts. A background worker polls each entry's receipt, marks it `Resolved` / `Reverted` / `Expired`, and on revert fires `LegExecutor::unwind_position` to flatten leg 1.
- **`--alert-webhook-url` / `--alert-provider`** — Generic, Slack, Discord, or Telegram adapter. The full URL is **never** logged — `mask_webhook_url` keeps only host + first path segment. Can also be set via `TELEGRAM_WEBHOOK`, `ALERT_PROVIDER`, and `TELEGRAM_CHAT_ID`.
- **`--metrics-port`** — Prometheus exporter port (S7a). Set `0` to disable the HTTP endpoint. Scrape `http://<host>:<port>/metrics`.
- **`--event-log-path <path>`** — append-only JSONL observability log for daily reporting. Set empty to disable. Captures bot lifecycle, terminal executions, DEX pending timeout/cancel outcomes, and reconcile lifecycle events.

Flashbots behaviour:
- The private DEX swapper builds and signs Uniswap V2 or V3 swap transactions locally, simulates with `eth_callBundle`, then submits the raw signed transaction bundle with `eth_sendBundle`.
- The same signed transaction is submitted for `max_blocks_to_try` consecutive target blocks. If no receipt appears by the last target block or inclusion timeout, the DEX leg returns `BundleNotIncluded` and the executor treats the signal as rejected without opening the CEX leg.
- Allowance approval is still public-chain state. Ensure the wallet has sufficient allowance before relying on fully private swaps, or allow the bot to perform the approval transaction first.

Live execution guardrails:
- `arb_bot` fails fast for live mode without an address book, without `--reconcile-db`, with unsupported/malformed CEX pair symbols, or with ambiguous token symbols in the selected address-book pairs.
- On Arbitrum, `USDC` must map to native USDC (`0xaf88...5831`); bridged USDC.e should be named distinctly, for example `USDC_E`.
- The default `https://relay.flashbots.net` relay is Ethereum mainnet-only; non-mainnet private mode must use a chain-specific relay or public DEX mode.
- Mixed V2/V3 live runs are supported, but custom `--dex-router` must be empty so V2 and V3 can use their own default routers.
- Public DEX pending timeouts attempt safe same-nonce cancellation/replacement when the original nonce is known; ambiguous outcomes are pushed to reconcile instead of guessed.

Prometheus metrics highlights:
- `peanut_flashbots_simulations_total`
- `peanut_flashbots_bundles_submitted_total`
- `peanut_flashbots_bundles_included_total`
- `peanut_flashbots_bundles_not_included_total`
- `peanut_flashbots_bundle_simulation_seconds`
- `peanut_flashbots_bundle_inclusion_blocks`
- `peanut_flashbots_relay_errors_total`
- `peanut_dex_pending_timeouts_total{pool_kind,private}`
- `peanut_dex_cancel_attempts_total{backend}`
- `peanut_dex_cancel_outcomes_total{outcome}`
- `peanut_reconcile_entries_total{reason}`
- `peanut_reconcile_resolved_total{outcome}`
- `peanut_reconcile_expired_total`
- `peanut_leg_outcomes_total{venue,leg,outcome}`

Daily-report JSONL event types:
- `bot_started` / `bot_stopped`
- `execution_terminal`
- `dex_pending_timeout`
- `dex_cancel_outcome`
- `reconcile_enqueued`
- `reconcile_enqueue_failed`
- `reconcile_resolved`
- `reconcile_inspect_error`

Generate a daily report from the JSONL event log:

```sh
cargo run --bin daily_report -- \
    --events logs/events.jsonl \
    --trade-log trades.jsonl \
    --reconcile-db data/reconcile.db \
    --metrics-snapshot reports/metrics.prom \
    --compare-report reports/2026-05-06/daily.json \
    --date 2026-05-07 \
    --format markdown \
    --output reports/2026-05-07.md \
    --alert-summary-output reports/2026-05-07-alert.json \
    --alert-summary-format telegram \
    --alert-telegram-chat-id "$TELEGRAM_CHAT_ID"

cargo run --bin daily_report -- --events logs/events.jsonl --format json
cargo run --bin daily_report -- --events logs/events.jsonl --format csv --output reports/latest.csv
cargo run --bin daily_report -- --events logs/events.jsonl --format html --output reports/latest.html
cargo run --bin daily_report -- --events logs/events.jsonl --fail-on-health yellow --output reports/latest.md
```

The report summarizes bot lifecycle, terminal executions, realized PnL from `execution_terminal`, optional trade-log PnL from `ArbRecord` JSONL, DEX pending timeout/cancel safety, reconcile outcomes, optional reconcile DB open/manual-review entries, optional Prometheus text snapshots for Flashbots/DEX/reconcile counters, a numeric `risk_score`, and top risk causes. Output formats are Markdown, JSON, CSV, and standalone HTML. Use `--compare-report reports/YYYY-MM-DD/daily.json` for day-over-day deltas, `--alert-summary-output alert.json` to write a compact alert payload for external notifiers, and `--fail-on-health yellow` or `--fail-on-health red` in cron/CI jobs to exit with status `2` after writing the report when the health threshold is met.

`--alert-summary-format structured` writes the structured daily alert summary and includes a nested `telegram_payload`. `--alert-summary-format telegram` writes a Telegram Bot API `sendMessage` JSON body directly: `{ "chat_id": "...", "text": "...", "parse_mode": "HTML", "disable_web_page_preview": true }`. Pair it with `--alert-telegram-chat-id` or `TELEGRAM_CHAT_ID`, then post it to the existing Telegram webhook URL from a separate notifier job.

For a repeatable archive workflow that captures metrics and writes Markdown, JSON, and HTML artifacts under `reports/YYYY-MM-DD/`, use `.windsurf/workflows/daily-report.md`.

### Auto-rebalance

`arb_bot` can periodically inspect CEX/wallet inventory skew and generate guarded rebalance plans:

- **Enablement**: `--rebalance-enabled` / `REBALANCE_ENABLED`, with `--rebalance-dry-run=true` as the safe default.
- **Planning controls**: `REBALANCE_INTERVAL_SECS`, `REBALANCE_THRESHOLD_PCT`, `REBALANCE_QUOTE_ASSET`, `REBALANCE_ALLOWED_ASSETS`, `REBALANCE_ALLOWED_VENUES`, `REBALANCE_MAX_STEP_USD`, and `REBALANCE_MAX_SLIPPAGE_BPS`.
- **Execution guardrails**: live rebalance requires RPC, wallet signing config, CEX deposit address, allowlisted assets/venues, max-step USD checks, min-fill checks, balance verification, and journaling to `REBALANCE_JOURNAL_PATH`.
- **Supported routes**: CEX trade steps via Binance IOC orders, CEX→wallet withdrawals with Binance capital-config preflight and withdrawal-history polling, and wallet→CEX native/ERC-20 transfers with on-chain receipt plus Binance deposit-history polling.
- **Arbitrum WETH**: CEX→wallet ETH withdrawal can be wrapped to WETH using address-book token metadata; wallet→CEX WETH transfer is intentionally rejected until unwrap/deposit handling is implemented.

When `REBALANCE_PAUSE_TRADING=true`, arbitrage ticks pause while an active rebalance plan is executing. Rebalance trigger/step events are emitted through the same alert pipeline.

### Live readiness pre-flight

Before switching from simulation/dry-run to live execution, run the read-only readiness checker:

```sh
cargo run --bin live_readiness -- \
    --config configs/address_book_arbitrum.json \
    --reconcile-db data/reconcile.db \
    --events logs/events.jsonl \
    --metrics-snapshot reports/latest/metrics.prom \
    --eth-rpc-url "$ETH_RPC_URL" \
    --wallet-address "$WALLET_ADDRESS" \
    --simulation=false \
    --dry-run=false \
    --max-gas-gwei 1 \
    --format markdown
```

`live_readiness` validates address-book router/token/pool sanity, expected chain id, RPC health, wallet native/ERC-20 balances, token allowances to the configured router, reconcile DB open entries, recent observability events, selected Prometheus metrics, halt file state, and live risk caps. It only uses read-only RPC calls and exits with status `2` when any check is `FAIL` unless `--no-fail` is set. If RPC or wallet inputs are omitted, live chain/balance/allowance checks are marked `SKIP` while local config, DB, event, metrics, and risk checks still run. A missing event log is a `WARN` for fresh deployments; `simulation=true` is a `FAIL` because the checker is intended to gate live readiness, so pass `--simulation=false` for the final pre-live run. For a repeatable checklist, use `.windsurf/workflows/live-readiness.md`.

The bot respects a circuit breaker (N failures in a rolling window → cool-off), a replay window that REJECTS duplicate signal ids, a watchdog halt file, daily-loss and absolute-capital risk halts, inventory reservations around queued executions, and a configurable `max_concurrent_executions` cap. A `ctrl-c` cleanly drains the queue worker before shutdown.

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
        X2 --> X3[executor/dex_swapper<br/>UniswapV2Swapper / UniswapV3Swapper / FlashbotsSwapper]
        X2 --> X4[executor/recovery<br/>breaker + replay]
        X2 --> X5[executor/reconcile<br/>LEG2_TIMEOUT worker]
    end

    subgraph Observability
        Y1[observability/metrics<br/>Prometheus] --> Y3[observability/server]
        Y2[observability/alerts<br/>Slack / Discord / Generic / Telegram]
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
    L2 -->|filled| OUT2[DONE]
    L2 -->|reverted| UW[Unwind leg 1] --> OUT3[FAILED]
    L2 -->|timeout| RS[Reconcile store<br/>SQLite] --> OUT4[LEG2_TIMEOUT]
    RS -.poll.-> WK[Reconcile worker]
    WK -->|reverted| UW
    WK -->|success| OUT2
    WK -->|expired| OUT5[MANUAL_REVIEW]
```

### Repository Architecture
- `src/core/`: Base types (Address, TokenAmount, Token), WalletManager, CanonicalSerializer
- `src/chain/`: ChainClient (RPC + retry), TransactionBuilder, shared NonceManager, Flashbots relay client, TransactionAnalyzer
- `src/pricing/`: AMM math, V3 pool pricing, router, mempool monitor, fork simulator, pricing engine
- `src/exchange/`: Binance client, signed/rate-limited HTTP client, WebSocket order-book streams, order book analyzer, price oracle
- `src/inventory/`: Position tracker, rebalance planner/executor helpers, PnL engine, on-chain wallet sync
- `src/strategy/`: Signal generator (from venue prices), scorer (weighted 4-component), fee model
- `src/executor/`: Queue (priority + reservations), `Executor` state machine, `LegExecutor` trait with `SimulatedLegs` / `LiveLegs`, V2/V3 swappers, Flashbots/private bundle swapper, circuit breaker, replay protection, reconciliation worker
- `src/observability/`: Prometheus metrics exporter, JSONL event logger, webhook alert sinks (Generic / Slack / Discord / Telegram)
- `src/integration/`: `ArbChecker` — end-to-end arbitrage pipeline helper
- `src/safety/`: risk limits, pre-trade validation, and watchdog kill-switch controls
- `src/bin/`: CLI binaries (`arb_bot`, `live_readiness`, `daily_report`, `orderbook_cli`, `pnl_cli`, `analyzer`, `integration_test`, etc.)
- `tests/`: Unit and integration tests
- `scripts/`: Automation scripts
- `configs/`: Non-secret configuration
- `docs/`: Module technical guides

### Module Details

#### exchange/
| File | Purpose |
|------|---------|
| `config.rs` | Venue configuration (Binance/Bybit) from env vars |
| `client.rs` | REST API client (order book, balance, orders, fees, withdrawals/deposits, Binance filters) |
| `http_client.rs` | Rate-limited HTTP wrapper with fresh signed-request timestamp/signature generation |
| `binance.rs` | Binance-specific adapter for market data and trading |
| `bybit.rs` | Bybit-specific adapter for market data and trading |
| `orderbook.rs` | Walk-the-book, depth analysis, spread, imbalance |
| `rate_limiter.rs` | Token-bucket rate limiter (core cross-cutting concern) |
| `ws.rs` | Binance bookTicker/depth WebSocket streams and local order-book updates |
| `types.rs` | OrderBookSnapshot, OrderResult, NormalizedBalance, etc. |
| `errors.rs` | ExchangeError enum |

#### inventory/
| File | Purpose |
|------|---------|
| `tracker.rs` | Multi-venue position tracking, can_execute, skew detection |
| `rebalancer.rs` | Threshold-based rebalance planning with fee/accounting and executable step generation |
| `rebalance_executor.rs` | Rebalance execution helpers for trade/transfer steps |
| `pnl.rs` | Per-trade and aggregate PnL tracking, CSV export, authoritative USD fields |
| `types.rs` | Venue enum, TransferPlan, fee/minimum balance constants including LINK/WETH |
| `errors.rs` | InventoryError enum |

#### pricing/
| File | Purpose |
|------|---------|
| `amm.rs` | Uniswap V2 math, reserves, quote calculation, token metadata helpers |
| `v3/` | Uniswap V3 QuoterV2 calls, pool state decoding, swap-log parsing, ticks/liquidity/sqrt_price math |
| `router.rs` | Multi-hop route finding and comparison |
| `engine.rs` | High-level `PricingEngine` combining RPC and simulation |
| `mempool.rs` | Monitor and decode pending swaps for early arb detection |
| `simulator.rs` | `ForkSimulator` for zero-risk on-chain trade validation |
| `history.rs` | `HistoricalImpactAnalyzer` for backtesting price impact |
| `arb.rs` | `ArbDetector` — logic to find profitable CEX/DEX loops |

#### integration/
| File | Purpose |
|------|---------|
| `arb_checker.rs` | End-to-end arb check: DEX price + CEX book + inventory + costs |

#### strategy/
| File | Purpose |
|------|---------|
| `signal.rs` | `Signal` / `SignalParams`, direction, TTL, deterministic `signal_id` |
| `generator.rs` | `SignalGenerator` — converts venue prices into Signals, cooldown + inventory pre-filter, ETH/WETH effective balance handling |
| `scorer.rs` | 4-component weighted scorer (spread, liquidity, inventory, history) |
| `fees.rs` | Fee model and breakdown shared between scoring, gas estimation, and PnL |

#### executor/
| File | Purpose |
|------|---------|
| `engine.rs` | `Executor` state machine, `LegExecutor` trait (`SimulatedLegs`, `LiveLegs` with injected DEX swapper), CEX-first + Flashbots DEX-first flows, safe DEX cancel/reconcile, partial-fill classifier, unwind |
| `queue.rs` | Priority-scored `SignalQueue` + `QueueWorker` with `max_concurrent_executions` semaphore, inventory reservations, and rejected-completion sink |
| `dex_swapper.rs` | `DexSwapper` trait + production V2/V3 swappers, composite mixed-pool dispatcher, private `FlashbotsSwapper`, calldata builders, quotes, approvals, nonce-aware submit/cancel |
| `recovery.rs` | `CircuitBreaker` + `ReplayProtection` (in-memory or SQLite-backed journal) |
| `reconcile.rs` | `ReconcileStore` (SQLite) + `ReconcileWorker` — receipt polling, async `spawn_blocking` I/O, transitions Pending→Resolved/Reverted/Expired |
| `errors.rs` | `ExecutorError`, `ExecutorResult` |

#### observability/
| File | Purpose |
|------|---------|
| `metrics.rs` | Prometheus counters / histograms (`init_metrics`, `metrics_handle`) |
| `events.rs` | Append-only JSONL event logger (`init_event_logger`, `emit_event`) for daily reports |
| `server.rs` | Hyper-based `/metrics` exporter (`serve_metrics`) |
| `alerts.rs` | `AlertEvent`, `AlertSink` (`Noop` / `Logging` / `Webhook`), provider adapters (Generic / Slack / Discord / Telegram), `mask_webhook_url`, `evaluate_execution` rule engine |

#### bin/
| File | Purpose |
|------|---------|
| `arb_bot.rs` | Main live/dry-run arbitrage runtime |
| `live_readiness.rs` | Read-only live trading pre-flight checker with Markdown/JSON output |
| `daily_report.rs` | Daily operations report generator from JSONL observability events; supports Markdown, JSON, CSV, HTML, comparison, CI health gates, and alert summary output |

#### safety/
| File | Purpose |
|------|---------|
| `limits.rs` | `RiskManager` — daily loss limits, max trade size, trade frequency |
| `validator.rs` | `PreTradeValidator` — sanity checks for prices and spreads |
| `killswitch.rs` | Watchdog file monitor for emergency manual halts |

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

The project has extensive unit, binary, and integration coverage (`cargo test --lib`, focused `--bin` suites, and integration suites under `tests/`). Highlights:

- **core / chain / pricing**: address validation, wallet secrecy (Debug/Display never expose the private key), AMM/V3 math, route validation, nonce reservation/rollback, fork-simulated quotes.
- **exchange / inventory**: order-book walking, Binance filters, signed request freshness, spread/slippage, rate limiter, inventory skew + rebalance, WETH handling, PnL aggregation + CSV export.
- **strategy**: signal generation (direction / TTL / cooldown), weighted scoring, fee/gas model, ETH/WETH effective balance handling.
- **executor**: full state machine (accepted → done / rejected / failed / leg2_timeout / manual_review), race-on-cancel all branches, LEG1_PARTIAL classifier (Full / ProceedReduced / AbortUnwind / Dust), DEX unwind/cancel/reconcile paths, Flashbots bundle target-block calculation, reconcile worker lifecycle (receipt success / revert / pending / expired / RPC error), SQLite persistence across reopens.
- **recovery**: circuit breaker open/close + cool-off, replay protection in-memory and journaled.
- **observability**: Prometheus counter increments including Flashbots/DEX/reconcile lifecycle metrics, JSONL events, webhook URL masking (Slack / Discord / Telegram / bad input), alert rule evaluation per terminal state.
- **property tests (proptest)**: AMM invariants, router net-math consistency.
