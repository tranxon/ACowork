# ACowork LSP install script: kotlin-language-server
# Phases: Install -> Verify -> Health Check
#
# kotlin-language-server is a native binary (Kotlin/Native). It does NOT
# require a JDK at runtime. Installation is via GitHub releases or
# VS Code extension (fwcd.kotlin).
#
# Prerequisites: none (standalone binary)

$ErrorActionPreference = "Stop"

$Binary = "kotlin-language-server"

# GitHub releases URL
$GITHUB_RELEASES = "https://github.com/fwcd/kotlin-language-server/releases"
$INSTALL_DIR = "$env:LOCALAPPDATA\kotlin-language-server"

# -- Helper: persist a directory on the user PATH -------------------
function Add-ToPath {
    param([string]$Dir)
    $currentUserPath = [Environment]::GetEnvironmentVariable("PATH", "User")
    if ($currentUserPath -notlike "*$Dir*") {
        [Environment]::SetEnvironmentVariable("PATH", "$currentUserPath;$Dir", "User")
        Write-Host "  Added to user PATH: $Dir" -ForegroundColor Green
    }
    if ($env:PATH -notlike "*$Dir*") {
        $env:PATH = "$env:PATH;$Dir"
    }
}

# -- Helper: search for kotlin-language-server in common locations --
function Find-KotlinLs {
    $names = @("kotlin-language-server.exe", "kotlin-language-server.cmd", "kotlin-language-server.bat", "kotlin-language-server")

    $searchDirs = @()

    # 1. VS Code Kotlin extension (fwcd.kotlin)
    $vscodeExt = "$env:USERPROFILE\.vscode\extensions"
    if (Test-Path $vscodeExt) {
        $kotlinDirs = Get-ChildItem $vscodeExt -Directory -Filter "fwcd.kotlin-*" -ErrorAction SilentlyContinue
        foreach ($kd in $kotlinDirs) {
            $binDir = Join-Path $kd.FullName "server\bin"
            if (Test-Path $binDir) { $searchDirs += $binDir }
        }
    }

    # 2. Common install paths
    $searchDirs += @(
        "$INSTALL_DIR\bin",
        "$env:ProgramFiles\kotlin-language-server\bin",
        "$env:LOCALAPPDATA\kotlin-language-server\bin"
    )

    foreach ($dir in $searchDirs) {
        if (-not (Test-Path $dir)) { continue }
        foreach ($name in $names) {
            $candidate = Join-Path $dir $name
            if (Test-Path $candidate) {
                return $candidate
            }
        }
    }
    return $null
}

# -- Phase 1: Install -----------------------------------------------
function Install-KotlinLs {
    Write-Host "[1/3] Installing kotlin-language-server..."

    # Already on PATH?
    $onPath = Get-Command $Binary -ErrorAction SilentlyContinue
    if ($onPath) {
        Write-Host "kotlin-language-server already on PATH at $($onPath.Source)"
        return
    }

    # Check VS Code extension
    $vscodeBin = Find-KotlinLs
    if ($vscodeBin) {
        $vscodeDir = Split-Path $vscodeBin -Parent
        Write-Host "Found kotlin-language-server from VS Code extension at $vscodeBin — adding to PATH..."
        Add-ToPath $vscodeDir
        $onPath = Get-Command $Binary -ErrorAction SilentlyContinue
        if ($onPath) { return }
    }

    # Not installed — download from GitHub releases
    Write-Host "kotlin-language-server not found."
    Write-Host ""
    Write-Host "Automatic download from GitHub releases is not yet implemented for Windows."
    Write-Host "Please install manually:"
    Write-Host "  1. Download the latest Windows binary from: $GITHUB_RELEASES"
    Write-Host "  2. Extract to $INSTALL_DIR"
    Write-Host "  3. Add $INSTALL_DIR\bin to your PATH"
    Write-Host ""
    Write-Host "Or install the VS Code 'Kotlin Language' extension (fwcd.kotlin),"
    Write-Host "which bundles kotlin-language-server."
    exit 1
}

# -- Phase 2: Verify ------------------------------------------------
function Verify-KotlinLs {
    Write-Host "[2/3] Verifying kotlin-language-server is on PATH..."

    $onPath = Get-Command $Binary -ErrorAction SilentlyContinue
    if ($onPath) {
        Write-Host "OK: kotlin-language-server found at $($onPath.Source)"
        return
    }

    # Check VS Code extension
    $vscodeBin = Find-KotlinLs
    if ($vscodeBin) {
        $vscodeDir = Split-Path $vscodeBin -Parent
        Write-Host "Found kotlin-language-server from VS Code extension at $vscodeBin — adding to PATH..."
        Add-ToPath $vscodeDir
        $onPath = Get-Command $Binary -ErrorAction SilentlyContinue
        if ($onPath) {
            Write-Host "OK: kotlin-language-server found at $($onPath.Source)"
            return
        }
    }

    Write-Host ""
    Write-Host "ERROR: kotlin-language-server not found on PATH after install."
    Write-Host "Install manually: download from $GITHUB_RELEASES"
    exit 1
}

# -- Phase 3: Health Check ------------------------------------------
function HealthCheck-KotlinLs {
    Write-Host "[3/3] Health check: testing stdio handshake..."

    $initMsg = '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{},"rootUri":"file:///tmp"}}'
    $header = "Content-Length: $($initMsg.Length)`r`n`r`n"

    try {
        $response = ($header + $initMsg) | & $Binary 2>$null | Select-Object -First 1
        if ($response -and $response -match "Content-Length") {
            Write-Host "OK: kotlin-language-server responds to LSP initialize (stdio mode)"
        } else {
            Write-Host "WARN: kotlin-language-server did not respond to handshake"
        }
    } catch {
        Write-Host "WARN: kotlin-language-server did not respond to handshake"
    }
}

# -- Main -----------------------------------------------------------
Write-Host "=== ACowork LSP Setup: kotlin-language-server ==="
Install-KotlinLs
Verify-KotlinLs
HealthCheck-KotlinLs
Write-Host "=== Done ==="
