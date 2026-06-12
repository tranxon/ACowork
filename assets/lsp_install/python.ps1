# RollBall LSP install script: pylsp (Python)
# Phases: Install -> Verify -> Health Check

$ErrorActionPreference = "Stop"

$Binary = "pylsp"

# -- Phase 1: Install -------------------------------------------------
function Install-Server {
    Write-Host "[1/3] Installing python-lsp-server..."
    if (Get-Command pip -ErrorAction SilentlyContinue) {
        pip install python-lsp-server
    } elseif (Get-Command pip3 -ErrorAction SilentlyContinue) {
        pip3 install python-lsp-server
    } else {
        Write-Host "ERROR: pip not found. Install Python first: https://python.org" -ForegroundColor Red
        exit 1
    }
}

# -- Phase 2: Verify --------------------------------------------------
function Verify-Server {
    Write-Host "[2/3] Verifying pylsp is on PATH..."
    $cmd = Get-Command $Binary -ErrorAction SilentlyContinue
    if ($cmd) {
        Write-Host "OK: pylsp found at $($cmd.Source)" -ForegroundColor Green
    } else {
        Write-Host "ERROR: pylsp not found on PATH after install" -ForegroundColor Red
        exit 1
    }
}

# -- Phase 3: Health Check --------------------------------------------
# pylsp requires --stdio flag for LSP communication.
function Health-Check {
    Write-Host "[3/3] Health check: testing stdio handshake..."
    $initMsg = '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{},"rootUri":"file:///C:/tmp"}}'
    $header = "Content-Length: $($initMsg.Length)`r`n`r`n"
    $input = $header + $initMsg

    try {
        $proc = Start-Process -FilePath $Binary -ArgumentList "--stdio" -NoNewWindow -RedirectStandardInput "$env:TEMP\lsp_init.txt" -RedirectStandardOutput "$env:TEMP\lsp_out.txt" -RedirectStandardError "$env:TEMP\lsp_err.txt" -PassThru
        Set-Content -Path "$env:TEMP\lsp_init.txt" -Value $input -NoNewline
        $proc | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
        if (!$proc.HasExited) { $proc | Stop-Process -Force }

        $output = Get-Content "$env:TEMP\lsp_out.txt" -Raw -ErrorAction SilentlyContinue
        if ($output -and $output.Contains("Content-Length")) {
            Write-Host "OK: pylsp responds to LSP initialize (--stdio mode)" -ForegroundColor Green
        } else {
            Write-Host "WARN: pylsp did not respond to handshake" -ForegroundColor Yellow
        }
    } catch {
        Write-Host "WARN: Health check failed: $_" -ForegroundColor Yellow
    }
}

# -- Main --------------------------------------------------------------
Write-Host "=== RollBall LSP Setup: pylsp (Python) ==="
Install-Server
Verify-Server
Health-Check
Write-Host "=== Done ==="