#!/usr/bin/env pwsh
# build_core.ps1 - Build Gateway + Runtime (debug or release mode)
# Usage:
#   .\dev\build_core.ps1                  Build release (default)
#   .\dev\build_core.ps1 -Debug           Build debug
#   .\dev\build_core.ps1 -Release         Build release (explicit)
#   .\dev\build_core.ps1 -Start           Build release + stop old + start Gateway
#   .\dev\build_core.ps1 -Debug -Start    Build debug + stop old + start Gateway
#
# Profile selection: -Debug / -Release switch > $env:ACOWORK_BUILD_PROFILE > release
# In debug profile, $env:ACOWORK_GATEWAY_LOG_LEVEL is auto-set to "debug" so any
# gateway process spawned from this script's process tree (including -Start
# and a manual `target\debug\acowork-gateway.exe` invocation from the same
# terminal) inherits verbose logging.

param(
    [switch] $Start,
    [switch] $Debug,
    [switch] $Release
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

# Runtime env linkage: debug profile auto-enables gateway verbose logging for
# any child process spawned from this script.
if ($Profile -eq "debug") {
    $env:ACOWORK_GATEWAY_LOG_LEVEL = "debug"
}

$targetDir = Join-Path $WorkspaceRoot "target\$Profile"
# Step count: Stop, Gateway, Runtime, Embed, LSP Relay, Copy resources, Start.
# Was previously {5/3}, which undercounted by 1 in both modes — fixing here so
# the new LSP Relay step doesn't compound the inconsistency.
$totalSteps = if ($Start) { 7 } else { 5 }

Write-Host "========================================" -ForegroundColor Cyan
Write-Host "ACowork Core Build Script" -ForegroundColor Cyan
Write-Host "Profile: $Profile" -ForegroundColor Cyan
if ($Start) { Write-Host "Mode: Build + Restart" -ForegroundColor Cyan }
else       { Write-Host "Mode: Build Only" -ForegroundColor Cyan }
Write-Host "========================================" -ForegroundColor Cyan
Write-Host ""

$step = 0

if ($Start) {
    # Step: Stop running processes
    $step++
    Write-Host "[$step/$totalSteps] Stopping running Gateway, Runtime, Embed, and LSP Relay processes..." -ForegroundColor Yellow

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
    & cargo @cargoArgs 2>&1 | ForEach-Object {
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
    & cargo @cargoArgs 2>&1 | ForEach-Object {
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
    & cargo @cargoArgs 2>&1 | ForEach-Object {
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
    & cargo @cargoArgs 2>&1 | ForEach-Object {
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
        Start-Process -FilePath $gatewayExe -WorkingDirectory $WorkspaceRoot -NoNewWindow
        Write-Host "  Gateway started." -ForegroundColor Green
    } else {
        Write-Host "  Gateway executable not found at: $gatewayExe" -ForegroundColor Red
        exit 1
    }

    Write-Host ""
    Write-Host "========================================" -ForegroundColor Cyan
    Write-Host "Done! Gateway is running." -ForegroundColor Cyan
    Write-Host "HTTP API: http://127.0.0.1:19876" -ForegroundColor Cyan
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
