#!/usr/bin/env pwsh
# build_core.ps1 - Build Gateway + Runtime + Node Agent (debug or release mode)
# Usage:
#   .\dev\build_core.ps1                  Build release (default)
#   .\dev\build_core.ps1 -Debug           Build debug
#   .\dev\build_core.ps1 -Release         Build release (explicit)
#   .\dev\build_core.ps1 -Start                 Build release + stop old + start Gateway (loopback)
#   .\dev\build_core.ps1 -Debug -Start          Build debug + stop old + start Gateway (loopback)
#   .\dev\build_core.ps1 -Stop                  Build release + stop old Gateway
#   .\dev\build_core.ps1 -Debug -Stop           Build debug + stop old Gateway
#   .\dev\build_core.ps1 -Start -Local          Build + start Gateway bound to 127.0.0.1 only (default)
#   .\dev\build_core.ps1 -Start -Remote         Build + start Gateway bound to 0.0.0.0 (LAN-reachable)
#                                               and advertise the detected LAN IP. Implies -Start.
#   .\dev\build_core.ps1 -Debug -Start -Remote  Build debug + start Gateway in remote (LAN) mode
#
# Profile selection: -Debug / -Release switch > $env:ACOWORK_BUILD_PROFILE > release
# In debug profile, $env:ACOWORK_GATEWAY_LOG_LEVEL is auto-set to "debug" so any
# gateway process spawned from this script's process tree (including -Start
# and a manual `target\debug\acowork-gateway.exe` invocation from the same
# terminal) inherits verbose logging.

param(
    [switch] $Start,
    [switch] $Stop,
    [switch] $Debug,
    [switch] $Release,
    [switch] $Local,
    [switch] $Remote
)

$ErrorActionPreference = "Stop"
$WorkspaceRoot = Split-Path -Parent $PSScriptRoot
$CoreDir = Join-Path $WorkspaceRoot "core"

# Resolve profile: CLI switch > $env:ACOWORK_BUILD_PROFILE > default (release)
$Profile = "release"
if ($Debug -and $Release) {
    Write-Host "ERROR: -Debug and -Release are mutually exclusive." -ForegroundColor Red
    exit 1
}
if ($Debug) { $Profile = "debug" }
elseif ($Release) { $Profile = "release" }
elseif ($env:ACOWORK_BUILD_PROFILE) {
    $envProfile = $env:ACOWORK_BUILD_PROFILE.Trim().ToLower()
    if ($envProfile -eq "debug" -or $envProfile -eq "release") {
        $Profile = $envProfile
    } else {
        Write-Host "WARN: ignoring unknown ACOWORK_BUILD_PROFILE='$envProfile' (expected 'debug' or 'release')" -ForegroundColor Yellow
    }
}

# Resolve network mode: -Local/-Remote only meaningful with -Start. -Remote
# implicitly turns -Start on (matches the macOS/Linux scripts' convention).
if ($Local -and $Remote) {
    Write-Host "ERROR: -Local and -Remote are mutually exclusive." -ForegroundColor Red
    exit 1
}
if ($Remote -and -not $Start) {
    Write-Host "INFO: -Remote implies -Start (Gateway needs to be started for bind mode to take effect)." -ForegroundColor Cyan
    $Start = $true
}
if (($Local -or $Remote) -and $Stop) {
    Write-Host "ERROR: -Local/-Remote cannot be combined with -Stop (no Gateway is started)." -ForegroundColor Red
    exit 1
}
$NetworkMode = if ($Remote) { "remote" } elseif ($Local) { "local" } else { "default" }

# Runtime env linkage: debug profile auto-enables gateway verbose logging for
# any child process spawned from this script.
if ($Profile -eq "debug") {
    $env:ACOWORK_GATEWAY_LOG_LEVEL = "debug"
}

# Build the gateway bind/advertise args from $NetworkMode. The Gateway CLI
# (core/acowork-gateway/src/cli.rs) accepts:
#   --addr HOST:PORT          HTTP bind   (default 127.0.0.1:19876)
#   --mqtt-addr HOST:PORT     MQTT bind   (default 127.0.0.1:19875)
#   --advertise-host HOST     IP distributed to Node Agents / Desktop
# For -Local we bind loopback and pin --advertise-host to 127.0.0.1 (same
# as the Desktop app) so published endpoints (embed /v1, package download
# URLs) stay loopback-reachable instead of leaking a LAN IP that a
# loopback-bound Gateway cannot serve; for -Remote we bind 0.0.0.0 and
# pin the detected LAN IP so Node Agents on other machines reach a stable
# host instead of relying on the gateway's auto-detect fallback (which
# warns).
$GatewayArgs = @()
if ($NetworkMode -eq "local") {
    $GatewayArgs = @("--addr", "127.0.0.1:19876", "--mqtt-addr", "127.0.0.1:19875", "--advertise-host", "127.0.0.1")
} elseif ($NetworkMode -eq "remote") {
    # Pick the address LAN peers will actually use to reach this host.
    # Preference order:
    #   1. $env:ACOWORK_ADVERTISE_HOST — explicit operator override.
    #   2. The IPv4 of the interface that owns the default route. Virtual
    #      adapters (VMware VMnet, WSL vEthernet, Hyper-V) and APIPA
    #      (169.254.x) never own the default route, so this cannot pick a
    #      non-LAN-reachable address by accident — the old
    #      "first non-loopback IPv4" heuristic did (VMnet8 sorts first).
    #   3. First non-loopback, non-APIPA IPv4 (fully offline lab fallback).
    # When all probes fail the Gateway auto-detects and logs a WARN at
    # startup. Upgrade path: honor [advertise_host] from gateway.toml when
    # present (single source of truth for the operator).
    $lanIp = $env:ACOWORK_ADVERTISE_HOST
    if (-not $lanIp) {
        try {
            $defaultRoute = Get-NetRoute -DestinationPrefix '0.0.0.0/0' -ErrorAction SilentlyContinue |
                Sort-Object RouteMetric, InterfaceMetric | Select-Object -First 1
            if ($defaultRoute) {
                $lanIp = Get-NetIPAddress -AddressFamily IPv4 -InterfaceIndex $defaultRoute.InterfaceIndex -ErrorAction SilentlyContinue |
                    Where-Object { $_.IPAddress -ne '127.0.0.1' } |
                    Select-Object -First 1 -ExpandProperty IPAddress
            }
            if (-not $lanIp) {
                $lanIp = Get-NetIPAddress -AddressFamily IPv4 -ErrorAction SilentlyContinue |
                    Where-Object { $_.IPAddress -ne '127.0.0.1' -and $_.IPAddress -notmatch '^169\.254\.' } |
                    Select-Object -First 1 -ExpandProperty IPAddress
            }
        } catch { $lanIp = $null }
    }
    # Bind 0.0.0.0 unconditionally — that is the whole point of -Remote; the
    # advertise host is best-effort and only appended when detected (the
    # Gateway auto-detects a good value itself, via its UDP route probe).
    $GatewayArgs = @("--addr", "0.0.0.0:19876", "--mqtt-addr", "0.0.0.0:19875")
    if (-not $lanIp) {
        Write-Host "WARN: could not detect a LAN-reachable IPv4 address for -Remote; binding 0.0.0.0 and letting the Gateway auto-detect the advertise host. Set [advertise_host] in gateway.toml (or `$env:ACOWORK_ADVERTISE_HOST) for a deterministic value." -ForegroundColor Yellow
    } else {
        $GatewayArgs += @("--advertise-host", $lanIp)
        Write-Host "Remote mode: Gateway will bind 0.0.0.0 and advertise $lanIp" -ForegroundColor Cyan
    }
}

$targetDir = Join-Path $WorkspaceRoot "target\$Profile"
# Step count:
#   -Start : Stop, Gateway, Runtime, Embed, LSP Relay, Node Agent, PM, Doc, Copy resources, Start (10)
#   -Stop  : Stop, Gateway, Runtime, Embed, LSP Relay, Node Agent, PM, Doc, Copy resources      (9)
#   else   :            Gateway, Runtime, Embed, LSP Relay, Node Agent, PM, Doc, Copy resources (8)
$totalSteps = if ($Start) { 10 } elseif ($Stop) { 9 } else { 8 }

Write-Host "========================================" -ForegroundColor Cyan
Write-Host "ACowork Core Build Script" -ForegroundColor Cyan
Write-Host "Profile: $Profile" -ForegroundColor Cyan
if ($Start) { Write-Host "Mode: Build + Restart" -ForegroundColor Cyan }
elseif ($Stop) { Write-Host "Mode: Build + Stop" -ForegroundColor Cyan }
else       { Write-Host "Mode: Build Only" -ForegroundColor Cyan }
if ($Start -and $NetworkMode -ne "default") {
    $bindDesc = if ($Remote) { '0.0.0.0 + LAN advertise' } else { '127.0.0.1' }
    Write-Host "Network: $NetworkMode (Gateway $bindDesc)" -ForegroundColor Cyan
}
Write-Host "========================================" -ForegroundColor Cyan
Write-Host ""

$step = 0

if ($Start -or $Stop) {
    # Step: Stop running processes
    $step++
    Write-Host "[$step/$totalSteps] Stopping running Desktop, Gateway, Runtime, Embed, LSP Relay, Node Agent, PM, and Doc processes..." -ForegroundColor Yellow

    $gatewayProcs = Get-Process -Name "acowork-gateway" -ErrorAction SilentlyContinue
    $runtimeProcs = Get-Process -Name "acowork-runtime" -ErrorAction SilentlyContinue
    $embedProcs   = Get-Process -Name "acowork-embed"   -ErrorAction SilentlyContinue
    # The LSP Relay runs in its own process group (see
    # core/acowork-gateway/src/lifecycle/lsp_relay.rs: cmd.process_group(0)),
    # so a Gateway shutdown does NOT cascade termination to it — we must
    # explicitly kill it to avoid leaving an orphan binding port 19878, which
    # would otherwise be attached by the new gateway via
    # attach_existing_lsp_relay() but owned by a now-dead parent.
    $lspProcs    = Get-Process -Name "acowork-lsp-relay" -ErrorAction SilentlyContinue
    # The Node Agent is spawned by the Gateway (ADR-055 §6.11). On Windows the
    # Gateway's orphan cleanup is skipped (no `ps`), so a node left behind by a
    # killed Gateway would keep running with the old broker connection — kill it
    # explicitly to keep the stop step idempotent.
    $nodeProcs   = Get-Process -Name "acowork-node"   -ErrorAction SilentlyContinue

    # The ACowork Desktop app (Tauri) embeds the Gateway as a sidecar. On
    # Windows the Tauri shell does NOT cascade-kill its sidecar, so killing
    # only the Gateway leaves the Desktop process alive — and within seconds
    # Desktop will respawn a fresh Gateway, which in turn spawns Node +
    # LSP Relay (ADR-055 §6.11). Those children hold file locks on their
    # .exe files, so subsequent cargo builds silently fail with
    # "Access is denied" (os error 5) when trying to replace those
    # binaries. Kill the Desktop FIRST so it cannot respawn anything we
    # are about to terminate below.
    $desktopProcs = Get-Process -Name "acowork-desktop" -ErrorAction SilentlyContinue
    if ($desktopProcs) {
        Write-Host "  Found Desktop processes: $($desktopProcs.Id -join ', ')" -ForegroundColor Gray
        Stop-Process -Name "acowork-desktop" -Force -ErrorAction SilentlyContinue
        Write-Host "  Desktop stopped." -ForegroundColor Green
    } else {
        Write-Host "  No Desktop process running." -ForegroundColor Gray
    }

    if ($gatewayProcs) {
        Write-Host "  Found Gateway processes: $($gatewayProcs.Id -join ', ')" -ForegroundColor Gray
        Stop-Process -Name "acowork-gateway" -Force -ErrorAction SilentlyContinue
        Write-Host "  Gateway stopped." -ForegroundColor Green
    } else {
        Write-Host "  No Gateway process running." -ForegroundColor Gray
    }

    if ($runtimeProcs) {
        Write-Host "  Found Runtime processes: $($runtimeProcs.Id -join ', ')" -ForegroundColor Gray
        Stop-Process -Name "acowork-runtime" -Force -ErrorAction SilentlyContinue
        Write-Host "  Runtime stopped." -ForegroundColor Green
    } else {
        Write-Host "  No Runtime process running." -ForegroundColor Gray
    }

    if ($embedProcs) {
        Write-Host "  Found Embed processes: $($embedProcs.Id -join ', ')" -ForegroundColor Gray
        Stop-Process -Name "acowork-embed" -Force -ErrorAction SilentlyContinue
        Write-Host "  Embed stopped." -ForegroundColor Green
    } else {
        Write-Host "  No Embed process running." -ForegroundColor Gray
    }

    if ($lspProcs) {
        Write-Host "  Found LSP Relay processes: $($lspProcs.Id -join ', ')" -ForegroundColor Gray
        Stop-Process -Name "acowork-lsp-relay" -Force -ErrorAction SilentlyContinue
        Write-Host "  LSP Relay stopped." -ForegroundColor Green
    } else {
        Write-Host "  No LSP Relay process running." -ForegroundColor Gray
    }

    if ($nodeProcs) {
        Write-Host "  Found Node Agent processes: $($nodeProcs.Id -join ', ')" -ForegroundColor Gray
        Stop-Process -Name "acowork-node" -Force -ErrorAction SilentlyContinue
        Write-Host "  Node Agent stopped." -ForegroundColor Green
    } else {
        Write-Host "  No Node Agent process running." -ForegroundColor Gray
    }

    # The PM service is a standalone process (ADR-064) spawned by the Gateway
    # supervisor. It self-exits via the ADR-018 watchdog when the Gateway dies,
    # but on Windows the watchdog poll can lag — kill it explicitly so the stop
    # step is idempotent and port 18082 is released before the next start.
    $pmProcs = Get-Process -Name "acowork-pm" -ErrorAction SilentlyContinue
    if ($pmProcs) {
        Write-Host "  Found PM processes: $($pmProcs.Id -join ', ')" -ForegroundColor Gray
        Stop-Process -Name "acowork-pm" -Force -ErrorAction SilentlyContinue
        Write-Host "  PM stopped." -ForegroundColor Green
    } else {
        Write-Host "  No PM process running." -ForegroundColor Gray
    }

    # The Doc service mirrors the PM pattern (ADR-064). It is a standalone
    # process (`acowork-doc`) spawned by the Gateway supervisor and listens on
    # port 18081 by default. The ADR-018 watchdog self-exit lags on Windows,
    # so kill it explicitly to keep the stop step idempotent and to release
    # 18081 before the next start.
    $docProcs = Get-Process -Name "acowork-doc" -ErrorAction SilentlyContinue
    if ($docProcs) {
        Write-Host "  Found Doc processes: $($docProcs.Id -join ', ')" -ForegroundColor Gray
        Stop-Process -Name "acowork-doc" -Force -ErrorAction SilentlyContinue
        Write-Host "  Doc stopped." -ForegroundColor Green
    } else {
        Write-Host "  No Doc process running." -ForegroundColor Gray
    }

    # Ensure embed port 18080 is released before starting a new gateway.
    # Stop-Process may not have released the port yet; the new gateway
    # spawns its own embed immediately and if the old one is still
    # binding, the new embed panics with AddrInUse.
    $portLine = netstat -ano 2>$null | Select-String ":18080\s" | Select-Object -First 1
    if ($portLine) {
        $pidFromPort = ($portLine.Line -split '\s+')[-1]
        if ($pidFromPort -match '^\d+$') {
            Write-Host "  Port 18080 held by PID $pidFromPort — force-killing" -ForegroundColor Gray
            Stop-Process -Id $pidFromPort -Force -ErrorAction SilentlyContinue
        }
    }
    # Wait up to 3s for the port to actually be released.
    $portWaited = 0
    while ($portWaited -lt 6) {
        $stillUp = netstat -ano 2>$null | Select-String ":18080\s"
        if (-not $stillUp) { break }
        Start-Sleep -Milliseconds 500
        $portWaited++
    }
    if ($portWaited -ge 6) {
        Write-Host "  WARNING: Port 18080 still in use after 3s" -ForegroundColor Red
    }

    # Ensure LSP Relay port 19878 is released (see process_group note above).
    # Independent counter so embed-port wait doesn't pre-empt the relay-port wait.
    $lspPortLine = netstat -ano 2>$null | Select-String ":19878\s" | Select-Object -First 1
    if ($lspPortLine) {
        $pidFromPort = ($lspPortLine.Line -split '\s+')[-1]
        if ($pidFromPort -match '^\d+$') {
            Write-Host "  Port 19878 held by PID $pidFromPort — force-killing" -ForegroundColor Gray
            Stop-Process -Id $pidFromPort -Force -ErrorAction SilentlyContinue
        }
    }
    $lspPortWaited = 0
    while ($lspPortWaited -lt 6) {
        $stillUp = netstat -ano 2>$null | Select-String ":19878\s"
        if (-not $stillUp) { break }
        Start-Sleep -Milliseconds 500
        $lspPortWaited++
    }
    if ($lspPortWaited -ge 6) {
        Write-Host "  WARNING: Port 19878 still in use after 3s" -ForegroundColor Red
    }

    # Ensure PM port 18082 is released (ADR-064 standalone process). The PM
    # supervisor auto-increments on conflict, but a stale process from a killed
    # Gateway would otherwise hold the default port and shift PM to 18083+.
    $pmPortLine = netstat -ano 2>$null | Select-String ":18082\s" | Select-Object -First 1
    if ($pmPortLine) {
        $pidFromPort = ($pmPortLine.Line -split '\s+')[-1]
        if ($pidFromPort -match '^\d+$') {
            Write-Host "  Port 18082 held by PID $pidFromPort — force-killing" -ForegroundColor Gray
            Stop-Process -Id $pidFromPort -Force -ErrorAction SilentlyContinue
        }
    }
    $pmPortWaited = 0
    while ($pmPortWaited -lt 6) {
        $stillUp = netstat -ano 2>$null | Select-String ":18082\s"
        if (-not $stillUp) { break }
        Start-Sleep -Milliseconds 500
        $pmPortWaited++
    }
    if ($pmPortWaited -ge 6) {
        Write-Host "  WARNING: Port 18082 still in use after 3s" -ForegroundColor Red
    }

    # Ensure Doc port 18081 is released (ADR-064 standalone process). Same
    # rationale as the PM port block above: a stale doc from a killed Gateway
    # would hold the default port and shift the new doc to 18082+.
    $docPortLine = netstat -ano 2>$null | Select-String ":18081\s" | Select-Object -First 1
    if ($docPortLine) {
        $pidFromPort = ($docPortLine.Line -split '\s+')[-1]
        if ($pidFromPort -match '^\d+$') {
            Write-Host "  Port 18081 held by PID $pidFromPort — force-killing" -ForegroundColor Gray
            Stop-Process -Id $pidFromPort -Force -ErrorAction SilentlyContinue
        }
    }
    $docPortWaited = 0
    while ($docPortWaited -lt 6) {
        $stillUp = netstat -ano 2>$null | Select-String ":18081\s"
        if (-not $stillUp) { break }
        Start-Sleep -Milliseconds 500
        $docPortWaited++
    }
    if ($docPortWaited -ge 6) {
        Write-Host "  WARNING: Port 18081 still in use after 3s" -ForegroundColor Red
    }

    Write-Host ""
}

# Step: Build Gateway
$step++
Write-Host "[$step/$totalSteps] Building Gateway ($Profile mode)..." -ForegroundColor Yellow
Set-Location $CoreDir
try {
    $cargoArgs = @("build")
    if ($Profile -eq "release") { $cargoArgs += "--release" }
    $cargoArgs += @("-p", "acowork-gateway")
    & cmd /c "cargo $($cargoArgs -join ' ')" 2>&1 | ForEach-Object {
        if ($_ -match "error" -or $_ -match "Compiling") {
            Write-Host "  $_" -ForegroundColor Gray
        }
    }
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed with exit code $LASTEXITCODE"
    }
    Write-Host "  Gateway build completed." -ForegroundColor Green
} catch {
    Write-Host "  Gateway build failed: $_" -ForegroundColor Red
    exit 1
}

Write-Host ""

# Step: Build Runtime
$step++
Write-Host "[$step/$totalSteps] Building Runtime ($Profile mode)..." -ForegroundColor Yellow
try {
    $cargoArgs = @("build")
    if ($Profile -eq "release") { $cargoArgs += "--release" }
    $cargoArgs += @("-p", "acowork-runtime")
    & cmd /c "cargo $($cargoArgs -join ' ')" 2>&1 | ForEach-Object {
        if ($_ -match "error" -or $_ -match "Compiling") {
            Write-Host "  $_" -ForegroundColor Gray
        }
    }
    Write-Host "  Runtime build completed." -ForegroundColor Green
} catch {
    Write-Host "  Runtime build failed: $_" -ForegroundColor Red
    exit 1
}

Write-Host ""

# Step: Build Embedding Runtime (ORT auto-detected from .ort/ directory)
$step++
Write-Host "[$step/$totalSteps] Building Embedding Runtime ($Profile mode)..." -ForegroundColor Yellow

$ortDir = Join-Path $WorkspaceRoot ".ort"
$ortEntries = @()
if (Test-Path $ortDir) {
    $ortEntries = Get-ChildItem -Path $ortDir -Directory -ErrorAction SilentlyContinue | Where-Object { $_.Name -like "onnxruntime-win-x64-*" } | Sort-Object Name -Descending
}
$preferredOrt = $ortEntries | Where-Object { $_.Name -eq "onnxruntime-win-x64-1.22.0" } | Select-Object -First 1
if (-not $preferredOrt) {
    $preferredOrt = $ortEntries | Select-Object -First 1
}
if ($preferredOrt) {
    $libDir = Join-Path $preferredOrt.FullName "lib"
    $dllPath = Join-Path $libDir "onnxruntime.dll"
    if (Test-Path $dllPath) {
        $env:ORT_LIB_LOCATION = $libDir
        $env:ORT_DYLIB_PATH = $dllPath
        Write-Host "  Using local ORT: $libDir" -ForegroundColor Green
    }
}
if (-not $env:ORT_LIB_LOCATION) {
    Write-Host "  ONNX Runtime not found. Run .\dev\setup_ort.ps1 first." -ForegroundColor Red
    if ($Profile -eq "release") {
        Write-Host "  Alternative: cargo build --release -p acowork-embed --features download-ort" -ForegroundColor Red
    } else {
        Write-Host "  Alternative: cargo build -p acowork-embed --features download-ort" -ForegroundColor Red
    }
    exit 1
}

try {
    $cargoArgs = @("build")
    if ($Profile -eq "release") { $cargoArgs += "--release" }
    $cargoArgs += @("-p", "acowork-embed")
    & cmd /c "cargo $($cargoArgs -join ' ')" 2>&1 | ForEach-Object {
        if ($_ -match "error" -or $_ -match "Compiling") {
            Write-Host "  $_" -ForegroundColor Gray
        }
    }
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed with exit code $LASTEXITCODE"
    }
    Write-Host "  Embedding Runtime build completed." -ForegroundColor Green
} catch {
    Write-Host "  Embedding Runtime build failed: $_" -ForegroundColor Red
    exit 1
}

# Step: Build LSP Relay (standalone binary, sibling of acowork-gateway.exe)
#
# See ADR-019 / core/acowork-gateway/src/lifecycle/lsp_relay.rs::spawn_lsp_relay.
# The Gateway locates the relay as `current_exe().parent().join("acowork-lsp-relay.exe")`
# (or without .exe on Unix), so the binary MUST sit next to acowork-gateway.exe —
# otherwise startup fails with:
#   GatewayError::Lifecycle("acowork-lsp-relay binary not found at ...")
$step++
Write-Host "[$step/$totalSteps] Building LSP Relay ($Profile mode)..." -ForegroundColor Yellow
try {
    $cargoArgs = @("build")
    if ($Profile -eq "release") { $cargoArgs += "--release" }
    $cargoArgs += @("-p", "acowork-lsp-relay")
    & cmd /c "cargo $($cargoArgs -join ' ')" 2>&1 | ForEach-Object {
        if ($_ -match "error" -or $_ -match "Compiling") {
            Write-Host "  $_" -ForegroundColor Gray
        }
    }
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed with exit code $LASTEXITCODE"
    }
    Write-Host "  LSP Relay build completed." -ForegroundColor Green
} catch {
    Write-Host "  LSP Relay build failed: $_" -ForegroundColor Red
    exit 1
}

Write-Host ""

# Step: Build Node Agent (standalone binary, sibling of acowork-gateway.exe)
#
# ADR-055 §6.11: the Gateway supervises a local Node Agent (`acowork-node`),
# located via `current_exe().parent().join("acowork-node.exe")` — so the
# binary MUST sit next to acowork-gateway.exe. Without it the Gateway
# silently disables the node topology ("acowork-node binary not found — local
# node agent disabled"), node 'local' never enrolls, and agent installs fail
# with 503 "Node 'local' has never enrolled (offline)".
$step++
Write-Host "[$step/$totalSteps] Building Node Agent ($Profile mode)..." -ForegroundColor Yellow
try {
    $cargoArgs = @("build")
    if ($Profile -eq "release") { $cargoArgs += "--release" }
    $cargoArgs += @("-p", "acowork-node")
    & cmd /c "cargo $($cargoArgs -join ' ')" 2>&1 | ForEach-Object {
        if ($_ -match "error" -or $_ -match "Compiling") {
            Write-Host "  $_" -ForegroundColor Gray
        }
    }
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed with exit code $LASTEXITCODE"
    }
    Write-Host "  Node Agent build completed." -ForegroundColor Green
} catch {
    Write-Host "  Node Agent build failed: $_" -ForegroundColor Red
    exit 1
}

Write-Host ""

# Step: Build PM service (standalone binary, sibling of acowork-gateway.exe)
#
# ADR-064: the PM service is a standalone process (`acowork-pm`), located via
# `current_exe().parent().join("acowork-pm.exe")` — so the binary MUST sit next
# to acowork-gateway.exe. Without it the Gateway supervisor logs
# "acowork-pm binary not found" and `/api/pm/*` returns 503.
$step++
Write-Host "[$step/$totalSteps] Building PM service ($Profile mode)..." -ForegroundColor Yellow
try {
    $cargoArgs = @("build")
    if ($Profile -eq "release") { $cargoArgs += "--release" }
    $cargoArgs += @("-p", "acowork-pm")
    & cmd /c "cargo $($cargoArgs -join ' ')" 2>&1 | ForEach-Object {
        if ($_ -match "error" -or $_ -match "Compiling") {
            Write-Host "  $_" -ForegroundColor Gray
        }
    }
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed with exit code $LASTEXITCODE"
    }
    Write-Host "  PM service build completed." -ForegroundColor Green
} catch {
    Write-Host "  PM service build failed: $_" -ForegroundColor Red
    exit 1
}

Write-Host ""

# Step: Build Doc service (standalone binary, sibling of acowork-gateway.exe)
#
# Mirrors the PM service above: the Doc service is a standalone process
# (`acowork-doc`), located via `current_exe().parent().join("acowork-doc.exe")`
# — so the binary MUST sit next to acowork-gateway.exe. Without it the Gateway
# supervisor logs "acowork-doc binary not found" and `/api/doc/*` returns 503
# (document library unavailable).
$step++
Write-Host "[$step/$totalSteps] Building Doc service ($Profile mode)..." -ForegroundColor Yellow
try {
    $cargoArgs = @("build")
    if ($Profile -eq "release") { $cargoArgs += "--release" }
    $cargoArgs += @("-p", "acowork-doc")
    & cmd /c "cargo $($cargoArgs -join ' ')" 2>&1 | ForEach-Object {
        if ($_ -match "error" -or $_ -match "Compiling") {
            Write-Host "  $_" -ForegroundColor Gray
        }
    }
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed with exit code $LASTEXITCODE"
    }
    Write-Host "  Doc service build completed." -ForegroundColor Green
} catch {
    Write-Host "  Doc service build failed: $_" -ForegroundColor Red
    exit 1
}

Write-Host ""

# Step: Copy offline_providers.json + embedding_models.json from assets to target dir
#
# The gateway (and embed) read embedding_models.json from `{exe_dir}/`. Whoever
# distributes the binary (this script for dev, the package installer for
# release, the Tauri bundler for desktop) is responsible for placing it there.
#
# We only stage into the directory matching the active profile — the previous
# "stage to both target\release and target\debug" pattern was the source of
# the silent stray-file bug when target\debug did not exist.
$step++
Write-Host "[$step/$totalSteps] Copying runtime resource files to target\\$Profile..." -ForegroundColor Yellow
$offlineSrc = Join-Path $WorkspaceRoot "assets\offline_providers.json"
$embedModelsSrc = Join-Path $WorkspaceRoot "core\acowork-embed\assets\embedding_models.json"

# Ensure the single profile target directory exists before any Copy-Item call.
# Copy-Item does not auto-create missing parent directories — if target\$Profile
# did not exist (typical after `-Debug` on a release-only checkout), it would
# silently create a file literally named "$Profile" inside target\ instead.
if (-not (Test-Path $targetDir)) {
    New-Item -ItemType Directory -Path $targetDir -Force | Out-Null
}

if (Test-Path $offlineSrc) {
    Copy-Item -Path $offlineSrc -Destination $targetDir -Force
    Write-Host "  offline_providers.json -> $targetDir" -ForegroundColor Green
} else {
    Write-Host "  WARNING: offline_providers.json not found at $offlineSrc" -ForegroundColor Red
}

if (Test-Path $embedModelsSrc) {
    Copy-Item -Path $embedModelsSrc -Destination (Join-Path $targetDir "embedding_models.json") -Force
    Write-Host "  embedding_models.json -> $targetDir" -ForegroundColor Green
} else {
    Write-Host "  WARNING: embedding_models.json not found at $embedModelsSrc" -ForegroundColor Red
}

$embedProvidersSrc = Join-Path $WorkspaceRoot "assets\offline_embedding_providers.json"
if (Test-Path $embedProvidersSrc) {
    Copy-Item -Path $embedProvidersSrc -Destination $targetDir -Force
    Write-Host "  offline_embedding_providers.json -> $targetDir" -ForegroundColor Green
} else {
    Write-Host "  WARNING: offline_embedding_providers.json not found at $embedProvidersSrc" -ForegroundColor Red
}

if ($env:ORT_DYLIB_PATH -and (Test-Path $env:ORT_DYLIB_PATH)) {
    Copy-Item -Path $env:ORT_DYLIB_PATH -Destination (Join-Path $targetDir "onnxruntime.dll") -Force -ErrorAction SilentlyContinue
    Write-Host "  onnxruntime.dll -> $targetDir" -ForegroundColor Green
}

Write-Host ""

if ($Start) {
    # Step: Start Gateway
    $step++
    $logLevel = if ($env:ACOWORK_GATEWAY_LOG_LEVEL) { $env:ACOWORK_GATEWAY_LOG_LEVEL } else { "info" }
    Write-Host "[$step/$totalSteps] Starting Gateway in daemon mode (log level: $logLevel)..." -ForegroundColor Yellow
    $env:ACOWORK_GATEWAY_DAEMON = "true"

    # Start Gateway in background
    $gatewayExe = Join-Path $WorkspaceRoot "target\$Profile\acowork-gateway.exe"
    if (Test-Path $gatewayExe) {
        if ($GatewayArgs.Count -gt 0) {
            Write-Host "  Gateway args: $($GatewayArgs -join ' ')" -ForegroundColor Gray
        }
        Start-Process -FilePath $gatewayExe -ArgumentList $GatewayArgs -WorkingDirectory $WorkspaceRoot -NoNewWindow
        Write-Host "  Gateway started." -ForegroundColor Green
    } else {
        Write-Host "  Gateway executable not found at: $gatewayExe" -ForegroundColor Red
        exit 1
    }

    Write-Host ""
    Write-Host "========================================" -ForegroundColor Cyan
    Write-Host "Done! Gateway is running." -ForegroundColor Cyan
    if ($NetworkMode -eq "remote" -and $lanIp) {
        Write-Host "HTTP API: http://${lanIp}:19876  (also reachable on the LAN)" -ForegroundColor Cyan
    } else {
        Write-Host "HTTP API: http://127.0.0.1:19876" -ForegroundColor Cyan
    }
    Write-Host "========================================" -ForegroundColor Cyan
} else {
    Write-Host ""
    Write-Host "========================================" -ForegroundColor Cyan
    Write-Host "Build complete (not started)." -ForegroundColor Cyan
    if ($Profile -eq "debug") {
        Write-Host "To start: .\dev\build_core.ps1 -Debug -Start" -ForegroundColor Cyan
    } else {
        Write-Host "To start: .\dev\build_core.ps1 -Start" -ForegroundColor Cyan
    }
    Write-Host "========================================" -ForegroundColor Cyan
}

# Return to workspace root
Set-Location $WorkspaceRoot
