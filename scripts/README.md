# Project Automation Scripts
 
- `scripts/start_fork.sh`: Starts local Anvil fork for pricing simulations (`ETH_RPC_URL` required).
- `scripts/start_fork.ps1`: PowerShell variant that loads `.env` and falls back to `MAINNET_RPC_URL` when `ETH_RPC_URL` is missing.
- `scripts/send_test_swap.ps1`: Sends a local test `swapExactETHForTokens` via `cast` using `.env` values (reads `ANVIL_DEFAULT_PRIVATE_KEY`/`PRIVATE_KEY`, no hardcoded secrets in repo).
- Use the `Makefile` as the primary interface for running and testing the project.

## CI Workflow (Github Actions / GitLab CI)
Automation scripts should:
1) Check formatting (`make format`)
2) Lint code (`make lint`)
3) Run security checks (pre-commit)
4) Run all tests (`make test`)
5) Ensure one-command deployment (`make start`)
