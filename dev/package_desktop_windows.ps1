#!/usr/bin/env pwsh
# package_desktop_windows.ps1 - Build ACowork Desktop installer for Windows

param(
    [switch] $ReinstallOrt,
    [switch] $NoMirror
)

$ErrorActionPreference = "Stop"
$WorkspaceRoot = Split-Path -Parent $PSScriptRoot
$DesktopDir = Join-Path $WorkspaceRoot "apps\acowork-desktop"
$OrtVersion = "1.22.0"
$OrtDir = Join-Path $WorkspaceRoot ".ort\onnxruntime-win-x64-$OrtVersion"
$OrtLibDir = Join-Path $OrtDir "lib"
$OrtDll = Join-Path $OrtLibDir "onnxruntime.dll"
$BinDir = Join-Path $DesktopDir "src-tauri\bin"

Write-Host "========================================" -ForegroundColor Cyan
Write-Host "ACowork Desktop Package (Windows)" -ForegroundColor Cyan
Write-Host "========================================" -ForegroundColor Cyan
Write-Host ""

if (-not (Test-Path $OrtDll) -or $ReinstallOrt) {
    $setupArgs = @("-Version", $OrtVersion)
    if ($ReinstallOrt) { $setupArgs += "-Reinstall" }
    if ($NoMirror) { $setupArgs += "-NoMirror" }
    & (Join-Path $PSScriptRoot "setup_ort.ps1") @setupArgs
}

if (-not (Test-Path $OrtDll)) {
    Write-Host "ONNX Runtime DLL not found: $OrtDll" -ForegroundColor Red
    exit 1
}

$env:ORT_LIB_LOCATION = $OrtLibDir
$env:ORT_DYLIB_PATH = $OrtDll
$env:ORT_PREFER_DYNAMIC_LINK = "1"
$env:PATH = "$OrtLibDir;$env:PATH"

if (-not (Test-Path $BinDir)) {
    New-Item -ItemType Directory -Path $BinDir -Force | Out-Null
}
Copy-Item -Path $OrtDll -Destination (Join-Path $BinDir "onnxruntime.dll") -Force
Write-Host "Bundled ORT DLL: $OrtDll" -ForegroundColor Green

# Bundle LSP Relay binary (sibling of acowork-gateway.exe, ADR-019).
# The Gateway locates it via `current_exe().parent().join("acowork-lsp-relay.exe")`,
# so without this copy the Tauri app's Gateway supervisor will fail to spawn LSP
# and Monaco / runtime codebase tool will silently lose all LSP features.
$LspRelayBin = Join-Path $WorkspaceRoot "target\release\acowork-lsp-relay.exe"
if (Test-Path $LspRelayBin) {
    Copy-Item -Path $LspRelayBin -Destination (Join-Path $BinDir "acowork-lsp-relay.exe") -Force
    Write-Host "Bundled LSP Relay binary: $LspRelayBin" -ForegroundColor Green
} else {
    Write-Host "WARN: acowork-lsp-relay.exe not found at $LspRelayBin." -ForegroundColor Yellow
    Write-Host "      Run .\dev\build_core.ps1 (release) first." -ForegroundColor Yellow
    Write-Host "      Without it, Gateway startup will fail with:" -ForegroundColor Yellow
    Write-Host "        acowork-lsp-relay binary not found" -ForegroundColor Yellow
}

Push-Location $DesktopDir
try {
    npm run tauri build
} finally {
    Pop-Location
}
