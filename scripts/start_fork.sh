#!/usr/bin/env bash
set -euo pipefail

# Start a local Anvil fork for simulation testing.
# Requires: anvil (https://book.getfoundry.sh/getting-started/installation)
# Usage:
#   ETH_RPC_URL=https://... ./scripts/start_fork.sh
# Optional env:
#   FORK_PORT=8545
#   FORK_ACCOUNTS=10
#   FORK_BALANCE=10000
#   FORK_BLOCK_NUMBER=24848480

if ! command -v anvil >/dev/null 2>&1; then
  echo "anvil not found. Install Foundry:"
  echo "  curl -L https://foundry.paradigm.xyz | bash"
  echo "  foundryup"
  exit 1
fi

if [[ -z "${ETH_RPC_URL:-}" ]]; then
  echo "ETH_RPC_URL is required"
  exit 1
fi

FORK_PORT="${FORK_PORT:-8545}"
FORK_ACCOUNTS="${FORK_ACCOUNTS:-10}"
FORK_BALANCE="${FORK_BALANCE:-10000}"

ANVIL_ARGS=(
  --fork-url "$ETH_RPC_URL"
  --port "$FORK_PORT"
  --accounts "$FORK_ACCOUNTS"
  --balance "$FORK_BALANCE"
)

if [[ -n "${FORK_BLOCK_NUMBER:-}" ]]; then
  if [[ "$FORK_BLOCK_NUMBER" =~ ^[0-9]+$ ]]; then
    ANVIL_ARGS+=(--fork-block-number "$FORK_BLOCK_NUMBER")
  else
    echo "FORK_BLOCK_NUMBER must be numeric"
    exit 1
  fi
fi

anvil "${ANVIL_ARGS[@]}"
