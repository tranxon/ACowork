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

# Step: Build Embedding Runtime (ORT auto-detected from .ort/ directory)
$step++
Write-Host "[$step/$totalSteps] Building Embedding Runtime (release mode)..." -ForegroundColor Yellow

# Auto-detect local ONNX Runtime install under .ort/
if (-not $env:ORT_LIB_LOCATION) {
    $ortDir = Join-Path $WorkspaceRoot ".ort"
    if (Test-Path $ortDir) {
        $entries = Get-ChildItem -Path $ortDir -Directory -ErrorAction SilentlyContinue | Where-Object { $_.Name -like "onnxruntime-*" }
        foreach ($entry in $entries) {
            $libDir = Join-Path $entry.FullName "lib"
            $dllPath = Join-Path $libDir "onnxruntime.dll"
            if (Test-Path $dllPath) {
                $env:ORT_LIB_LOCATION = $libDir
                $env:ORT_DYLIB_PATH = $dllPath
                Write-Host "  Detected local ORT: $libDir" -ForegroundColor Green
                break
            }
        }
    }
    if (-not $env:ORT_LIB_LOCATION) {
        Write-Host "  ONNX Runtime not found. Run .\dev\setup_ort.ps1 first." -ForegroundColor Red
        Write-Host "  Alternative: cargo build --release -p rollball-embed --features download-ort" -ForegroundColor Red
        exit 1
    }
}

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
