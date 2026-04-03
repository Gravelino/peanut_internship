# Peanut Internship - Rust Project

Week 1 introduces the core Ethereum foundation in Rust:
- `core`: addresses, token amounts, deterministic serialization, wallet signing
- `chain`: RPC client, receipt parsing, transaction helpers

### Quick Start
1. `cp .env.example .env`
2. `make run`
3. `make test`
4. `cargo run --bin check_env_and_rpc`

### Environment
- `PRIVATE_KEY`: hex-encoded Ethereum private key for local/testnet use
- `SEPOLIA_RPC_URL`: Sepolia RPC endpoint for integration work
- `MAINNET_RPC_URL`: optional mainnet RPC endpoint for analysis tooling

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
# text output (default)
cargo run --bin analyzer -- <tx_hash> --rpc <url>

# JSON output for programmatic use
cargo run --bin analyzer -- <tx_hash> --format json

# or rely on MAINNET_RPC_URL from .env
cargo run --bin analyzer -- <tx_hash>
```

Features:
- Decodes ERC-20 (`transfer`, `approve`, `transferFrom`) and Uniswap V2/V3 function calls
- Parses event logs (Transfer, Swap, Sync events)
- Shows revert reason for failed transactions
- Supports `--format json` for programmatic output

### Repository Architecture
- `src/core/`: Base types (Address, TokenAmount, Token), WalletManager, CanonicalSerializer
- `src/chain/`: ChainClient (RPC + retry), TransactionBuilder, TransactionAnalyzer
- `src/bin/`: CLI binaries (analyzer, check_env_and_rpc, integration_test)
- `tests/`: Unit and integration tests
- `scripts/`: Automation scripts
- `configs/`: Non-secret configuration
- `docs/`: Module technical guides

### Running Tests

```sh
# all tests
make test

# specific test suites
cargo test --test unit_core         # base types + serializer + wallet
cargo test --test unit_analyzer     # analyzer selectors, events, errors
cargo test --test unit_client       # RPC error classification + retry
cargo test --test unit_builder      # transaction builder
cargo test --test integration_wallet_security  # keyfile + signing security
```
