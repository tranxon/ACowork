#!/usr/bin/env bash
# ACowork LSP install script: kotlin-language-server
# Phases: Install → Verify → Health Check
#
# kotlin-language-server is a native binary (Kotlin/Native). It does NOT
# require a JDK at runtime, unlike jdtls. Installation is straightforward:
# brew on macOS, sdkman on Linux, or manual download from GitHub releases.
#
# Prerequisites: none (standalone binary)

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

BINARY="kotlin-language-server"

# ── Helpers ────────────────────────────────────────────────────────

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

# Search VS Code extension for bundled kotlin-language-server binary.
find_vscode_kotlin_ls() {
    for ext_dir in "$HOME/.vscode/extensions/fwcd.kotlin-"*; do
        local candidate="$ext_dir/server/bin/kotlin-language-server"
        if [[ -x "$candidate" ]]; then
            echo "$candidate"
            return 0
        fi
    done
    for ext_dir in "$HOME/.vscode-insiders/extensions/fwcd.kotlin-"*; do
        local candidate="$ext_dir/server/bin/kotlin-language-server"
        if [[ -x "$candidate" ]]; then
            echo "$candidate"
            return 0
        fi
    done
    return 1
}

# Search common install locations.
find_kotlin_ls() {
    for d in \
        "$HOME/.local/bin" \
        "/usr/local/bin" \
        "/opt/homebrew/bin" \
        "/usr/bin" \
        "$HOME/.sdkman/candidates/kotlin-language-server/current/bin"; do
        local candidate="$d/$BINARY"
        if [[ -x "$candidate" ]]; then
            echo "$candidate"
            return 0
        fi
    done
    return 1
}

# ── Phase 1: Install ──────────────────────────────────────────────────
install() {
    echo "[1/3] Installing kotlin-language-server..."

    # Already on PATH?
    if command -v "$BINARY" &>/dev/null; then
        echo "kotlin-language-server already on PATH at $(command -v "$BINARY")"
        return 0
    fi

    # Check VS Code extension (fwcd.kotlin bundles kotlin-language-server)
    local vscode_bin
    if vscode_bin=$(find_vscode_kotlin_ls); then
        local vscode_dir
        vscode_dir=$(dirname "$vscode_bin")
        echo "Found kotlin-language-server from VS Code extension at $vscode_bin — adding to PATH..."
        add_to_path "$vscode_dir"
        if command -v "$BINARY" &>/dev/null; then
            return 0
        fi
    fi

    # Not on PATH — search common locations
    local found
    found=$(find_kotlin_ls) || true
    if [[ -n "$found" ]]; then
        local dir
        dir=$(dirname "$found")
        echo "Found kotlin-language-server at $found — adding $dir to PATH..."
        add_to_path "$dir"
        return 0
    fi

    # Not installed — try package managers
    if command -v brew &>/dev/null && [[ "$(uname -s)" == "Darwin" ]]; then
        echo "Trying: brew install kotlin-language-server..."
        if brew install kotlin-language-server 2>/dev/null; then
            echo "Installed via Homebrew."
            return 0
        fi
        echo "Homebrew install failed, trying next strategy..."
    fi

    if command -v sdk &>/dev/null; then
        echo "Trying: sdk install kotlin-language-server..."
        if sdk install kotlin-language-server 2>/dev/null; then
            echo "Installed via SDKMAN."
            return 0
        fi
        echo "SDKMAN install failed, trying next strategy..."
    fi

    # Fallback: manual guidance
    echo ""
    echo "ERROR: Could not install kotlin-language-server automatically."
    echo ""
    echo "Manual options:"
    echo "  1. brew install kotlin-language-server          (macOS)"
    echo "  2. sdk install kotlin-language-server           (Linux/macOS via SDKMAN)"
    echo "  3. Install VS Code 'Kotlin Language' extension  (bundles the server)"
    echo "  4. Download from GitHub: https://github.com/fwcd/kotlin-language-server/releases"
    exit 1
}

# ── Phase 2: Verify ──────────────────────────────────────────────────
verify() {
    echo "[2/3] Verifying kotlin-language-server is on PATH..."

    if command -v "$BINARY" &>/dev/null; then
        local path
        path=$(command -v "$BINARY")
        echo "OK: kotlin-language-server found at $path"
        return 0
    fi

    # Check VS Code extension
    local vscode_bin
    if vscode_bin=$(find_vscode_kotlin_ls); then
        local vscode_dir
        vscode_dir=$(dirname "$vscode_bin")
        echo "Found kotlin-language-server from VS Code extension at $vscode_bin — adding to PATH..."
        add_to_path "$vscode_dir"
        if command -v "$BINARY" &>/dev/null; then
            echo "OK: kotlin-language-server found at $(command -v "$BINARY")"
            return 0
        fi
    fi

    # Search common locations
    local found
    found=$(find_kotlin_ls) || true
    if [[ -n "$found" ]]; then
        local dir
        dir=$(dirname "$found")
        echo "Found kotlin-language-server at $found — adding $dir to PATH..."
        add_to_path "$dir"
        if command -v "$BINARY" &>/dev/null; then
            echo "OK: kotlin-language-server found at $(command -v "$BINARY")"
            return 0
        fi
    fi

    echo ""
    echo "ERROR: kotlin-language-server not found on PATH after install."
    echo "Install manually: brew install kotlin-language-server"
    exit 1
}

# ── Phase 3: Health Check ────────────────────────────────────────────
# kotlin-language-server defaults to stdio mode; no --stdio flag needed.
health_check() {
    echo "[3/3] Health check: testing stdio handshake..."
    local init_msg
    init_msg='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{},"rootUri":"file:///tmp"}}'
    local header
    header="Content-Length: ${#init_msg}\r\n\r\n"

    local response
    response=$(printf "${header}${init_msg}" | _timeout 10 "$BINARY" 2>/dev/null | head -c 4096 || true)

    if [[ -n "$response" && "$response" == *"Content-Length"* ]]; then
        echo "OK: kotlin-language-server responds to LSP initialize (stdio mode)"
    else
        echo "WARN: kotlin-language-server did not respond to handshake"
    fi
}

# ── Main ──────────────────────────────────────────────────────────────
echo "=== ACowork LSP Setup: kotlin-language-server ==="
install
verify
health_check
echo "=== Done ==="
