# RollBall LSP install script: vscode-json-language-server
# Phases: Install -> Verify -> Health Check
#
# vscode-json-languageserver (old npm package) is deprecated and removed.
# The CSS/HTML/JSON language servers are now bundled together in
# 'vscode-langservers-extracted'. Installing that ONE package gives
# us vscode-css-language-server, vscode-html-language-server, and
# vscode-json-language-server all at once.
# Note: the new executable names use "language-server" (with dash),
#   not "languageserver" (old format).

$ErrorActionPreference = "Stop"

$Binary = "vscode-json-language-server"
$LegacyBinary = "vscode-json-languageserver"
$NpmPackage = "vscode-langservers-extracted"

# -- Helpers -----------------------------------------------------------

function Add-ToPath {
    param([string]$Dir)
    $currentUserPath = [Environment]::GetEnvironmentVariable("PATH", "User")
    if ($currentUserPath -notlike "*$Dir*") {
        [Environment]::SetEnvironmentVariable("PATH", "$currentUserPath;$Dir", "User")
        Write-Host "Added $Dir to persistent user PATH." -ForegroundColor Green
    }
    if ($env:PATH -notlike "*$Dir*") {
        $env:PATH = "$env:PATH;$Dir"
    }
}

function Find-NpmGlobalBin {
    param([string]$Name)
    $npmBin = "$env:APPDATA\npm"
    foreach ($n in @("$Name.cmd", "$Name.exe", "$Name")) {
        $candidate = Join-Path $npmBin $n
        if (Test-Path $candidate) { return $candidate }
    }
    return $null
}

# -- Phase 1: Install -------------------------------------------------
function Install-Server {
    Write-Host "[1/3] Installing $Binary (from $NpmPackage)..."

    # Already on PATH (new or legacy name)?
    $existing = Get-Command $Binary -ErrorAction SilentlyContinue
    if ($existing) {
        Write-Host "$Binary already on PATH at $($existing.Source)" -ForegroundColor Green
        return
    }
    $existingLegacy = Get-Command $LegacyBinary -ErrorAction SilentlyContinue
    if ($existingLegacy) {
        Write-Host "Legacy $LegacyBinary already on PATH at $($existingLegacy.Source) — no need to reinstall" -ForegroundColor Green
        return
    }

    # Not on PATH — search npm global bin (new + legacy name)
    $found = Find-NpmGlobalBin $Binary
    if ($found) {
        Write-Host "Found $found — adding npm global bin to PATH..." -ForegroundColor Yellow
        Add-ToPath "$env:APPDATA\npm"
        return
    }
    $foundLegacy = Find-NpmGlobalBin $LegacyBinary
    if ($foundLegacy) {
        Write-Host "Found legacy $foundLegacy — adding npm global bin to PATH..." -ForegroundColor Yellow
        Add-ToPath "$env:APPDATA\npm"
        return
    }

    # Not installed — install vscode-langservers-extracted
    if (Get-Command npm -ErrorAction SilentlyContinue) {
        Write-Host "Installing $NpmPackage (bundles CSS + HTML + JSON servers)..." -ForegroundColor Cyan
        npm install -g $NpmPackage
    } else {
        Write-Host "ERROR: npm not found. Install Node.js first: https://nodejs.org" -ForegroundColor Red
        exit 1
    }
}

# -- Phase 2: Verify --------------------------------------------------
function Verify-Server {
    Write-Host "[2/3] Verifying $Binary is on PATH..."

    $cmd = Get-Command $Binary -ErrorAction SilentlyContinue
    if ($cmd) {
        Write-Host "OK: $Binary found at $($cmd.Source)" -ForegroundColor Green
        return
    }
    $cmdLegacy = Get-Command $LegacyBinary -ErrorAction SilentlyContinue
    if ($cmdLegacy) {
        Write-Host "OK: Legacy $LegacyBinary found at $($cmdLegacy.Source)" -ForegroundColor Green
        return
    }

    # Search npm global bin (new + legacy name)
    $found = Find-NpmGlobalBin $Binary
    if ($found) {
        Write-Host "Found $found — adding npm global bin to PATH..." -ForegroundColor Yellow
        Add-ToPath "$env:APPDATA\npm"
        $cmd = Get-Command $Binary -ErrorAction SilentlyContinue
        if ($cmd) {
            Write-Host "OK: $Binary found at $($cmd.Source)" -ForegroundColor Green
            return
        }
    }
    $foundLegacy = Find-NpmGlobalBin $LegacyBinary
    if ($foundLegacy) {
        Write-Host "Found legacy $foundLegacy — adding npm global bin to PATH..." -ForegroundColor Yellow
        Add-ToPath "$env:APPDATA\npm"
        $cmd = Get-Command $Binary -ErrorAction SilentlyContinue
        if ($cmd) {
            Write-Host "OK: $Binary found at $($cmd.Source)" -ForegroundColor Green
            return
        }
    }

    Write-Host "ERROR: $Binary not found on PATH after install" -ForegroundColor Red
    exit 1
}

# -- Phase 3: Health Check --------------------------------------------
function Health-Check {
    Write-Host "[3/3] Health check: testing stdio handshake..."
    $initMsg = '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{},"rootUri":"file:///C:/tmp"}}'
    $header = "Content-Length: $($initMsg.Length)`r`n`r`n"
    $input = $header + $initMsg

    try {
        Set-Content -Path "$env:TEMP\lsp_init.txt" -Value $input -NoNewline

        $testBinary = $Binary
        if (-not (Get-Command $Binary -ErrorAction SilentlyContinue)) {
            $testBinary = $LegacyBinary
        }

        $procPath = $testBinary
        $procArgs = "--stdio"
        if ((Get-Command $testBinary -ErrorAction SilentlyContinue).Source -like "*.cmd") {
            $procPath = "cmd.exe"
            $procArgs = "/c $testBinary --stdio"
        }
        $proc = Start-Process -FilePath $procPath -ArgumentList $procArgs -NoNewWindow `
            -RedirectStandardInput "$env:TEMP\lsp_init.txt" `
            -RedirectStandardOutput "$env:TEMP\lsp_out.txt" `
            -RedirectStandardError "$env:TEMP\lsp_err.txt" `
            -PassThru
        $proc | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
        if (!$proc.HasExited) { $proc | Stop-Process -Force }

        $output = Get-Content "$env:TEMP\lsp_out.txt" -Raw -ErrorAction SilentlyContinue
        if ($output -and $output.Contains("Content-Length")) {
            Write-Host "OK: $testBinary responds to LSP initialize (--stdio mode)" -ForegroundColor Green
        } else {
            Write-Host "WARN: $testBinary did not respond to handshake" -ForegroundColor Yellow
        }
    } catch {
        Write-Host "WARN: Health check failed: $_" -ForegroundColor Yellow
    }
}

# -- Main --------------------------------------------------------------
Write-Host "=== RollBall LSP Setup: vscode-json-language-server ==="
Install-Server
Verify-Server
Health-Check
Write-Host "=== Done ==="
