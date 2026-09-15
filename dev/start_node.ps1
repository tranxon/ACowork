#!/usr/bin/env pwsh
<#
.SYNOPSIS
    Start a local acowork-node daemon connected to a remote Gateway.
.DESCRIPTION
    Stops any running local Node / Runtimes / LSP relay, builds
    (incremental) and starts `acowork-node start` pointed at a Gateway
    reachable at the given HOST:PORT (the Gateway's MQTT broker port,
    default 19875). Auto-enrolls on first run — one command = deployed.

    ADR-055 §6.13.2: `acowork-node start --gateway HOST:PORT` is the
    canonical bootstrap; this script is a thin wrapper that resolves the
    target binary location for you.

    Use this on a "worker" machine that should host Agent Runtimes but
    has no Gateway of its own. The Gateway will discover it via the MQTT
    control plane.

    Restart semantics: an existing Node is stopped first — a second Node
    sharing the same identity would fight the first one over the broker
    session. Runtimes and the LSP relay are swept too: the Node keeps
    Runtimes alive across its own death by design (ADR-055 §6.10), and a
    Node killed abruptly (closed console window, force-kill) never ran
    its graceful shutdown, so those orphans would otherwise race the new
    Node's Runtimes over the broker (same instance identity => mutual
    MQTT takeover).
.PARAMETER GatewayHost
    Gateway MQTT broker `HOST:PORT` (e.g. `192.168.1.10:19875`).
.PARAMETER Addr
    This node's public reverse-proxy address (`HOST:PORT`). Default
    `auto` — live-detect the LAN IP at connect time (ADR-055 §6.3.3).
.PARAMETER Debug
    Build/run the debug profile (`target\debug`). Takes precedence over
    -Release and $env:ACOWORK_BUILD_PROFILE.
.PARAMETER Release
    Build/run the release profile (`target\release`) — the default.
.PARAMETER NoStop
    Do NOT stop an already-running Node / Runtimes / LSP relay
    (diagnostics only).
.PARAMETER NoBuild
    Skip the build step; fail loudly if the binary is missing instead
    of silently compiling.
.EXAMPLE
    .\dev\start_node.ps1 '192.168.1.10:19875'
.EXAMPLE
    .\dev\start_node.ps1 gateway.local:19875 -Addr auto -Debug
#>

# NOTE: PowerShell treats a bare `HOST:PORT` as a drive/scope path. Quote the
# argument so the colon reaches the script:
#   .\dev\start_node.ps1 '192.168.1.10:19875'
#   .\dev\start_node.ps1 gateway.local:19875 -Addr auto
# Unquoted `1.2.3.4:19875` fails with "A positional parameter cannot be
# found that accepts argument …" — the colon is PSH syntax, not ours.

# NOTE: deliberately a simple (non-advanced) param block — no [Parameter()]
# attributes and no [CmdletBinding()]. Either one would make this an
# advanced script, whose common parameter set reserves -Debug and collides
# with our -Debug switch ("A parameter with the name 'Debug' was defined
# multiple times"). Position 0 comes from declaration order, so the
# mandatory gateway host is validated manually below.
param(
    [string]$GatewayHost,

    [string]$Addr = "auto",

    [switch]$Debug,

    [switch]$Release,

    [switch]$NoStop,

    [switch]$NoBuild
)

$ErrorActionPreference = "Stop"

if ([string]::IsNullOrEmpty($GatewayHost)) {
    Write-Host "[node] ERROR: missing GATEWAY_HOST:PORT (first positional argument)." -ForegroundColor Red
    Write-Host "[node] Usage: .\dev\start_node.ps1 '192.168.1.10:19875' [-Addr HOST:PORT] [-Debug|-Release] [-NoStop] [-NoBuild]" -ForegroundColor Red
    exit 1
}

# ── Profile resolution ──────────────────────────────────────────────

# -Debug / -Release switch > $env:ACOWORK_BUILD_PROFILE > release
# (mirrors dev/build_core.ps1). The profile also selects the target dir:
# debug -> target\debug, release -> target\release.
if ($Debug -and $Release) {
    Write-Host "[node] ERROR: -Debug and -Release are mutually exclusive." -ForegroundColor Red
    exit 1
}
$Profile = "release"
if ($Debug) { $Profile = "debug" }
elseif ($Release) { $Profile = "release" }
elseif ($env:ACOWORK_BUILD_PROFILE) {
    $envProfile = $env:ACOWORK_BUILD_PROFILE.Trim().ToLower()
    if ($envProfile -eq "debug" -or $envProfile -eq "release") {
        $Profile = $envProfile
    } else {
        Write-Host "[node] WARN: ignoring unknown ACOWORK_BUILD_PROFILE='$envProfile' (expected 'debug' or 'release')" -ForegroundColor Yellow
    }
}

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$ProjectRoot = Split-Path -Parent $ScriptDir
$CoreDir = Join-Path $ProjectRoot "core"
$Bin = Join-Path $ProjectRoot "target\$Profile\acowork-node.exe"

# Node spawns its agent Runtimes and LSP sidecars from sibling binaries
# (current_exe().parent()), so all three must live in the same target dir:
#   acowork-runtime.exe   — spawn_agent_process (ADR-055)
#   acowork-lsp-relay.exe — spawn_lsp_relay (sidecar)
$SiblingBins = @("acowork-runtime.exe", "acowork-lsp-relay.exe")

function Write-Step { param([string]$msg) Write-Host "[node] $msg" -ForegroundColor Cyan }
function Write-Ok   { param([string]$msg) Write-Host "[node] $msg" -ForegroundColor Green }
function Write-Err  { param([string]$msg) Write-Host "[node] $msg" -ForegroundColor Red }

# ── Stop (default) ──────────────────────────────────────────────────

# Order matters: stop everything BEFORE cargo builds — a running .exe is
# file-locked on Windows and cargo cannot replace it ("Access is denied",
# os error 5; same reasoning as build_core.ps1). This sweeps every
# acowork-* process on the machine, matching build_core.ps1's stop step:
#   - acowork-runtime: the Node keeps Runtimes alive across its own death
#     by design (ADR-055 §6.10), and a Node killed abruptly (e.g. a closed
#     console window -> CTRL_CLOSE_EVENT) never runs its graceful
#     shutdown — the survivors would otherwise race the new Node's
#     Runtimes over the broker (same instance identity => mutual MQTT
#     takeover).
#   - acowork-lsp-relay: sidecar on port 19878; a stale relay makes the
#     next Node's relay spawn fail on the port.
if (-not $NoStop) {
    Write-Step "Stopping existing Node / Runtimes / LSP relay (if any)..."
    $anyStopped = $false
    foreach ($procName in @("acowork-node", "acowork-runtime", "acowork-lsp-relay")) {
        $procs = Get-Process -Name $procName -ErrorAction SilentlyContinue
        if ($procs) {
            Write-Host "  Found ${procName}: $($procs.Id -join ', ')" -ForegroundColor Gray
            Stop-Process -Name $procName -Force -ErrorAction SilentlyContinue
            $anyStopped = $true
        }
    }
    if ($anyStopped) {
        # Stop-Process returns before listeners are guaranteed gone; wait
        # for the Node proxy (19900) and LSP relay (19878) ports to be
        # released (same pattern as build_core.ps1).
        foreach ($port in @(19900, 19878)) {
            $waited = 0
            while ($waited -lt 6) {
                $stillUp = netstat -ano 2>$null | Select-String ":$port\s"
                if (-not $stillUp) { break }
                Start-Sleep -Milliseconds 500
                $waited++
            }
            if ($waited -ge 6) {
                Write-Host "  WARNING: port $port still in use after 3s" -ForegroundColor Yellow
            }
        }
        Write-Ok "  Stopped."
    } else {
        Write-Host "  Nothing to stop." -ForegroundColor Gray
    }
}

# ── Build (incremental, on by default) ──────────────────────────────

# Always invoke cargo: cargo is the only reliable arbiter of "needs a
# rebuild" (source freshness, profile, features). With an up-to-date
# target dir this takes a few seconds; -NoBuild skips it entirely.
if (-not $NoBuild) {
    Write-Step "Building acowork-node + runtime + lsp-relay ($Profile, incremental)..."
    $cargoArgs = @(
        "build",
        "--manifest-path", (Join-Path $CoreDir "Cargo.toml"),
        "-p", "acowork-node",
        "-p", "acowork-runtime",
        "-p", "acowork-lsp-relay"
    )
    if ($Profile -eq "release") { $cargoArgs += "--release" }
    & cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}

# Fail loudly when a required binary is still missing (e.g. -NoBuild on
# a fresh checkout).
$requiredBins = @((Split-Path $Bin -Leaf)) + $SiblingBins
foreach ($name in $requiredBins) {
    $path = Join-Path (Split-Path $Bin) $name
    if (-not (Test-Path $path)) {
        Write-Err "Binary not found at $path — run dev/build_core.ps1 first (or drop -NoBuild)."
        exit 1
    }
}

# ── Start ───────────────────────────────────────────────────────────

Write-Step "Starting acowork-node -> gateway $GatewayHost  (profile=$Profile)"
Write-Step "Ctrl+C to stop. Logs: $env:USERPROFILE\.acowork\acowork-node\logs"

& $Bin start --gateway $GatewayHost --addr $Addr
exit $LASTEXITCODE
