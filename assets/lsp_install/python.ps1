# RollBall LSP install script: pylsp (Python)
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

function Find-PythonScriptsDir {
    # Try sysconfig first (most reliable)
    try {
        $scripts = python -c "import sysconfig; print(sysconfig.get_path('scripts'))" 2>$null
        if ($scripts -and (Test-Path $scripts)) { return $scripts.Trim() }
    } catch {}
    # Fall back to common install locations
    $candidates = @(
        "$env:APPDATA\Python\Python313\Scripts",
        "$env:APPDATA\Python\Python312\Scripts",
        "$env:APPDATA\Python\Python311\Scripts",
        "$env:LOCALAPPDATA\Programs\Python\Python313\Scripts",
        "$env:LOCALAPPDATA\Programs\Python\Python312\Scripts",
        "$env:LOCALAPPDATA\Programs\Python\Python311\Scripts"
    )
    foreach ($d in $candidates) {
        if (Test-Path $d) { return $d }
    }
    return $null
}

$ErrorActionPreference = "Stop"

$Binary = "pylsp"

# -- Phase 1: Install -------------------------------------------------
function Install-Server {
    Write-Host "[1/3] Installing python-lsp-server..."

    # Already on PATH?
    $existing = Get-Command $Binary -ErrorAction SilentlyContinue
    if ($existing) {
        Write-Host "pylsp already on PATH at $($existing.Source)" -ForegroundColor Green
        return
    }

    # Not on PATH — search Python Scripts directory (pip installs there)
    $scriptsDir = Find-PythonScriptsDir
    if ($scriptsDir) {
        $candidate = Join-Path $scriptsDir "$Binary.exe"
        if (Test-Path $candidate) {
            Write-Host "Found pylsp at $candidate — adding $scriptsDir to PATH..." -ForegroundColor Yellow
            Add-ToPath $scriptsDir
            return
        }
    }

    # Not installed — run pip install
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
        return
    }

    # Still not found — search Python Scripts
    $scriptsDir = Find-PythonScriptsDir
    if ($scriptsDir) {
        $candidate = Join-Path $scriptsDir "$Binary.exe"
        if (Test-Path $candidate) {
            Write-Host "Found pylsp at $candidate — adding $scriptsDir to PATH..." -ForegroundColor Yellow
            Add-ToPath $scriptsDir
            $cmd = Get-Command $Binary -ErrorAction SilentlyContinue
            if ($cmd) {
                Write-Host "OK: pylsp found at $($cmd.Source)" -ForegroundColor Green
                return
            }
        }
    }

    Write-Host "ERROR: pylsp not found on PATH after install" -ForegroundColor Red
    exit 1
}

# -- Phase 3: Health Check --------------------------------------------
# pylsp requires --stdio flag for LSP communication.
function Health-Check {
    Write-Host "[3/3] Health check: testing stdio handshake..."
    $initMsg = '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{},"rootUri":"file:///C:/tmp"}}'
    $header = "Content-Length: $($initMsg.Length)`r`n`r`n"
    $input = $header + $initMsg

    try {
        # Write stdin input BEFORE starting the process to avoid a race
        Set-Content -Path "$env:TEMP\lsp_init.txt" -Value $input -NoNewline
        $proc = Start-Process -FilePath $Binary -ArgumentList "--stdio" -NoNewWindow -RedirectStandardInput "$env:TEMP\lsp_init.txt" -RedirectStandardOutput "$env:TEMP\lsp_out.txt" -RedirectStandardError "$env:TEMP\lsp_err.txt" -PassThru
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