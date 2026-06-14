#!/usr/bin/env pwsh
# build_core.ps1 - Build Gateway + Runtime (release mode)
# Usage:
#   .\dev\build_core.ps1          Build only (default)
#   .\dev\build_core.ps1 -Start   Build + stop old + start Gateway

param([switch] $Start)

$ErrorActionPreference = "Stop"
$WorkspaceRoot = Split-Path -Parent $PSScriptRoot
$CoreDir = Join-Path $WorkspaceRoot "core"

$totalSteps = if ($Start) { 5 } else { 3 }

Write-Host "========================================" -ForegroundColor Cyan
Write-Host "Rollball Core Build Script" -ForegroundColor Cyan
if ($Start) { Write-Host "Mode: Build + Restart" -ForegroundColor Cyan }
else       { Write-Host "Mode: Build Only" -ForegroundColor Cyan }
Write-Host "========================================" -ForegroundColor Cyan
Write-Host ""

$step = 0

if ($Start) {
    # Step: Stop running processes
    $step++
    Write-Host "[$step/$totalSteps] Stopping running Gateway, Runtime, and Embed processes..." -ForegroundColor Yellow

    $gatewayProcs = Get-Process -Name "rollball-gateway" -ErrorAction SilentlyContinue
    $runtimeProcs = Get-Process -Name "rollball-runtime" -ErrorAction SilentlyContinue
    $embedProcs   = Get-Process -Name "rollball-embed"   -ErrorAction SilentlyContinue

    if ($gatewayProcs) {
        Write-Host "  Found Gateway processes: $($gatewayProcs.Id -join ', ')" -ForegroundColor Gray
        Stop-Process -Name "rollball-gateway" -Force -ErrorAction SilentlyContinue
        Write-Host "  Gateway stopped." -ForegroundColor Green
    } else {
        Write-Host "  No Gateway process running." -ForegroundColor Gray
    }

    if ($runtimeProcs) {
        Write-Host "  Found Runtime processes: $($runtimeProcs.Id -join ', ')" -ForegroundColor Gray
        Stop-Process -Name "rollball-runtime" -Force -ErrorAction SilentlyContinue
        Write-Host "  Runtime stopped." -ForegroundColor Green
    } else {
        Write-Host "  No Runtime process running." -ForegroundColor Gray
    }

    if ($embedProcs) {
        Write-Host "  Found Embed processes: $($embedProcs.Id -join ', ')" -ForegroundColor Gray
        Stop-Process -Name "rollball-embed" -Force -ErrorAction SilentlyContinue
        Write-Host "  Embed stopped." -ForegroundColor Green
    } else {
        Write-Host "  No Embed process running." -ForegroundColor Gray
    }

    Write-Host ""
}

# Step: Build Gateway
$step++
Write-Host "[$step/$totalSteps] Building Gateway (release mode)..." -ForegroundColor Yellow
Set-Location $CoreDir
try {
    cargo build --release -p rollball-gateway 2>&1 | ForEach-Object {
        if ($_ -match "error" -or $_ -match "Compiling") {
            Write-Host "  $_" -ForegroundColor Gray
        }
    }
    Write-Host "  Gateway build completed." -ForegroundColor Green
} catch {
    Write-Host "  Gateway build failed: $_" -ForegroundColor Red
    exit 1
}

Write-Host ""

# Step: Build Runtime
$step++
Write-Host "[$step/$totalSteps] Building Runtime (release mode)..." -ForegroundColor Yellow
try {
    cargo build --release -p rollball-runtime 2>&1 | ForEach-Object {
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

# Step: Build Embedding Runtime (ORT auto-detected by build.rs)
$step++
Write-Host "[$step/$totalSteps] Building Embedding Runtime (release mode)..." -ForegroundColor Yellow
try {
    cargo build --release -p rollball-embed 2>&1 | ForEach-Object {
        if ($_ -match "error" -or $_ -match "Compiling") {
            Write-Host "  $_" -ForegroundColor Gray
        }
    }
    Write-Host "  Embedding Runtime build completed." -ForegroundColor Green
} catch {
    Write-Host "  Embedding Runtime build failed: $_" -ForegroundColor Red
    exit 1
}

Write-Host ""

# Step: Copy offline_providers.json + embedding_models.json from assets to target dirs
#
# The gateway (and embed) read embedding_models.json from `{exe_dir}/`. Whoever
# distributes the binary (this script for dev, the package installer for
# release, the Tauri bundler for desktop) is responsible for placing it there.
$step++
Write-Host "[$step/$totalSteps] Copying runtime resource files to target directories..." -ForegroundColor Yellow
$offlineSrc = Join-Path $WorkspaceRoot "assets\offline_providers.json"
$embedModelsSrc = Join-Path $WorkspaceRoot "core\rollball-embed\assets\embedding_models.json"
$releaseDir = Join-Path $WorkspaceRoot "target\release"
$debugDir = Join-Path $WorkspaceRoot "target\debug"

if (Test-Path $offlineSrc) {
    Copy-Item -Path $offlineSrc -Destination $releaseDir -Force
    Write-Host "  offline_providers.json -> $releaseDir" -ForegroundColor Green
    Copy-Item -Path $offlineSrc -Destination $debugDir -Force
    Write-Host "  offline_providers.json -> $debugDir" -ForegroundColor Green
} else {
    Write-Host "  WARNING: offline_providers.json not found at $offlineSrc" -ForegroundColor Red
}

if (Test-Path $embedModelsSrc) {
    Copy-Item -Path $embedModelsSrc -Destination (Join-Path $releaseDir "embedding_models.json") -Force
    Write-Host "  embedding_models.json -> $releaseDir" -ForegroundColor Green
    Copy-Item -Path $embedModelsSrc -Destination (Join-Path $debugDir "embedding_models.json") -Force
    Write-Host "  embedding_models.json -> $debugDir" -ForegroundColor Green
} else {
    Write-Host "  WARNING: embedding_models.json not found at $embedModelsSrc" -ForegroundColor Red
}

Write-Host ""

if ($Start) {
    # Step: Start Gateway
    $step++
    Write-Host "[$step/$totalSteps] Starting Gateway in daemon mode (debug logging)..." -ForegroundColor Yellow
    $env:ROLLBALL_GATEWAY_DAEMON = "true"
    $env:ROLLBALL_GATEWAY_LOG_LEVEL = "debug"

    # Start Gateway in background
    $gatewayExe = Join-Path $WorkspaceRoot "target\release\rollball-gateway.exe"
    if (Test-Path $gatewayExe) {
        Start-Process -FilePath $gatewayExe -NoNewWindow
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
    Write-Host "To start: .\dev\build_core.ps1 -Start" -ForegroundColor Cyan
    Write-Host "========================================" -ForegroundColor Cyan
}

# Return to workspace root
Set-Location $WorkspaceRoot
