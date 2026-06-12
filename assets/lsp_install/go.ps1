# RollBall LSP install script: gopls (Go)
# Phases: Install -> Verify -> Health Check

$ErrorActionPreference = "Stop"

$Binary = "gopls"

# -- Phase 1: Install -------------------------------------------------
function Install-Server {
    Write-Host "[1/3] Installing gopls..."
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
    } else {
        Write-Host "ERROR: gopls not found on PATH (GOPATH/bin may not be on PATH)" -ForegroundColor Red
        Write-Host "Try: `$env:PATH += `";`" + [System.IO.Path]::Combine((go env GOPATH), 'bin')"
        exit 1
    }
}

# -- Phase 3: Health Check --------------------------------------------
# gopls uses 'serve' subcommand (not --stdio flag).
function Health-Check {
    Write-Host "[3/3] Health check: testing stdio handshake..."
    $initMsg = '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{},"rootUri":"file:///C:/tmp"}}'
    $header = "Content-Length: $($initMsg.Length)`r`n`r`n"
    $input = $header + $initMsg

    try {
        $proc = Start-Process -FilePath $Binary -ArgumentList "serve" -NoNewWindow -RedirectStandardInput "$env:TEMP\lsp_init.txt" -RedirectStandardOutput "$env:TEMP\lsp_out.txt" -RedirectStandardError "$env:TEMP\lsp_err.txt" -PassThru
        Set-Content -Path "$env:TEMP\lsp_init.txt" -Value $input -NoNewline
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