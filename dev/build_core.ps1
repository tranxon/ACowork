#!/usr/bin/env pwsh
# build_core.ps1 - One-click rebuild and restart Gateway + Runtime
# Usage: .\dev\build_core.ps1

$ErrorActionPreference = "Stop"
$WorkspaceRoot = Split-Path -Parent $PSScriptRoot
$CoreDir = Join-Path $WorkspaceRoot "core"

Write-Host "========================================" -ForegroundColor Cyan
Write-Host "Rollball Core Rebuild & Restart Script" -ForegroundColor Cyan
Write-Host "========================================" -ForegroundColor Cyan
Write-Host ""

# Step 1: Stop running processes
Write-Host "[1/4] Stopping running Gateway and Runtime processes..." -ForegroundColor Yellow

$gatewayProcs = Get-Process -Name "rollball-gateway" -ErrorAction SilentlyContinue
$runtimeProcs = Get-Process -Name "rollball-runtime" -ErrorAction SilentlyContinue

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

Write-Host ""

# Step 2: Build Gateway
Write-Host "[2/4] Building Gateway (release mode)..." -ForegroundColor Yellow
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

# Step 3: Build Runtime
Write-Host "[3/4] Building Runtime (release mode)..." -ForegroundColor Yellow
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

# Step 4: Copy offline_providers.json to release dir
Write-Host "[4/5] Copying offline_providers.json to release directory..." -ForegroundColor Yellow
$offlineSrc = Join-Path $CoreDir "rollball-gateway\src\http\offline_providers.json"
$releaseDir = Join-Path $WorkspaceRoot "target\release"
if (Test-Path $offlineSrc) {
    Copy-Item -Path $offlineSrc -Destination $releaseDir -Force
    Write-Host "  Copied to $releaseDir" -ForegroundColor Green
} else {
    Write-Host "  WARNING: offline_providers.json not found at $offlineSrc" -ForegroundColor Red
}

Write-Host ""

# Step 5: Start Gateway
Write-Host "[5/5] Starting Gateway in daemon mode (debug logging)..." -ForegroundColor Yellow
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

# Return to workspace root
Set-Location $WorkspaceRoot
