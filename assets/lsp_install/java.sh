#!/usr/bin/env bash
# ACowork LSP install script: jdtls (Java)
# Phases: Install → Verify → Health Check
#
# jdtls is the Eclipse JDT Language Server, a Java application (not a native
# binary). This script automates download from Eclipse snapshots and installs
# to ~/.local/jdtls (Linux/macOS) or %LOCALAPPDATA%\jdtls (Windows via Git Bash).
#
# Prerequisites: JDK 21+ (java on PATH or JAVA_HOME set)

set -euo pipefail

# Cross-platform timeout helper (macOS doesn't ship `timeout`).
_timeout() {
    local seconds="$1"; shift
    if command -v gtimeout &>/dev/null; then
        gtimeout "$seconds" "$@"
    elif command -v timeout &>/dev/null; then
        timeout "$seconds" "$@"
    else
        perl -e 'alarm shift; exec @ARGV' -- "$seconds" "$@"
    fi
}

BINARY="jdtls"

# JDT LS snapshot download URL (always the latest build)
JDTLS_DOWNLOAD_URL="https://download.eclipse.org/jdtls/snapshots/jdt-language-server-latest.tar.gz"
# Install directory — prefer XDG if set, otherwise ~/.local/jdtls
if [[ -n "${XDG_DATA_HOME:-}" ]]; then
    JDTLS_INSTALL_DIR="${XDG_DATA_HOME}/jdtls"
elif [[ "$(uname -s)" == "Darwin" ]]; then
    JDTLS_INSTALL_DIR="$HOME/Library/Application Support/jdtls"
else
    JDTLS_INSTALL_DIR="$HOME/.local/jdtls"
fi

# ── Helper: persist a directory on PATH ──────────────────────────────
add_to_path() {
    local dir="$1"
    case ":$PATH:" in
        *:"$dir":*) ;;
        *) export PATH="$PATH:$dir" ;;
    esac
    # Persist to shell profile — detect current shell and choose the
    # appropriate profile file. zsh users get .zshrc (or .zprofile),
    # bash users get .bashrc (or .bash_profile), others fall back to
    # the traditional .profile.
    local profile_file=""
    case "${SHELL:-}" in
        */zsh)
            for f in "$HOME/.zshrc" "$HOME/.zprofile" "$HOME/.profile"; do
                if [[ -f "$f" ]]; then
                    profile_file="$f"
                    break
                fi
            done
            # If no zsh profile exists, create .zshrc
            [[ -z "$profile_file" ]] && profile_file="$HOME/.zshrc"
            ;;
        */bash)
            for f in "$HOME/.bashrc" "$HOME/.bash_profile" "$HOME/.profile"; do
                if [[ -f "$f" ]]; then
                    profile_file="$f"
                    break
                fi
            done
            [[ -z "$profile_file" ]] && profile_file="$HOME/.bashrc"
            ;;
        *)
            for f in "$HOME/.bashrc" "$HOME/.zshrc" "$HOME/.profile"; do
                if [[ -f "$f" ]]; then
                    profile_file="$f"
                    break
                fi
            done
            [[ -z "$profile_file" ]] && profile_file="$HOME/.profile"
            ;;
    esac
    if [[ -n "${profile_file:-}" ]] && ! grep -qF "$dir" "$profile_file" 2>/dev/null; then
        echo "export PATH=\"\$PATH:$dir\"" >> "$profile_file"
    fi
}

# ── Helper: search for jdtls launcher in common locations ────────────
find_jdtls_dir() {
    local dir

    # 1. VS Code Java extension (redhat.java)
    for ext_dir in "$HOME/.vscode/extensions/redhat.java-"*; do
        [[ -d "$ext_dir/server/bin" ]] || continue
        for name in jdtls jdtls.py; do
            if [[ -x "$ext_dir/server/bin/$name" ]]; then
                echo "$ext_dir/server/bin"
                return 0
            fi
        done
    done

    # 2. VS Code (Insiders) Java extension
    for ext_dir in "$HOME/.vscode-insiders/extensions/redhat.java-"*; do
        [[ -d "$ext_dir/server/bin" ]] || continue
        for name in jdtls jdtls.py; do
            if [[ -x "$ext_dir/server/bin/$name" ]]; then
                echo "$ext_dir/server/bin"
                return 0
            fi
        done
    done

    # 3. Common manual install locations
    for dir in \
        /usr/local/jdtls/bin \
        /opt/jdtls/bin \
        "$HOME/jdtls/bin" \
        "$HOME/.jdtls/bin" \
        "$HOME/.local/jdtls/bin" \
        "$HOME/.local/share/jdtls/bin" \
        "$HOME/Library/Application Support/jdtls/bin"; do
        [[ -d "$dir" ]] || continue
        for name in jdtls jdtls.py; do
            if [[ -x "$dir/$name" ]]; then
                echo "$dir"
                return 0
            fi
        done
    done

    return 1
}

# ── Helper: check if JDK 21+ is available ────────────────────────────
test_jdk_available() {
    local java_cmd=""
    if command -v java &>/dev/null; then
        java_cmd="$(command -v java)"
    elif [[ -n "${JAVA_HOME:-}" && -x "$JAVA_HOME/bin/java" ]]; then
        export PATH="$JAVA_HOME/bin:$PATH"
        java_cmd="$JAVA_HOME/bin/java"
    fi

    if [[ -z "$java_cmd" ]]; then
        echo "ERROR: JDK 21+ is required but 'java' is not on PATH and JAVA_HOME is not set." >&2
        echo "" >&2
        echo "Install JDK 21+ from https://adoptium.net/ and ensure 'java' is on PATH." >&2
        return 1
    fi

    # Verify Java version >= 21
    local version_output
    version_output=$(java -version 2>&1 || true)
    local major
    major=$(echo "$version_output" | awk -F'"' '/version/ {print $2}' | cut -d'.' -f1)
    if [[ -n "$major" && "$major" -lt 21 ]]; then
        echo "ERROR: jdtls requires Java 21+, but found Java $major." >&2
        echo "" >&2
        echo "Current: $java_cmd" >&2
        echo "Upgrade to JDK 21+ from https://adoptium.net/" >&2
        return 1
    fi
    echo "JDK $major found: $java_cmd"
    return 0
}

# ── Helper: download and extract jdtls snapshot ──────────────────────
install_jdtls_from_eclipse() {
    echo ""
    echo "Downloading Eclipse JDT Language Server (latest snapshot)..."
    echo "  Source: $JDTLS_DOWNLOAD_URL"
    echo "  Target: $JDTLS_INSTALL_DIR"
    echo "  Note: ~200 MB download — this may take a few minutes..."

    local temp_file
    temp_file="$(mktemp)"
    # Use .tar.gz extension for clarity
    mv "$temp_file" "${temp_file}.tar.gz"
    temp_file="${temp_file}.tar.gz"

    # Download
    echo "  Downloading..."
    if command -v curl &>/dev/null; then
        if ! curl -fSL --progress-bar "$JDTLS_DOWNLOAD_URL" -o "$temp_file"; then
            echo "ERROR: Failed to download jdtls from $JDTLS_DOWNLOAD_URL" >&2
            rm -f "$temp_file"
            return 1
        fi
    elif command -v wget &>/dev/null; then
        if ! wget -q --show-progress "$JDTLS_DOWNLOAD_URL" -O "$temp_file"; then
            echo "ERROR: Failed to download jdtls from $JDTLS_DOWNLOAD_URL" >&2
            rm -f "$temp_file"
            return 1
        fi
    else
        echo "ERROR: Neither curl nor wget found. Please install one of them." >&2
        rm -f "$temp_file"
        return 1
    fi

    local size_mb
    size_mb=$(du -m "$temp_file" 2>/dev/null | cut -f1 || echo "?")
    echo "  Downloaded: ${size_mb}MB"

    # Create install directory
    if [[ -d "$JDTLS_INSTALL_DIR" ]]; then
        echo "  Removing previous installation..."
        rm -rf "$JDTLS_INSTALL_DIR"
    fi
    mkdir -p "$JDTLS_INSTALL_DIR"

    # Extract
    echo "  Extracting..."
    if ! tar -xzf "$temp_file" -C "$JDTLS_INSTALL_DIR"; then
        echo "ERROR: Failed to extract jdtls archive" >&2
        rm -rf "$JDTLS_INSTALL_DIR"
        rm -f "$temp_file"
        return 1
    fi

    rm -f "$temp_file"

    # Verify extraction
    if [[ -x "$JDTLS_INSTALL_DIR/bin/jdtls" ]]; then
        echo "  Extraction complete: jdtls installed at $JDTLS_INSTALL_DIR"
        return 0
    else
        echo "ERROR: Extraction completed but jdtls launcher not found in bin/" >&2
        return 1
    fi
}

# ── Helper: print manual install guidance (fallback) ──────────────
print_guidance() {
    echo "jdtls requires a JDK (21+) and manual setup."
    echo ""
    echo "Option 1 (recommended): Install via Visual Studio Code"
    echo "  Install the 'Extension Pack for Java' in VS Code, which bundles jdtls."
    echo "  Then restart this script — it will auto-detect the VS Code extension."
    echo ""
    echo "Option 2: Download manually"
    echo "  1. Install JDK 21+ from https://adoptium.net/"
    echo "  2. Download jdtls: https://download.eclipse.org/jdtls/snapshots/"
    echo "  3. Extract to a permanent location and add to PATH"
    echo ""
    echo "For detailed instructions, see: https://github.com/eclipse-jdtls/eclipse.jdt.ls"
}

# ── Phase 1: Install ──────────────────────────────────────────────────
install() {
    echo "[1/3] Installing Eclipse JDT Language Server..."

    # Ensure JDK 21+ is available before attempting to use jdtls.
    # jdtls is a Java application — even with wrapper scripts, it needs
    # java on PATH (or JAVA_HOME set). Do this FIRST, regardless of
    # whether jdtls is already on PATH.
    if ! test_jdk_available; then
        echo ""
        print_guidance
        exit 1
    fi

    # Check if already on PATH
    if command -v "$BINARY" &>/dev/null; then
        local path
        path=$(command -v "$BINARY")
        echo "jdtls already on PATH at $path"
        return 0
    fi

    # Not on PATH — search common locations
    local found_dir
    if found_dir=$(find_jdtls_dir); then
        echo "Found jdtls launcher at $found_dir (not on PATH)"
        add_to_path "$found_dir"
        if command -v "$BINARY" &>/dev/null; then
            echo "jdtls now available on PATH"
            return 0
        fi
    fi

    # Not found anywhere — try automated download from Eclipse
    echo "jdtls launcher not found."

    # Download and extract jdtls
    if install_jdtls_from_eclipse; then
        add_to_path "$JDTLS_INSTALL_DIR/bin"
        if command -v "$BINARY" &>/dev/null; then
            echo "jdtls installed and available on PATH"
            return 0
        fi
    fi

    # If auto-install failed, show manual guidance
    echo ""
    print_guidance
}

# ── Phase 2: Verify ──────────────────────────────────────────────────
verify() {
    echo "[2/3] Verifying jdtls is on PATH..."

    if command -v "$BINARY" &>/dev/null; then
        local path
        path=$(command -v "$BINARY")
        echo "OK: jdtls found at $path"
        return 0
    fi

    # Still not on PATH — one last search and auto-add attempt
    local found_dir
    if found_dir=$(find_jdtls_dir); then
        echo "Found jdtls launcher at $found_dir — adding to PATH"
        add_to_path "$found_dir"
        if command -v "$BINARY" &>/dev/null; then
            echo "OK: jdtls now available on PATH"
            return 0
        fi
    fi

    echo ""
    echo "ERROR: jdtls not found on PATH and not found in common locations."
    echo ""
    print_guidance
    exit 1
}

# ── Phase 3: Health Check ────────────────────────────────────────────
health_check() {
    echo "[3/3] Health check: testing stdio handshake..."
    local init_msg
    init_msg='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{},"rootUri":"file:///tmp"}}'
    local header
    header="Content-Length: ${#init_msg}\r\n\r\n"

    local response
    response=$(printf "${header}${init_msg}" | _timeout 15 "$BINARY" 2>/dev/null | head -c 4096 || true)

    if [[ -n "$response" && "$response" == *"Content-Length"* ]]; then
        echo "OK: jdtls responds to LSP initialize (stdio mode)"
    else
        echo "WARN: jdtls did not respond to handshake"
        echo "  This may be caused by missing JDK or incorrect JAVA_HOME."
    fi
}

# ── Main ──────────────────────────────────────────────────────────────
echo "=== ACowork LSP Setup: jdtls (Java) ==="
install
verify
health_check
echo "=== Done ==="
