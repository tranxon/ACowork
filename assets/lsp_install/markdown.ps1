# RollBall LSP install script: marksman (Markdown)
# Phases: Install -> Verify -> Health Check
# marksman is a standalone binary (not npm). Defaults to stdio mode.

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

function Find-MarksmanExe {
    # Check winget package directories first
    $wingetBase = "$env:LOCALAPPDATA\Microsoft\WinGet\Packages"
    if (Test-Path $wingetBase) {
        $dirs = Get-ChildItem $wingetBase -Directory -Filter "*Marksman*" -ErrorAction SilentlyContinue
        foreach ($d in $dirs) {
            $exe = Join-Path $d.FullName "marksman.exe"
            if (Test-Path $exe) { return $exe }
        }
    }
    # Also check WinGet Links (shim directory)
    $linkDir = "$env:LOCALAPPDATA\Microsoft\WinGet\Links"
    if (Test-Path $linkDir) {
        $link = Join-Path $linkDir "marksman.exe"
        if (Test-Path $link) { return $link }
    }
    return $null
}

$ErrorActionPreference = "Stop"

$Binary = "marksman"

# -- Phase 1: Install -------------------------------------------------
function Install-Server {
    Write-Host "[1/3] Installing marksman..."

    # Already on PATH?
    $existing = Get-Command $Binary -ErrorAction SilentlyContinue
    if ($existing) {
        Write-Host "marksman already on PATH at $($existing.Source)" -ForegroundColor Green
        return
    }

    # Not on PATH — search common install locations (winget, scoop)
    $found = Find-MarksmanExe
    if ($found) {
        $dir = Split-Path $found -Parent
        Write-Host "Found marksman at $found — adding $dir to PATH..." -ForegroundColor Yellow
        Add-ToPath $dir
        return
    }

    # Not installed — try winget
    if (Get-Command winget -ErrorAction SilentlyContinue) {
        Write-Host "Installing marksman via winget..."
        winget install marksman --accept-source-agreements 2>$null
        if ($LASTEXITCODE -ne 0) {
            Write-Host "winget returned exit code $LASTEXITCODE (may already be installed)" -ForegroundColor Yellow
            # Refresh PATH from registry and search again
            $env:PATH = [Environment]::GetEnvironmentVariable("PATH", "User") + ";" + [Environment]::GetEnvironmentVariable("PATH", "Machine")
            $found = Find-MarksmanExe
            if ($found) {
                $dir = Split-Path $found -Parent
                Write-Host "Found marksman after PATH refresh: $found" -ForegroundColor Green
                Add-ToPath $dir
                return
            }
            Write-Host "Trying scoop..." -ForegroundColor Yellow
            if (Get-Command scoop -ErrorAction SilentlyContinue) {
                scoop install marksman
            } else {
                Write-Host "ERROR: Neither winget nor scoop could install marksman." -ForegroundColor Red
                Write-Host "Download manually from: https://github.com/artempyanykh/marksman/releases" -ForegroundColor Yellow
                exit 1
            }
        }
    } elseif (Get-Command scoop -ErrorAction SilentlyContinue) {
        scoop install marksman
    } else {
        Write-Host "ERROR: No package manager found (winget or scoop)." -ForegroundColor Red
        Write-Host "Download manually from: https://github.com/artempyanykh/marksman/releases" -ForegroundColor Yellow
        exit 1
    }
}

# -- Phase 2: Verify --------------------------------------------------
function Verify-Server {
    Write-Host "[2/3] Verifying marksman is on PATH..."
    # winget may require terminal restart for PATH refresh
    $cmd = Get-Command $Binary -ErrorAction SilentlyContinue
    if ($cmd) {
        Write-Host "OK: marksman found at $($cmd.Source)" -ForegroundColor Green
        return
    }

    # Still not found — search winget packages again
    $found = Find-MarksmanExe
    if ($found) {
        $dir = Split-Path $found -Parent
        Write-Host "Found marksman at $found — adding $dir to PATH..." -ForegroundColor Yellow
        Add-ToPath $dir
        $cmd = Get-Command $Binary -ErrorAction SilentlyContinue
        if ($cmd) {
            Write-Host "OK: marksman found at $($cmd.Source)" -ForegroundColor Green
            return
        }
    }

    Write-Host "ERROR: marksman not found on PATH. You may need to restart your terminal." -ForegroundColor Red
    exit 1
}

# -- Phase 3: Health Check --------------------------------------------
# marksman defaults to stdio mode; no --stdio flag needed.
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
            Write-Host "OK: marksman responds to LSP initialize (stdio mode)" -ForegroundColor Green
        } else {
            Write-Host "WARN: marksman did not respond to handshake (may need workspace context)" -ForegroundColor Yellow
        }
    } catch {
        Write-Host "WARN: Health check failed: $_" -ForegroundColor Yellow
    }
}

# -- Main --------------------------------------------------------------
Write-Host "=== RollBall LSP Setup: marksman (Markdown) ==="
Install-Server
Verify-Server
Health-Check
Write-Host "=== Done ==="
