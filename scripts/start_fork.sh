#!/usr/bin/env bash
set -euo pipefail

# Start a local Anvil fork for simulation testing.
# Requires: anvil (https://book.getfoundry.sh/getting-started/installation)
#
# Usage:
#   ./scripts/start_fork.sh
#   ./scripts/start_fork.sh --port 8546
#   MAINNET_RPC_URL=https://... ./scripts/start_fork.sh
#
# The script reads RPC URL from:
#   1. MAINNET_RPC_URL env var
#   2. ETH_RPC_URL env var
#   3. .env file in project root (MAINNET_RPC_URL)
#
# Optional env:
#   FORK_PORT=8545
#   FORK_ACCOUNTS=10
#   FORK_BALANCE=10000
#   FORK_BLOCK_NUMBER=24848480

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Load .env if it exists (macOS compatible — no bash 4+ features needed)
if [[ -f "$PROJECT_ROOT/.env" ]]; then
    # Safe source: skip empty lines and comments, export vars
    while IFS='=' read -r key value; do
        key="$(echo "$key" | xargs)"
        value="$(echo "$value" | xargs)"
        if [[ -n "$key" && ! "$key" =~ ^# ]]; then
            export "$key=$value"
        fi
    done < "$PROJECT_ROOT/.env"
    echo "Loaded .env from $PROJECT_ROOT"
fi

# Resolve RPC URL (prefer MAINNET_RPC_URL, fall back to ETH_RPC_URL)
RPC_URL="${MAINNET_RPC_URL:-${ETH_RPC_URL:-}}"

if [[ -z "$RPC_URL" ]]; then
    echo "ERROR: No RPC URL configured."
    echo ""
    echo "Set one of:"
    echo "  export MAINNET_RPC_URL=https://eth-mainnet.g.alchemy.com/v2/YOUR_KEY"
    echo "  export ETH_RPC_URL=https://eth-mainnet.g.alchemy.com/v2/YOUR_KEY"
    echo ""
    echo "Or add MAINNET_RPC_URL=... to $PROJECT_ROOT/.env"
    exit 1
fi

# Check for anvil
if ! command -v anvil >/dev/null 2>&1; then
    # Try foundry bin dir (common on macOS after fresh install)
    if [[ -x "$HOME/.foundry/bin/anvil" ]]; then
        export PATH="$HOME/.foundry/bin:$PATH"
    else
        echo "ERROR: anvil not found. Install Foundry:"
        echo "  curl -L https://foundry.paradigm.xyz | bash"
        echo "  source \$HOME/.zshenv  # or restart terminal"
        echo "  foundryup"
        exit 1
    fi
fi

FORK_PORT="${FORK_PORT:-8545}"
FORK_ACCOUNTS="${FORK_ACCOUNTS:-5}"
FORK_BALANCE="${FORK_BALANCE:-10000}"

ANVIL_ARGS=(
    --fork-url "$RPC_URL"
    --port "$FORK_PORT"
    --accounts "$FORK_ACCOUNTS"
    --balance "$FORK_BALANCE"
)

if [[ -n "${FORK_BLOCK_NUMBER:-}" ]]; then
    if [[ "$FORK_BLOCK_NUMBER" =~ ^[0-9]+$ ]]; then
        ANVIL_ARGS+=(--fork-block-number "$FORK_BLOCK_NUMBER")
    else
        echo "ERROR: FORK_BLOCK_NUMBER must be numeric"
        exit 1
    fi
fi

echo "Starting Anvil fork..."
echo "  RPC:       $RPC_URL"
echo "  Port:      $FORK_PORT"
echo "  Accounts:  $FORK_ACCOUNTS"
echo "  Balance:   $FORK_BALANCE ETH each"
echo ""

exec anvil "${ANVIL_ARGS[@]}"
