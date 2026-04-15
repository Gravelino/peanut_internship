param(
    [string]$EnvFile = ".env",
    [string]$RpcUrl = "http://127.0.0.1:8545",
    [string]$Router = "0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D",
    [string]$Path = "[0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2,0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48]",
    [string]$Recipient = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
    [string]$ValueWei = "10000000000000000",
    [string]$AmountOutMin = "0",
    [string]$Deadline = "9999999999",
    [string]$GasLimit = "500000",
    [string]$PrivateKey
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

function Get-CastPath {
    $cmd = Get-Command cast -ErrorAction SilentlyContinue
    if ($cmd) {
        return $cmd.Source
    }

    $fallbackA = Join-Path $env:USERPROFILE "foundry\bin\cast.exe"
    if (Test-Path $fallbackA) {
        return $fallbackA
    }

    $fallbackB = Join-Path $env:USERPROFILE ".cargo\bin\cast.exe"
    if (Test-Path $fallbackB) {
        return $fallbackB
    }

    throw "cast not found. Install Foundry and ensure cast is in PATH."
}

Load-DotEnv -Path $EnvFile

if (-not $PrivateKey) {
    # Prefer dedicated local key for Anvil if provided; otherwise fall back to generic PRIVATE_KEY.
    if ($RpcUrl -match '^https?://(127\.0\.0\.1|localhost)(:\d+)?/?$') {
        $PrivateKey = $env:ANVIL_DEFAULT_PRIVATE_KEY
        if (-not $PrivateKey) {
            $PrivateKey = $env:PRIVATE_KEY
        }
    }
    else {
        $PrivateKey = $env:PRIVATE_KEY
    }
}

if (-not $PrivateKey) {
    throw "No private key available. Pass -PrivateKey or set ANVIL_DEFAULT_PRIVATE_KEY / PRIVATE_KEY in .env."
}

$castPath = Get-CastPath

Write-Host "Sending test swap via router $Router"
Write-Host "RPC: $RpcUrl"
Write-Host "Path: $Path"

$args = @(
    "send",
    "--rpc-url", $RpcUrl,
    "--private-key", $PrivateKey,
    "--value", $ValueWei,
    $Router,
    "swapExactETHForTokens(uint256,address[],address,uint256)",
    $AmountOutMin,
    $Path,
    $Recipient,
    $Deadline,
    "--gas-limit", $GasLimit
)

& $castPath @args
