# RollBall LSP install script: gopls (Go)
# Phases: Install -> Verify -> Health Check

# ── Helpers ──────────────────────────────────────────────────────────

# Add a directory to the persistent user PATH (and current session).
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

$Binary = "gopls"

# -- Phase 1: Install -------------------------------------------------
function Install-Server {
    Write-Host "[1/3] Installing gopls..."

    # Already on PATH?
    $existing = Get-Command $Binary -ErrorAction SilentlyContinue
    if ($existing) {
        Write-Host "gopls already on PATH at $($existing.Source)" -ForegroundColor Green
        return
    }

    # Not on PATH — search GOPATH/bin (Go installs binaries there, may not be on PATH)
    try {
        $goPath = & go env GOPATH 2>$null
        if ($goPath) {
            $goBin = Join-Path $goPath "bin"
            $candidate = Join-Path $goBin "gopls.exe"
            if (Test-Path $candidate) {
                Write-Host "Found gopls at $candidate — adding $goBin to PATH..." -ForegroundColor Yellow
                Add-ToPath $goBin
                return
            }
        }
    } catch {}

    # Not installed — run go install
    if (Get-Command go -ErrorAction SilentlyContinue) {
        go install golang.org/x/tools/gopls@latest
    } else {
        Write-Host "ERROR: go not found. Install Go first: https://go.dev/dl/" -ForegroundColor Red
        exit 1
    }
}

# -- Phase 2: Verify --------------------------------------------------
function Verify-Server {
    Write-Host "[2/3] Verifying gopls is on PATH..."
    $cmd = Get-Command $Binary -ErrorAction SilentlyContinue
    if ($cmd) {
        Write-Host "OK: gopls found at $($cmd.Source)" -ForegroundColor Green
        return
    }

    # Still not found — search GOPATH/bin
    try {
        $goPath = & go env GOPATH 2>$null
        if ($goPath) {
            $goBin = Join-Path $goPath "bin"
            $candidate = Join-Path $goBin "gopls.exe"
            if (Test-Path $candidate) {
                Write-Host "Found gopls at $candidate — adding $goBin to PATH..." -ForegroundColor Yellow
                Add-ToPath $goBin
                $cmd = Get-Command $Binary -ErrorAction SilentlyContinue
                if ($cmd) {
                    Write-Host "OK: gopls found at $($cmd.Source)" -ForegroundColor Green
                    return
                }
            }
        }
    } catch {}

    Write-Host "ERROR: gopls not found on PATH (GOPATH/bin may not be on PATH)" -ForegroundColor Red
    Write-Host "Try: `$env:PATH += `";`" + [System.IO.Path]::Combine((go env GOPATH), 'bin')"
    exit 1
}

# -- Phase 3: Health Check --------------------------------------------
# gopls uses 'serve' subcommand (not --stdio flag).
function Health-Check {
    Write-Host "[3/3] Health check: testing stdio handshake..."
    $initMsg = '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{},"rootUri":"file:///C:/tmp"}}'
    $header = "Content-Length: $($initMsg.Length)`r`n`r`n"
    $input = $header + $initMsg

    try {
        # Write stdin input BEFORE starting the process to avoid a race
        # where the process reads an empty file.
        Set-Content -Path "$env:TEMP\lsp_init.txt" -Value $input -NoNewline
        $proc = Start-Process -FilePath $Binary -ArgumentList "serve" -NoNewWindow -RedirectStandardInput "$env:TEMP\lsp_init.txt" -RedirectStandardOutput "$env:TEMP\lsp_out.txt" -RedirectStandardError "$env:TEMP\lsp_err.txt" -PassThru
        $proc | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
        if (!$proc.HasExited) { $proc | Stop-Process -Force }

        $output = Get-Content "$env:TEMP\lsp_out.txt" -Raw -ErrorAction SilentlyContinue
        if ($output -and $output.Contains("Content-Length")) {
            Write-Host "OK: gopls responds to LSP initialize (serve mode)" -ForegroundColor Green
        } else {
            Write-Host "WARN: gopls did not respond to handshake (may need Go project context)" -ForegroundColor Yellow
        }
    } catch {
        Write-Host "WARN: Health check failed: $_" -ForegroundColor Yellow
    }
}

# -- Main --------------------------------------------------------------
Write-Host "=== RollBall LSP Setup: gopls (Go) ==="
Install-Server
Verify-Server
Health-Check
Write-Host "=== Done ==="