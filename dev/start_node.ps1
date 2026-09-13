#!/usr/bin/env pwsh
<#
.SYNOPSIS
    Start a local acowork-node daemon connected to a remote Gateway.
.DESCRIPTION
    Locally builds (if needed) and starts `acowork-node start` pointed at a
    Gateway reachable at the given HOST:PORT (the Gateway's MQTT broker
    port, default 19875). Auto-enrolls on first run — one command =
    deployed.

    ADR-055 §6.13.2: `acowork-node start --gateway HOST:PORT` is the
    canonical bootstrap; this script is a thin wrapper that resolves the
    target binary location for you.

    Use this on a "worker" machine that should host Agent Runtimes but
    has no Gateway of its own. The Gateway will discover it via the MQTT
    control plane.
.PARAMETER GatewayHost
    Gateway MQTT broker `HOST:PORT` (e.g. `192.168.1.10:19875`).
.PARAMETER Addr
    This node's public reverse-proxy address (`HOST:PORT`). Default
    `auto` — live-detect the LAN IP at connect time (ADR-055 §6.3.3).
.PARAMETER Profile
    Build profile to use if the binary is missing: `debug` or `release`
    (default `release`).
.PARAMETER ForceBuild
    Rebuild acowork-node even when the binary already exists.
.PARAMETER NoBuild
    Skip the auto-build step; fail loudly if the binary is missing
    instead of silently compiling.
.EXAMPLE
    .\dev\start_node.ps1 '192.168.1.10:19875'
.EXAMPLE
    .\dev\start_node.ps1 gateway.local:19875 -Addr auto
#>

# NOTE: PowerShell treats a bare `HOST:PORT` as a drive/scope path. Quote the
# argument so the colon reaches the script:
#   .\dev\start_node.ps1 '192.168.1.10:19875'
#   .\dev\start_node.ps1 gateway.local:19875 -Addr auto
# Unquoted `1.2.3.4:19875` fails with "A positional parameter cannot be
# found that accepts argument …" — the colon is PSH syntax, not ours.

param(
    [Parameter(Position = 0, Mandatory = $true)]
    [string]$GatewayHost,

    [Parameter()]
    [string]$Addr = "auto",

    [Parameter()]
    [ValidateSet("debug", "release")]
    [string]$Profile = "release",

    [Parameter()]
    [switch]$ForceBuild,

    [Parameter()]
    [switch]$NoBuild
)

$ErrorActionPreference = "Stop"
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$ProjectRoot = Split-Path -Parent $ScriptDir
$CoreDir = Join-Path $ProjectRoot "core"
$Bin = Join-Path $ProjectRoot "target\$Profile\acowork-node.exe"

# Node spawns its agent Runtimes and LSP sidecars from sibling binaries
# (current_exe().parent()), so all three must live in the same target dir:
#   acowork-runtime.exe   — spawn_agent_process (ADR-055)
#   acowork-lsp-relay.exe — spawn_lsp_relay (sidecar)
$SiblingBins = @("acowork-runtime.exe", "acowork-lsp-relay.exe")
$missingSiblings = @($SiblingBins | Where-Object { -not (Test-Path (Join-Path (Split-Path $Bin) $_)) })

function Write-Step   { param([string]$msg) Write-Host "[node] $msg" -ForegroundColor Cyan }
function Write-Ok     { param([string]$msg) Write-Host "[node] $msg" -ForegroundColor Green }
function Write-Err    { param([string]$msg) Write-Host "[node] $msg" -ForegroundColor Red }

# ── Build if needed ────────────────────────────────────────────────

$needsBuild = $ForceBuild -or -not (Test-Path $Bin) -or ($missingSiblings.Count -gt 0)
if ($needsBuild -and $NoBuild) {
    Write-Err "Binary not found at $Bin or siblings ($($missingSiblings -join ', ')) and -NoBuild was given. Run dev/build_core.ps1 first."
    exit 1
}
if ($needsBuild) {
    Write-Step "Building acowork-node + runtime + lsp-relay ($Profile) — first run, this takes a few minutes..."
    cargo build --manifest-path (Join-Path $CoreDir "Cargo.toml") -p acowork-node -p acowork-runtime -p acowork-lsp-relay --profile $Profile
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}

# ── Start ───────────────────────────────────────────────────────────

Write-Step "Starting acowork-node -> gateway $GatewayHost  (profile=$Profile)"
Write-Step "Ctrl+C to stop. Logs: $env:USERPROFILE\.acowork\acowork-node\data\logs"

& $Bin start --gateway $GatewayHost --addr $Addr
exit $LASTEXITCODE