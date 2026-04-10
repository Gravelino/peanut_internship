param(
    [string]$EnvFile = ".env",
    [int]$ForkPort = 8545,
    [int]$ForkAccounts = 10,
    [int]$ForkBalance = 10000,
    [string]$ForkBlockNumber
)

$ErrorActionPreference = "Stop"

function Load-DotEnv {
    param([string]$Path)

    if (-not (Test-Path $Path)) {
        return
    }

    Get-Content $Path | ForEach-Object {
        if ($_ -match '^\s*([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.*)\s*$') {
            $name = $matches[1]
            $value = $matches[2].Trim().Trim('"')
            [Environment]::SetEnvironmentVariable($name, $value, 'Process')
        }
    }
}

function Get-AnvilPath {
    $cmd = Get-Command anvil -ErrorAction SilentlyContinue
    if ($cmd) {
        return $cmd.Source
    }

    $fallback = Join-Path $env:USERPROFILE "foundry\bin\anvil.exe"
    if (Test-Path $fallback) {
        return $fallback
    }

    throw "anvil not found. Install Foundry and ensure anvil is in PATH."
}

Load-DotEnv -Path $EnvFile

if (-not $env:ETH_RPC_URL -and $env:MAINNET_RPC_URL) {
    $env:ETH_RPC_URL = $env:MAINNET_RPC_URL
}

if (-not $env:ETH_RPC_URL) {
    throw "ETH_RPC_URL is required (or set MAINNET_RPC_URL in .env)."
}

$args = @(
    "--fork-url", $env:ETH_RPC_URL,
    "--port", $ForkPort,
    "--accounts", $ForkAccounts,
    "--balance", $ForkBalance
)

if ($ForkBlockNumber) {
    if ($ForkBlockNumber -match '^\d+$') {
        $args += @("--fork-block-number", $ForkBlockNumber)
    }
    else {
        throw "ForkBlockNumber must be numeric for this Anvil version."
    }
}

$anvilPath = Get-AnvilPath
Write-Host "Starting Anvil fork on port $ForkPort using $anvilPath"
& $anvilPath @args
