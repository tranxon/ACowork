# RollBall LSP install script: clangd (C/C++)
# Phases: Install -> Verify -> Health Check

$ErrorActionPreference = "Stop"

$Binary = "clangd"

# -- Phase 1: Install -------------------------------------------------
function Install-Server {
    Write-Host "[1/3] Installing clangd..."

    # Check if clangd is already on PATH
    $existing = Get-Command $Binary -ErrorAction SilentlyContinue
    if ($existing) {
        Write-Host "clangd already on PATH at $($existing.Source)" -ForegroundColor Green
        return
    }

    # Not on PATH — try to find it in common LLVM install locations
    $llvmDirs = @(
        "$env:ProgramFiles\LLVM\bin",
        "${env:ProgramFiles(x86)}\LLVM\bin",
        "$env:LOCALAPPDATA\LLVM\bin"
    )
    $foundPath = $null
    foreach ($dir in $llvmDirs) {
        $candidate = Join-Path $dir "clangd.exe"
        if (Test-Path $candidate) {
            $foundPath = $dir
            Write-Host "Found clangd at $candidate (not on PATH)" -ForegroundColor Yellow
            break
        }
    }

    if ($foundPath) {
        # LLVM installed but bin/ not on PATH — add it
        Write-Host "Adding $foundPath to user PATH..."
        $currentUserPath = [Environment]::GetEnvironmentVariable("PATH", "User")
        if ($currentUserPath -notlike "*$foundPath*") {
            [Environment]::SetEnvironmentVariable("PATH", "$currentUserPath;$foundPath", "User")
            Write-Host "OK: Added to persistent user PATH. Restart Gateway after install." -ForegroundColor Green
        }
        # Also add to current session so verify can find it
        $env:PATH = "$env:PATH;$foundPath"
        return
    }

    # Not installed at all — try winget
    if (Get-Command winget -ErrorAction SilentlyContinue) {
        Write-Host "Installing LLVM via winget..."
        winget install LLVM.LLVM --accept-source-agreements 2>$null
        if ($LASTEXITCODE -eq 0) {
            Write-Host "LLVM installed successfully. Restart your terminal/Gateway for PATH to update." -ForegroundColor Green
            # winget should add to PATH; refresh current session
            $env:PATH = [Environment]::GetEnvironmentVariable("PATH", "User") + ";" + [Environment]::GetEnvironmentVariable("PATH", "Machine")
        } else {
            Write-Host "winget install returned exit code $LASTEXITCODE (may already be installed)" -ForegroundColor Yellow
            # Try again to find it after refresh
            $env:PATH = [Environment]::GetEnvironmentVariable("PATH", "User") + ";" + [Environment]::GetEnvironmentVariable("PATH", "Machine")
            $refreshed = Get-Command $Binary -ErrorAction SilentlyContinue
            if ($refreshed) {
                Write-Host "clangd found after PATH refresh at $($refreshed.Source)" -ForegroundColor Green
            }
        }
    } else {
        Write-Host "NOTE: Automatic install not available on this system." -ForegroundColor Yellow
        Write-Host "Please install LLVM/clangd manually:" -ForegroundColor Cyan
        Write-Host "  Option 1: Download installer from https://releases.llvm.org/" -ForegroundColor Cyan
        Write-Host "  Option 2: Install via Visual Studio C++ Clang tools component" -ForegroundColor Cyan
        Write-Host "  Option 3: Install via winget: winget install LLVM.LLVM" -ForegroundColor Cyan
        Write-Host ""
        Write-Host "Press Enter to continue after manual install, or Ctrl+C to abort..." -ForegroundColor Yellow
        $null = Read-Host
    }
}

# -- Phase 2: Verify --------------------------------------------------
function Verify-Server {
    Write-Host "[2/3] Verifying clangd is on PATH..."
    $cmd = Get-Command $Binary -ErrorAction SilentlyContinue
    if ($cmd) {
        Write-Host "OK: clangd found at $($cmd.Source)" -ForegroundColor Green
        return
    }

    # Still not found — try common LLVM install locations one more time
    $llvmDirs = @(
        "$env:ProgramFiles\LLVM\bin",
        "${env:ProgramFiles(x86)}\LLVM\bin",
        "$env:LOCALAPPDATA\LLVM\bin"
    )
    foreach ($dir in $llvmDirs) {
        $candidate = Join-Path $dir "clangd.exe"
        if (Test-Path $candidate) {
            Write-Host "Found clangd at $candidate — adding to PATH now..." -ForegroundColor Yellow
            $currentUserPath = [Environment]::GetEnvironmentVariable("PATH", "User")
            if ($currentUserPath -notlike "*$dir*") {
                [Environment]::SetEnvironmentVariable("PATH", "$currentUserPath;$dir", "User")
            }
            $env:PATH = "$env:PATH;$dir"
            $cmd = Get-Command $Binary -ErrorAction SilentlyContinue
            if ($cmd) {
                Write-Host "OK: clangd found at $($cmd.Source)" -ForegroundColor Green
                return
            }
        }
    }

    Write-Host "ERROR: clangd not found on PATH after install" -ForegroundColor Red
    Write-Host "Make sure LLVM bin directory is on your PATH" -ForegroundColor Yellow
    exit 1
}

# -- Phase 3: Health Check --------------------------------------------
# clangd defaults to stdio mode; no --stdio flag needed.
function Health-Check {
    Write-Host "[3/3] Health check: testing stdio handshake..."
    $initMsg = '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{},"rootUri":"file:///C:/tmp"}}'
    $header = "Content-Length: $($initMsg.Length)`r`n`r`n"
    $input = $header + $initMsg

    try {
        # Write stdin input BEFORE starting the process to avoid a race
        # where the process reads an empty file.
        Set-Content -Path "$env:TEMP\lsp_init.txt" -Value $input -NoNewline
        $proc = Start-Process -FilePath $Binary -NoNewWindow -RedirectStandardInput "$env:TEMP\lsp_init.txt" -RedirectStandardOutput "$env:TEMP\lsp_out.txt" -RedirectStandardError "$env:TEMP\lsp_err.txt" -PassThru
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