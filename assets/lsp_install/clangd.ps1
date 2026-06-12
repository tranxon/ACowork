# RollBall LSP install script: clangd (C/C++)
# Phases: Install -> Verify -> Health Check

$ErrorActionPreference = "Stop"

$Binary = "clangd"

# -- Phase 1: Install -------------------------------------------------
function Install-Server {
    Write-Host "[1/3] Installing clangd..."

    # On Windows, LLVM/clangd can be installed via:
    # 1. LLVM installer from https://releases.llvm.org/
    # 2. Visual Studio's C++ Clang tools component
    # 3. winget (if available)

    if (Get-Command winget -ErrorAction SilentlyContinue) {
        winget install LLVM.LLVM
    } else {
        Write-Host "NOTE: Automatic install not available on this system." -ForegroundColor Yellow
        Write-Host "Please install LLVM/clangd manually:" -ForegroundColor Cyan
        Write-Host "  Option 1: Download installer from https://releases.llvm.org/" -ForegroundColor Cyan
        Write-Host "  Option 2: Install via Visual Studio C++ Clang tools component" -ForegroundColor Cyan
        Write-Host "  Option 3: Install via winget: winget install LLVM.LLVM" -ForegroundColor Cyan
        Write-Host ""
        Write-Host "Press any key to continue after manual install, or Ctrl+C to abort..." -ForegroundColor Yellow
        $null = $Host.UI.PromptForChoice("Continue", "Have you installed clangd?", @("&Yes"; "&No"), 0)
    }
}

# -- Phase 2: Verify --------------------------------------------------
function Verify-Server {
    Write-Host "[2/3] Verifying clangd is on PATH..."
    $cmd = Get-Command $Binary -ErrorAction SilentlyContinue
    if ($cmd) {
        Write-Host "OK: clangd found at $($cmd.Source)" -ForegroundColor Green
    } else {
        Write-Host "ERROR: clangd not found on PATH after install" -ForegroundColor Red
        Write-Host "Make sure LLVM bin directory is on your PATH" -ForegroundColor Yellow
        exit 1
    }
}

# -- Phase 3: Health Check --------------------------------------------
# clangd defaults to stdio mode; no --stdio flag needed.
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
            Write-Host "OK: clangd responds to LSP initialize (stdio mode)" -ForegroundColor Green
        } else {
            Write-Host "WARN: clangd did not respond to handshake (may need project context)" -ForegroundColor Yellow
        }
    } catch {
        Write-Host "WARN: Health check failed: $_" -ForegroundColor Yellow
    }
}

# -- Main --------------------------------------------------------------
Write-Host "=== RollBall LSP Setup: clangd (C/C++) ==="
Install-Server
Verify-Server
Health-Check
Write-Host "=== Done ==="