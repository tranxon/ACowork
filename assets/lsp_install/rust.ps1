# ACowork LSP install script: rust-analyzer
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

$ErrorActionPreference = "Stop"

$Binary = "rust-analyzer"

# -- Phase 1: Install -------------------------------------------------
function Install-Server {
    Write-Host "[1/3] Installing rust-analyzer..."

    # Already on PATH and actually runnable?
    # rustup creates a proxy at ~/.cargo/bin/rust-analyzer.exe that exists
    # on PATH but fails at runtime if the component isn't installed for the
    # active toolchain.  `--version` catches this — the proxy exits non-zero.
    $existing = Get-Command $Binary -ErrorAction SilentlyContinue
    if ($existing) {
        $versionOk = $false
        try {
            & $Binary --version 2>$null | Out-Null
            if ($LASTEXITCODE -eq 0) { $versionOk = $true }
        } catch { }
        if ($versionOk) {
            Write-Host "rust-analyzer already on PATH at $($existing.Source)" -ForegroundColor Green
            return
        }
        Write-Host "rust-analyzer proxy found but --version failed — installing component..." -ForegroundColor Yellow
    }

    # Not on PATH — search ~/.cargo/bin (rustup/rust default install location)
    $cargoBin = "$env:USERPROFILE\.cargo\bin"
    $candidate = Join-Path $cargoBin "$Binary.exe"
    if (Test-Path $candidate) {
        Write-Host "Found rust-analyzer at $candidate — adding $cargoBin to PATH..." -ForegroundColor Yellow
        Add-ToPath $cargoBin
        $versionOk = $false
        try {
            & $Binary --version 2>$null | Out-Null
            if ($LASTEXITCODE -eq 0) { $versionOk = $true }
        } catch { }
        if ($versionOk) { return }
    fi

    # Not installed or broken proxy — install the component
    if (Get-Command rustup -ErrorAction SilentlyContinue) {
        Write-Host "Installing rust-analyzer component via rustup..."
        rustup component add rust-analyzer
    } else {
        Write-Host "ERROR: rustup not found. Install Rust first: https://rustup.rs" -ForegroundColor Red
        exit 1
    }
}

# -- Phase 2: Verify --------------------------------------------------
function Verify-Server {
    Write-Host "[2/3] Verifying rust-analyzer is on PATH and runnable..."
    $cmd = Get-Command $Binary -ErrorAction SilentlyContinue
    if ($cmd) {
        $versionOk = $false
        try {
            & $Binary --version 2>$null | Out-Null
            if ($LASTEXITCODE -eq 0) { $versionOk = $true }
        } catch { }
        if ($versionOk) {
            Write-Host "OK: rust-analyzer found at $($cmd.Source)" -ForegroundColor Green
            return
        }
    }

    # Still not found — search ~/.cargo/bin
    $cargoBin = "$env:USERPROFILE\.cargo\bin"
    $candidate = Join-Path $cargoBin "$Binary.exe"
    if (Test-Path $candidate) {
        Write-Host "Found rust-analyzer at $candidate — adding $cargoBin to PATH..." -ForegroundColor Yellow
        Add-ToPath $cargoBin
        $cmd = Get-Command $Binary -ErrorAction SilentlyContinue
        if ($cmd) {
            $versionOk = $false
            try {
                & $Binary --version 2>$null | Out-Null
                if ($LASTEXITCODE -eq 0) { $versionOk = $true }
            } catch { }
            if ($versionOk) {
                Write-Host "OK: rust-analyzer found at $($cmd.Source)" -ForegroundColor Green
                return
            }
        }
    }

    Write-Host "ERROR: rust-analyzer not found or not runnable on PATH after install" -ForegroundColor Red
    exit 1
}

# -- Phase 3: Health Check --------------------------------------------
# rust-analyzer defaults to stdio mode; no --stdio flag needed.
function Health-Check {
    Write-Host "[3/3] Health check: testing stdio handshake..."
    $initMsg = '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{},"rootUri":"file:///C:/tmp"}}'
    $header = "Content-Length: $($initMsg.Length)`r`n`r`n"
    $input = $header + $initMsg

    try {
        # Write stdin input BEFORE starting the process to avoid a race
        Set-Content -Path "$env:TEMP\lsp_init.txt" -Value $input -NoNewline
        $proc = Start-Process -FilePath $Binary -NoNewWindow -RedirectStandardInput "$env:TEMP\lsp_init.txt" -RedirectStandardOutput "$env:TEMP\lsp_out.txt" -RedirectStandardError "$env:TEMP\lsp_err.txt" -PassThru
        $proc | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
        if (!$proc.HasExited) { $proc | Stop-Process -Force }

        $output = Get-Content "$env:TEMP\lsp_out.txt" -Raw -ErrorAction SilentlyContinue
        if ($output -and $output.Contains("Content-Length")) {
            Write-Host "OK: rust-analyzer responds to LSP initialize (stdio mode)" -ForegroundColor Green
        } else {
            Write-Host "WARN: rust-analyzer did not respond to handshake (may need project context)" -ForegroundColor Yellow
        }
    } catch {
        Write-Host "WARN: Health check failed: $_" -ForegroundColor Yellow
    }
}

# -- Main --------------------------------------------------------------
Write-Host "=== ACowork LSP Setup: rust-analyzer ==="
Install-Server
Verify-Server
Health-Check
Write-Host "=== Done ==="