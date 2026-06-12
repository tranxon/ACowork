# RollBall LSP install script: rust-analyzer
# Phases: Install -> Verify -> Health Check

$ErrorActionPreference = "Stop"

$Binary = "rust-analyzer"

# -- Phase 1: Install -------------------------------------------------
function Install-Server {
    Write-Host "[1/3] Installing rust-analyzer..."
    if (Get-Command rustup -ErrorAction SilentlyContinue) {
        rustup component add rust-analyzer
    } else {
        Write-Host "ERROR: rustup not found. Install Rust first: https://rustup.rs" -ForegroundColor Red
        exit 1
    }
}

# -- Phase 2: Verify --------------------------------------------------
function Verify-Server {
    Write-Host "[2/3] Verifying rust-analyzer is on PATH..."
    $cmd = Get-Command $Binary -ErrorAction SilentlyContinue
    if ($cmd) {
        Write-Host "OK: rust-analyzer found at $($cmd.Source)" -ForegroundColor Green
    } else {
        Write-Host "ERROR: rust-analyzer not found on PATH after install" -ForegroundColor Red
        exit 1
    }
}

# -- Phase 3: Health Check --------------------------------------------
# rust-analyzer defaults to stdio mode; no --stdio flag needed.
function Health-Check {
    Write-Host "[3/3] Health check: testing stdio handshake..."
    $initMsg = '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{},"rootUri":"file:///C:/tmp"}}'
    $header = "Content-Length: $($initMsg.Length)`r`n`r`n"
    $input = $header + $initMsg

    try {
        $proc = Start-Process -FilePath $Binary -NoNewWindow -RedirectStandardInput "$env:TEMP\lsp_init.txt" -RedirectStandardOutput "$env:TEMP\lsp_out.txt" -RedirectStandardError "$env:TEMP\lsp_err.txt" -PassThru
        Set-Content -Path "$env:TEMP\lsp_init.txt" -Value $input -NoNewline
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
Write-Host "=== RollBall LSP Setup: rust-analyzer ==="
Install-Server
Verify-Server
Health-Check
Write-Host "=== Done ==="