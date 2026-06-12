# RollBall LSP install script: jdtls (Java)
# Phases: Install -> Verify -> Health Check
#
# jdtls is the Eclipse JDT Language Server, a Java application (not a native
# binary). This script automates download from Eclipse snapshots and installs
# to %LOCALAPPDATA%\jdtls (Windows) or ~/.local/jdtls (Unix).
#
# Prerequisites: JDK 21+ (java on PATH or JAVA_HOME set)

$ErrorActionPreference = "Stop"

$Binary = "jdtls"

# JDT LS snapshot download URL (always the latest build)
$JDTLS_DOWNLOAD_URL = "https://download.eclipse.org/jdtls/snapshots/jdt-language-server-latest.tar.gz"
$JDTLS_INSTALL_DIR = "$env:LOCALAPPDATA\jdtls"

# -- Helper: persist a directory on the user PATH -------------------
function Add-ToPath {
    param([string]$Dir)
    $currentUserPath = [Environment]::GetEnvironmentVariable("PATH", "User")
    if ($currentUserPath -notlike "*$Dir*") {
        [Environment]::SetEnvironmentVariable("PATH", "$currentUserPath;$Dir", "User")
        Write-Host "  Added to user PATH: $Dir" -ForegroundColor Green
    }
    if ($env:PATH -notlike "*$Dir*") {
        $env:PATH = "$env:PATH;$Dir"
    }
}

# -- Helper: set up jdtls from VS Code extension's bundled server ----
# VS Code's jdtls.bat has a 'pause' at the end (hangs the Gateway process).
# Instead, we copy the launcher scripts to our own dir and create a clean
# wrapper .cmd without 'pause'.
function Setup-VsCodeJdtls {
    param([string]$VsCodeBinDir)

    # server/ directory (parent of bin/) — contains plugins, config, etc.
    $serverDir = Split-Path $VsCodeBinDir -Parent
    Write-Host "  VS Code jdtls server dir: $serverDir"

    $targetDir = "$JDTLS_INSTALL_DIR\bin"
    if (-not (Test-Path $targetDir)) {
        New-Item -ItemType Directory -Path $targetDir -Force | Out-Null
    }

    # Create a clean wrapper .cmd (NO pause!) — jdtls.py resolves
    # config/plugins relative to its own location, so cwd doesn't matter.
    $wrapperPath = Join-Path $targetDir "jdtls.cmd"
    $wrapperContent = @"
@echo off
REM RollBall jdtls wrapper - launches Eclipse JDT LS (no pause)
python "$VsCodeBinDir\jdtls" %*
"@
    Set-Content -Path $wrapperPath -Value $wrapperContent -Encoding ASCII

    Write-Host "  Created clean wrapper: $wrapperPath" -ForegroundColor Green
    Write-Host "  (VS Code's jdtls.bat has 'pause' at end — skipped)" -ForegroundColor Yellow
}

# -- Helper: search for jdtls launcher in common locations ----------
function Find-JdtlsDir {
    # jdtls launcher can be named jdtls, jdtls.bat, or jdtls.cmd
    $names = @("jdtls.bat", "jdtls.cmd", "jdtls")

    $searchDirs = @()

    # 1. VS Code Java extension (redhat.java)
    $vscodeExt = "$env:USERPROFILE\.vscode\extensions"
    if (Test-Path $vscodeExt) {
        $javaDirs = Get-ChildItem $vscodeExt -Directory -Filter "redhat.java-*" -ErrorAction SilentlyContinue
        foreach ($jd in $javaDirs) {
            $binDir = Join-Path $jd.FullName "server\bin"
            if (Test-Path $binDir) { $searchDirs += $binDir }
        }
    }

    # 2. Eclipse JDT LS standalone (common install paths)
    $searchDirs += @(
        "$env:ProgramFiles\EclipseJDTLS\bin",
        "${env:ProgramFiles(x86)}\EclipseJDTLS\bin",
        "$env:LOCALAPPDATA\jdtls\bin",
        "$env:APPDATA\jdtls\bin",
        "$env:USERPROFILE\jdtls\bin",
        "$env:USERPROFILE\.jdtls\bin"
    )

    foreach ($dir in $searchDirs) {
        if (-not (Test-Path $dir)) { continue }
        foreach ($name in $names) {
            $candidate = Join-Path $dir $name
            if (Test-Path $candidate) {
                return $dir
            }
        }
    }

    return $null
}

# -- Helper: check if JDK 21+ is available ---------------------------
function Test-JdkAvailable {
    $java = Get-Command java -ErrorAction SilentlyContinue
    if (-not $java) {
        # Also check JAVA_HOME
        $javaHome = [Environment]::GetEnvironmentVariable("JAVA_HOME", "Machine")
        if (-not $javaHome) {
            $javaHome = [Environment]::GetEnvironmentVariable("JAVA_HOME", "User")
        }
        if ($javaHome) {
            $javaBin = Join-Path $javaHome "bin\java.exe"
            if (Test-Path $javaBin) {
                # Temporarily add to PATH for this session
                $env:PATH = "$env:PATH;$javaHome\bin"
                $java = Get-Command java -ErrorAction SilentlyContinue
            }
        }
    }
    if (-not $java) {
        $msg = "ERROR: JDK 21+ is required but 'java' is not on PATH and JAVA_HOME is not set.`n`nInstall JDK 21+ from https://adoptium.net/ and ensure 'java' is on PATH."
        [Console]::Error.WriteLine($msg)
        Write-Host $msg -ForegroundColor Red
        return $false
    }

    # Verify Java version >= 21
    # java -version writes to stderr; temporarily relax ErrorActionPreference
    # so that stderr output doesn't throw a terminating error.
    $prevEAP = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $versionOutput = & java -version 2>&1 | Out-String
    } finally {
        $ErrorActionPreference = $prevEAP
    }
    if ($versionOutput -and $versionOutput -match 'version "(\d+)\.') {
        $major = [int]$Matches[1]
        if ($major -lt 21) {
            $msg = "ERROR: jdtls requires Java 21+, but found Java $major.`n`nCurrent: $($java.Source)`nUpgrade to JDK 21+ from https://adoptium.net/"
            [Console]::Error.WriteLine($msg)
            Write-Host $msg -ForegroundColor Red
            return $false
        }
        Write-Host "JDK $major found: $($java.Source)" -ForegroundColor Green
    } else {
        Write-Host "JDK found (version unknown): $($java.Source)" -ForegroundColor Yellow
    }
    return $true
}
# -- Helper: download and extract jdtls snapshot ---------------------
function Install-JdtlsFromEclipse {
    Write-Host ""
    Write-Host "Downloading Eclipse JDT Language Server (latest snapshot)..." -ForegroundColor Cyan
    Write-Host "  Source: $JDTLS_DOWNLOAD_URL"
    Write-Host "  Target: $JDTLS_INSTALL_DIR"
    Write-Host "  Note: ~200 MB download — this may take a few minutes..." -ForegroundColor Yellow

    $tempFile = "$env:TEMP\jdtls-latest.tar.gz"

    # Remove stale temp files
    if (Test-Path $tempFile) { Remove-Item $tempFile -Force }

    try {
        # Download with progress
        Write-Host "  Downloading..."
        Invoke-WebRequest -Uri $JDTLS_DOWNLOAD_URL -OutFile $tempFile -UseBasicParsing

        $sizeMB = [math]::Round((Get-Item $tempFile).Length / 1MB, 1)
        Write-Host "  Downloaded: ${sizeMB}MB" -ForegroundColor Green

        # Create install directory
        if (Test-Path $JDTLS_INSTALL_DIR) {
            Write-Host "  Removing previous installation..."
            Remove-Item $JDTLS_INSTALL_DIR -Recurse -Force -ErrorAction SilentlyContinue
        }
        New-Item -ItemType Directory -Path $JDTLS_INSTALL_DIR -Force | Out-Null

        # Extract with tar (built-in on Windows 10+)
        Write-Host "  Extracting..."
        tar -xzf $tempFile -C $JDTLS_INSTALL_DIR

        # Verify extraction
        $launcher = Join-Path $JDTLS_INSTALL_DIR "bin\jdtls.bat"
        if (-not (Test-Path $launcher)) {
            $launcher = Join-Path $JDTLS_INSTALL_DIR "bin\jdtls"
        }
        if (Test-Path $launcher) {
            Write-Host "  Extraction complete: jdtls installed at $JDTLS_INSTALL_DIR" -ForegroundColor Green
        } else {
            throw "Extraction completed but jdtls launcher not found in bin/"
        }
    } catch {
        $msg = "ERROR: Failed to download or extract jdtls: $_"
        [Console]::Error.WriteLine($msg)
        Write-Host $msg -ForegroundColor Red
        Write-Host ""
        Write-Host "You can install manually:" -ForegroundColor Yellow
        Write-Host "  1. Download from https://download.eclipse.org/jdtls/snapshots/"
        Write-Host "  2. Extract to $JDTLS_INSTALL_DIR"
        Write-Host "  3. Re-run this script"
        return $false
    } finally {
        # Clean up temp file
        if (Test-Path $tempFile) { Remove-Item $tempFile -Force -ErrorAction SilentlyContinue }
    }

    return $true
}

# -- Helper: print manual install guidance (fallback) ----------------
function Write-InstallGuidance {
    $msg = @"
jdtls requires a JDK (21+) and manual setup.

Option 1 (recommended): Install via Visual Studio Code
  Install the 'Extension Pack for Java' in VS Code, which bundles jdtls.
  Then restart this script — it will auto-detect the VS Code extension.

Option 2: Download manually
  1. Install JDK 21+: https://adoptium.net/
  2. Download jdtls: https://download.eclipse.org/jdtls/snapshots/
  3. Extract to a permanent location and add to PATH

For detailed instructions, see: https://github.com/eclipse-jdtls/eclipse.jdt.ls
"@
    # Write to both stderr (captured by Gateway) and console (colored)
    [Console]::Error.WriteLine($msg)
    Write-Host $msg -ForegroundColor Yellow
}

# -- Phase 1: Install -------------------------------------------------
function Install-Server {
    Write-Host "[1/3] Installing Eclipse JDT Language Server..."

    # Ensure JDK 21+ is available before attempting to use jdtls.
    # jdtls is a Java application — even with wrapper scripts, it needs
    # java on PATH (or JAVA_HOME set). Do this FIRST, regardless of
    # whether jdtls is already on PATH.
    if (-not (Test-JdkAvailable)) {
        Write-Host ""
        Write-InstallGuidance
        exit 1
    }

    # Check if already on PATH
    $existing = Get-Command $Binary -ErrorAction SilentlyContinue
    if ($existing) {
        Write-Host "jdtls already on PATH at $($existing.Source)" -ForegroundColor Green
        return
    }

    # Not on PATH — search common locations
    $foundDir = Find-JdtlsDir
    if ($foundDir) {
        # Check if this is a VS Code extension (redhat.java)
        if ($foundDir -like "*\.vscode\extensions\redhat.java-*\server\bin") {
            Write-Host "Found jdtls in VS Code extension at $foundDir" -ForegroundColor Yellow
            Write-Host "VS Code's jdtls.bat has 'pause' — creating clean wrapper instead" -ForegroundColor Yellow
            Setup-VsCodeJdtls $foundDir
            Add-ToPath "$JDTLS_INSTALL_DIR\bin"
        } else {
            Write-Host "Found jdtls launcher at $foundDir (not on PATH)" -ForegroundColor Yellow
            Add-ToPath $foundDir
        }
        # Re-check after adding to PATH
        $verify = Get-Command $Binary -ErrorAction SilentlyContinue
        if ($verify) {
            Write-Host "jdtls now available on PATH" -ForegroundColor Green
            return
        }
    }

    # Not found anywhere — try automated download from Eclipse
    Write-Host "jdtls launcher not found." -ForegroundColor Yellow

    # Check JDK prerequisite first
    if (-not (Test-JdkAvailable)) {
        # JDK missing — can't auto-install, show manual guidance
        Write-Host ""
        Write-InstallGuidance
        return
    }

    # Download and extract jdtls
    if (Install-JdtlsFromEclipse) {
        Add-ToPath "$JDTLS_INSTALL_DIR\bin"
        $verify = Get-Command $Binary -ErrorAction SilentlyContinue
        if ($verify) {
            Write-Host "jdtls installed and available on PATH" -ForegroundColor Green
            return
        }
    }

    # If auto-install failed, show manual guidance
    Write-Host ""
    Write-InstallGuidance
}

# -- Phase 2: Verify --------------------------------------------------
function Verify-Server {
    Write-Host "[2/3] Verifying jdtls is on PATH..."

    $cmd = Get-Command $Binary -ErrorAction SilentlyContinue
    if ($cmd) {
        Write-Host "OK: jdtls found at $($cmd.Source)" -ForegroundColor Green
        return
    }

    # Still not on PATH — one last search and auto-add attempt
    $foundDir = Find-JdtlsDir
    if ($foundDir) {
        if ($foundDir -like "*\.vscode\extensions\redhat.java-*\server\bin") {
            Write-Host "Found jdtls in VS Code extension — creating clean wrapper" -ForegroundColor Yellow
            Setup-VsCodeJdtls $foundDir
            Add-ToPath "$JDTLS_INSTALL_DIR\bin"
        } else {
            Write-Host "Found jdtls launcher at $foundDir — adding to PATH" -ForegroundColor Yellow
            Add-ToPath $foundDir
        }
        $retry = Get-Command $Binary -ErrorAction SilentlyContinue
        if ($retry) {
            Write-Host "OK: jdtls now available on PATH" -ForegroundColor Green
            return
        }
    }

    Write-Host ""
    $errMsg = "ERROR: jdtls not found on PATH and not found in common locations."
    [Console]::Error.WriteLine($errMsg)
    [Console]::Error.WriteLine("")
    Write-Host $errMsg -ForegroundColor Red
    Write-Host ""
    Write-InstallGuidance
    exit 1
}

# -- Phase 3: Health Check --------------------------------------------
function Health-Check {
    Write-Host "[3/3] Health check: testing stdio handshake..."

    # Resolve the actual command name (may be jdtls.bat or jdtls.cmd on Windows)
    $cmdName = $Binary
    $found = Get-Command $Binary -ErrorAction SilentlyContinue
    if ($found) {
        $cmdName = $found.Name
    }

    $initMsg = '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{},"rootUri":"file:///C:/tmp"}}'
    $header = "Content-Length: $($initMsg.Length)`r`n`r`n"
    $input = $header + $initMsg

    try {
        # Write stdin input BEFORE starting the process to avoid a race
        Set-Content -Path "$env:TEMP\lsp_init.txt" -Value $input -NoNewline
        $proc = Start-Process -FilePath $cmdName -NoNewWindow `
            -RedirectStandardInput "$env:TEMP\lsp_init.txt" `
            -RedirectStandardOutput "$env:TEMP\lsp_out.txt" `
            -RedirectStandardError "$env:TEMP\lsp_err.txt" `
            -PassThru
        $proc | Wait-Process -Timeout 15 -ErrorAction SilentlyContinue
        if (!$proc.HasExited) { $proc | Stop-Process -Force }

        $output = Get-Content "$env:TEMP\lsp_out.txt" -Raw -ErrorAction SilentlyContinue
        if ($output -and $output.Contains("Content-Length")) {
            Write-Host "OK: jdtls responds to LSP initialize (stdio mode)" -ForegroundColor Green
        } else {
            Write-Host "WARN: jdtls did not respond to handshake" -ForegroundColor Yellow
            Write-Host "  This may be caused by missing JDK or incorrect JAVA_HOME." -ForegroundColor Yellow
        }
    } catch {
        Write-Host "WARN: Health check failed: $_" -ForegroundColor Yellow
        Write-Host "  Ensure JDK 17+ is installed and JAVA_HOME is set." -ForegroundColor Yellow
    }
}

# -- Main --------------------------------------------------------------
Write-Host "=== RollBall LSP Setup: jdtls (Java) ==="
Install-Server
Verify-Server
Health-Check
Write-Host "=== Done ==="
