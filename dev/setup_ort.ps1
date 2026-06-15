#!/usr/bin/env pwsh
# setup_ort.ps1 - Download and configure ONNX Runtime for Windows
#
# Usage:
#   .\dev\setup_ort.ps1                       # Auto-detect and install
#   .\dev\setup_ort.ps1 -Version "1.22.0"     # Specific version
#   .\dev\setup_ort.ps1 -Reinstall            # Force re-download
#   .\dev\setup_ort.ps1 -NoMirror             # Skip China mirrors, use GitHub directly
#
# After running, env vars are set for the current session. To build:
#   .\dev\build_core.ps1              # Auto-detects .ort/ and builds all crates
#   cargo build -p acowork-embed      # Direct build (env set in this session)

param(
    [string] $Version = "",
    [switch] $Reinstall,
    [switch] $NoMirror
)

$ErrorActionPreference = "Stop"
$WorkspaceRoot = Split-Path -Parent $PSScriptRoot

Write-Host "============================================" -ForegroundColor Cyan
Write-Host "  ONNX Runtime Setup for AgentCowork.AI (Win)" -ForegroundColor Cyan
Write-Host "============================================" -ForegroundColor Cyan
Write-Host ""

# ── Defaults ────────────────────────────────────────────────────────────────
$OrtVersion = if ([string]::IsNullOrEmpty($Version)) { "1.22.0" } else { $Version }
$OrtPlatform = "win"
$OrtArch = "x64"
$OrtLibName = "onnxruntime.dll"
$OrtLibStatic = "onnxruntime.lib"

# ── Paths ───────────────────────────────────────────────────────────────────
$OrtInstallDir = Join-Path $WorkspaceRoot ".ort"
$OrtArchiveName = "onnxruntime-${OrtPlatform}-${OrtArch}-${OrtVersion}"
$OrtExtractedDir = Join-Path $OrtInstallDir $OrtArchiveName
$OrtGitHubUrl = "https://github.com/microsoft/onnxruntime/releases/download/v${OrtVersion}/${OrtArchiveName}.zip"

# ── GitHub mirror proxies (China mainland acceleration) ─────────────────────
# Tries multiple mirrors in order; first successful download wins.
# Use -NoMirror to skip mirrors and use GitHub directly.
$MirrorPrefixes = @(
    "https://ghfast.top/"
    "https://gh-proxy.com/"
    "https://mirror.ghproxy.com/"
    "https://ghproxy.net/"
)

$OrtUrls = @()
if (-not $NoMirror) {
    foreach ($prefix in $MirrorPrefixes) {
        $OrtUrls += "${prefix}${OrtGitHubUrl}"
    }
}
$OrtUrls += $OrtGitHubUrl

Write-Host "  ORT Version : $OrtVersion" -ForegroundColor Cyan
Write-Host "  Platform    : $OrtPlatform ($OrtArch)" -ForegroundColor Cyan
Write-Host "  Install Dir : $OrtExtractedDir" -ForegroundColor Cyan
Write-Host "  Sources     : $($OrtUrls.Count) (mirrors + GitHub)" -ForegroundColor Cyan
Write-Host ""

$existingOrtDirs = @()
if (Test-Path $OrtInstallDir) {
    $existingOrtDirs = Get-ChildItem -Path $OrtInstallDir -Directory -ErrorAction SilentlyContinue | Where-Object { $_.Name -like "onnxruntime-win-x64-*" }
}
foreach ($dir in $existingOrtDirs) {
    if ($dir.Name -ne $OrtArchiveName) {
        Write-Host "  Found older/different ORT: $($dir.Name)" -ForegroundColor Yellow
    }
}

# ── Check if already installed ──────────────────────────────────────────────
$OrtLibPath = Join-Path $OrtExtractedDir "lib\$OrtLibStatic"
if ((Test-Path $OrtLibPath) -and -not $Reinstall) {
    Write-Host "  ORT $OrtVersion already installed." -ForegroundColor Green
    Write-Host "  Use -Reinstall to force re-download." -ForegroundColor Gray
} else {
    # ── Download (with mirror fallback) ─────────────────────────────────────
    Write-Host "[1/4] Downloading ONNX Runtime $OrtVersion..." -ForegroundColor Yellow
    Write-Host "  Trying $($OrtUrls.Count) sources..." -ForegroundColor Gray

    $tempZip = Join-Path $env:TEMP "${OrtArchiveName}.zip"
    $downloadOk = $false

    foreach ($url in $OrtUrls) {
        Write-Host "  Trying: $($url.Substring(0, [Math]::Min(60, $url.Length)))..." -ForegroundColor Gray
        try {
            $progressPreference = 'SilentlyContinue'
            Invoke-WebRequest -Uri $url -OutFile $tempZip -UseBasicParsing -TimeoutSec 30
            $progressPreference = 'Continue'
            if (Test-Path $tempZip) {
                $fileSize = (Get-Item $tempZip).Length
                if ($fileSize -gt 1MB) {
                    Write-Host "  Download OK ($([Math]::Round($fileSize / 1MB)) MB)" -ForegroundColor Green
                    $downloadOk = $true
                    break
                } else {
                    Write-Host "  File too small ($fileSize bytes), trying next..." -ForegroundColor Yellow
                    Remove-Item -Path $tempZip -Force -ErrorAction SilentlyContinue
                }
            }
        } catch {
            Write-Host "  Failed: $($_.Exception.Message)" -ForegroundColor Red
            Remove-Item -Path $tempZip -Force -ErrorAction SilentlyContinue
        }
    }

    if (-not $downloadOk) {
        Write-Host "  All download sources failed." -ForegroundColor Red
        Write-Host "  Direct URL: $OrtGitHubUrl" -ForegroundColor Red
        exit 1
    }
    Write-Host ""

    # ── Extract ──────────────────────────────────────────────────────────────
    Write-Host "[2/4] Extracting..." -ForegroundColor Yellow
    if (-not (Test-Path $OrtInstallDir)) {
        New-Item -ItemType Directory -Path $OrtInstallDir -Force | Out-Null
    }
    if ($Reinstall -and (Test-Path $OrtExtractedDir)) {
        Remove-Item -Recurse -Force $OrtExtractedDir
    }

    Expand-Archive -Path $tempZip -DestinationPath $OrtInstallDir -Force
    Remove-Item -Path $tempZip -Force -ErrorAction SilentlyContinue

    if (-not (Test-Path $OrtExtractedDir)) {
        Write-Host "  Extraction failed. Expected: $OrtExtractedDir" -ForegroundColor Red
        exit 1
    }
    Write-Host "  Extracted to: $OrtExtractedDir" -ForegroundColor Green
    Write-Host ""

    # ── Verify ───────────────────────────────────────────────────────────────
    Write-Host "[3/4] Verifying installation..." -ForegroundColor Yellow
    $libDir = Join-Path $OrtExtractedDir "lib"
    if (Test-Path (Join-Path $libDir $OrtLibName)) {
        Write-Host "  DLL: $OrtLibName" -ForegroundColor Green
    }
    if (Test-Path (Join-Path $libDir $OrtLibStatic)) {
        Write-Host "  Lib: $OrtLibStatic" -ForegroundColor Green
    }
    Write-Host ""

    # ── Clean cache ──────────────────────────────────────────────────────────
    Write-Host "[4/4] Cleaning cached ORT downloads..." -ForegroundColor Yellow
    $ortCache = Join-Path $env:USERPROFILE ".cache\ort.pyke.io"
    if (Test-Path $ortCache) {
        Remove-Item -Recurse -Force $ortCache
        Write-Host "  Removed $ortCache" -ForegroundColor Green
    } else {
        Write-Host "  No cache to clean." -ForegroundColor Gray
    }
    Write-Host ""
}

# ── Set environment variables ───────────────────────────────────────────────
$OrtLibLocation = Join-Path $OrtExtractedDir "lib"
$OrtDylibPath = Join-Path $OrtLibLocation $OrtLibName
$env:ORT_LIB_LOCATION = $OrtLibLocation
$env:ORT_DYLIB_PATH = $OrtDylibPath

# ── Summary ─────────────────────────────────────────────────────────────────
Write-Host "============================================" -ForegroundColor Cyan
Write-Host "  Setup Complete" -ForegroundColor Cyan
Write-Host "============================================" -ForegroundColor Cyan
Write-Host ""
Write-Host "  ORT Version : $OrtVersion" -ForegroundColor White
Write-Host "  Install Dir : $OrtExtractedDir" -ForegroundColor White
Write-Host "  Library     : $OrtLibLocation" -ForegroundColor White
Write-Host ""
Write-Host "Next steps:" -ForegroundColor Yellow
Write-Host ""
Write-Host "  Build & Run (recommended):" -ForegroundColor White
Write-Host "    .\dev\build_core.ps1" -ForegroundColor Cyan
Write-Host ""
Write-Host "  Build Embed only (env already set in this session):" -ForegroundColor White
Write-Host "    cd core" -ForegroundColor Cyan
Write-Host "    cargo build --release -p acowork-embed" -ForegroundColor Cyan
Write-Host ""
