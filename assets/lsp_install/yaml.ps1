# RollBall LSP install script: yaml-language-server
# Phases: Install -> Verify -> Health Check

# ── Helpers ──────────────────────────────────────────────────────────

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
    $candidate = Join-Path $npmBin "$Name.cmd"
    if (Test-Path $candidate) { return $candidate }
    $candidate = Join-Path $npmBin "$Name.exe"
    if (Test-Path $candidate) { return $candidate }
    return $null
}

$ErrorActionPreference = "Stop"

$Binary = "yaml-language-server"

# -- Phase 1: Install -------------------------------------------------
function Install-Server {
    Write-Host "[1/3] Installing yaml-language-server..."

    # Already on PATH?
    $existing = Get-Command $Binary -ErrorAction SilentlyContinue
    if ($existing) {
        Write-Host "yaml-language-server already on PATH at $($existing.Source)" -ForegroundColor Green
        return
    }

    # Not on PATH — search npm global bin (may not be on PATH for stale processes)
    $found = Find-NpmGlobalBin $Binary
    if ($found) {
        Write-Host "Found $found — adding npm global bin to PATH..." -ForegroundColor Yellow
        Add-ToPath "$env:APPDATA\npm"
        return
    }

    # Not installed — run npm install
    if (Get-Command npm -ErrorAction SilentlyContinue) {
        npm install -g yaml-language-server
    } else {
        Write-Host "ERROR: npm not found. Install Node.js first: https://nodejs.org" -ForegroundColor Red
        exit 1
    }
}

# -- Phase 2: Verify --------------------------------------------------
function Verify-Server {
    Write-Host "[2/3] Verifying yaml-language-server is on PATH..."
    $cmd = Get-Command $Binary -ErrorAction SilentlyContinue
    if ($cmd) {
        Write-Host "OK: yaml-language-server found at $($cmd.Source)" -ForegroundColor Green
        return
    }

    $cmdCmd = Get-Command "$Binary.cmd" -ErrorAction SilentlyContinue
    if ($cmdCmd) {
        Write-Host "OK: yaml-language-server.cmd found at $($cmdCmd.Source)" -ForegroundColor Green
        return
    }

    # Still not found — search npm global bin
    $found = Find-NpmGlobalBin $Binary
    if ($found) {
        Write-Host "Found $found — adding npm global bin to PATH..." -ForegroundColor Yellow
        Add-ToPath "$env:APPDATA\npm"
        $cmd = Get-Command $Binary -ErrorAction SilentlyContinue
        if ($cmd) {
            Write-Host "OK: yaml-language-server found at $($cmd.Source)" -ForegroundColor Green
            return
        }
    }

    Write-Host "ERROR: yaml-language-server not found on PATH after install" -ForegroundColor Red
    exit 1
}

# -- Phase 3: Health Check --------------------------------------------
# yaml-language-server requires --stdio flag.
function Health-Check {
    Write-Host "[3/3] Health check: testing stdio handshake..."
    $initMsg = '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{},"rootUri":"file:///C:/tmp"}}'
    $header = "Content-Length: $($initMsg.Length)`r`n`r`n"
    $input = $header + $initMsg

    try {
        # Write stdin input BEFORE starting the process to avoid a race
        # where the process reads an empty file.
        Set-Content -Path "$env:TEMP\lsp_init.txt" -Value $input -NoNewline

        # On Windows, npm global packages are .cmd wrappers — Start-Process
        # can't execute them directly. Route through cmd /c when needed.
        $procPath = $Binary
        $procArgs = "--stdio"
        if ((Get-Command $Binary -ErrorAction SilentlyContinue).Source -like "*.cmd") {
            $procPath = "cmd.exe"
            $procArgs = "/c $Binary --stdio"
        }
        $proc = Start-Process -FilePath $procPath -ArgumentList $procArgs -NoNewWindow -RedirectStandardInput "$env:TEMP\lsp_init.txt" -RedirectStandardOutput "$env:TEMP\lsp_out.txt" -RedirectStandardError "$env:TEMP\lsp_err.txt" -PassThru
        $proc | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
        if (!$proc.HasExited) { $proc | Stop-Process -Force }

        $output = Get-Content "$env:TEMP\lsp_out.txt" -Raw -ErrorAction SilentlyContinue
        if ($output -and $output.Contains("Content-Length")) {
            Write-Host "OK: yaml-language-server responds to LSP initialize (--stdio mode)" -ForegroundColor Green
        } else {
            Write-Host "WARN: yaml-language-server did not respond to handshake" -ForegroundColor Yellow
        }
    } catch {
        Write-Host "WARN: Health check failed: $_" -ForegroundColor Yellow
    }
}

# -- Main --------------------------------------------------------------
Write-Host "=== RollBall LSP Setup: yaml-language-server ==="
Install-Server
Verify-Server
Health-Check
Write-Host "=== Done ==="
