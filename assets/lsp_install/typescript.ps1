# RollBall LSP install script: typescript-language-server
# Phases: Install -> Verify -> Health Check

$ErrorActionPreference = "Stop"

$Binary = "typescript-language-server"

# -- Phase 1: Install -------------------------------------------------
function Install-Server {
    Write-Host "[1/3] Installing typescript-language-server..."
    if (Get-Command npm -ErrorAction SilentlyContinue) {
        npm install -g typescript-language-server typescript
    } else {
        Write-Host "ERROR: npm not found. Install Node.js first: https://nodejs.org" -ForegroundColor Red
        exit 1
    }
}

# -- Phase 2: Verify --------------------------------------------------
function Verify-Server {
    Write-Host "[2/3] Verifying typescript-language-server is on PATH..."
    $cmd = Get-Command $Binary -ErrorAction SilentlyContinue
    if ($cmd) {
        Write-Host "OK: typescript-language-server found at $($cmd.Source)" -ForegroundColor Green
    } else {
        # On Windows, .cmd variant may exist
        $cmdCmd = Get-Command "$Binary.cmd" -ErrorAction SilentlyContinue
        if ($cmdCmd) {
            Write-Host "OK: typescript-language-server.cmd found at $($cmdCmd.Source)" -ForegroundColor Green
        } else {
            Write-Host "ERROR: typescript-language-server not found on PATH after install" -ForegroundColor Red
            exit 1
        }
    }
}

# -- Phase 3: Health Check --------------------------------------------
# typescript-language-server requires --stdio flag.
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
            Write-Host "OK: typescript-language-server responds to LSP initialize (--stdio mode)" -ForegroundColor Green
        } else {
            Write-Host "WARN: typescript-language-server did not respond to handshake" -ForegroundColor Yellow
        }
    } catch {
        Write-Host "WARN: Health check failed: $_" -ForegroundColor Yellow
    }
}

# -- Main --------------------------------------------------------------
Write-Host "=== RollBall LSP Setup: typescript-language-server ==="
Install-Server
Verify-Server
Health-Check
Write-Host "=== Done ==="