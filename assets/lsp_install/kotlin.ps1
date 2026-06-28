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
        "$INSTALL_DIR\server\bin",
        "$INSTALL_DIR\bin",
        "$env:ProgramFiles\kotlin-language-server\server\bin",
        "$env:ProgramFiles\kotlin-language-server\bin",
        "$env:LOCALAPPDATA\kotlin-language-server\server\bin",
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
    Write-Host "kotlin-language-server not found. Downloading from GitHub releases..."
    Write-Host "  Source: $GITHUB_RELEASES"
    Write-Host "  Target: $INSTALL_DIR"
    Write-Host "  Note: ~87 MB download — this may take a few minutes..." -ForegroundColor Yellow

    $tempZip = "$env:TEMP\kotlin-language-server.zip"

    # Remove stale temp files
    if (Test-Path $tempZip) { Remove-Item $tempZip -Force }

    try {
        # Download from GitHub latest release (redirect URL).
        # Use curl.exe instead of Invoke-WebRequest because:
        # - Invoke-WebRequest -UseBasicParsing produces NO output during download,
        #   which triggers the idle-timeout watchdog (60s with no stdout/stderr).
        # - curl.exe prints progress to stderr, keeping the watchdog alive.
        Write-Host "  Downloading (curl)..." 
        $curlArgs = @(
            "-L", "-o", $tempZip,
            "--progress-bar",
            "https://github.com/fwcd/kotlin-language-server/releases/latest/download/server.zip"
        )
        & curl.exe @curlArgs 2>&1 | ForEach-Object {
            # Forward curl's stderr progress to stdout so the idle watchdog sees output
            Write-Host $_
        }
        if ($LASTEXITCODE -ne 0) {
            throw "curl exited with code $LASTEXITCODE"
        }

        $sizeMB = [math]::Round((Get-Item $tempZip).Length / 1MB, 1)
        Write-Host "  Downloaded: ${sizeMB}MB" -ForegroundColor Green

        # Create install directory
        if (Test-Path $INSTALL_DIR) {
            Write-Host "  Removing previous installation..."
            Remove-Item $INSTALL_DIR -Recurse -Force -ErrorAction SilentlyContinue
        }
        New-Item -ItemType Directory -Path $INSTALL_DIR -Force | Out-Null

        # Extract
        Write-Host "  Extracting..."
        Expand-Archive -Path $tempZip -DestinationPath $INSTALL_DIR -Force

        # The server.zip contains a 'server/' directory with the binary inside.
        # Typical layout: server/bin/kotlin-language-server.bat
        $binDir = Join-Path $INSTALL_DIR "server\bin"
        if (-not (Test-Path $binDir)) {
            # Some releases may have a different layout — search for the binary
            $found = Get-ChildItem $INSTALL_DIR -Recurse -Filter "kotlin-language-server*" `
                -ErrorAction SilentlyContinue | Select-Object -First 1
            if ($found) {
                $binDir = Split-Path $found.FullName -Parent
            }
        }

        if (Test-Path $binDir) {
            Write-Host "  Extraction complete: kotlin-language-server installed at $binDir" -ForegroundColor Green
            Add-ToPath $binDir
        } else {
            throw "Extraction completed but kotlin-language-server binary not found"
        }
    } catch {
        $msg = "ERROR: Failed to download or extract kotlin-language-server: $_"
        [Console]::Error.WriteLine($msg)
        Write-Host $msg -ForegroundColor Red
        Write-Host ""
        Write-Host "You can install manually:" -ForegroundColor Yellow
        Write-Host "  1. Download the latest server.zip from: $GITHUB_RELEASES"
        Write-Host "  2. Extract to $INSTALL_DIR"
        Write-Host "  3. Add $INSTALL_DIR\server\bin to your PATH"
        Write-Host ""
        Write-Host "Or install the VS Code 'Kotlin Language' extension (fwcd.kotlin),"
        Write-Host "which bundles kotlin-language-server."
        exit 1
    } finally {
        # Clean up temp file
        if (Test-Path $tempZip) { Remove-Item $tempZip -Force -ErrorAction SilentlyContinue }
    }
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
