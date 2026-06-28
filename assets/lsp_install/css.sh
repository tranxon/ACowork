#!/usr/bin/env bash
# ACowork LSP install script: vscode-css-language-server
# Phases: Install -> Verify -> Health Check
#
# vscode-css-languageserver npm package was DEPRECATED.
# The correct package is vscode-langservers-extracted, which bundles
# CSS, HTML, JSON, and ESLint servers all at once.
# When installed, it creates:
#   vscode-css-language-server
#   vscode-html-language-server
#   vscode-json-language-server
# Note: executable names use "language-server" (with dash),
#   not "languageserver" (old format).

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

BINARY="vscode-css-language-server"
LEGACY_BINARY="vscode-css-languageserver"
NPM_PACKAGE="vscode-langservers-extracted"

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
                [[ -f "$f" ]] && { profile_file="$f"; break; }
            done
            # If no zsh profile exists, create .zshrc
            [[ -z "$profile_file" ]] && profile_file="$HOME/.zshrc"
            ;;
        */bash)
            for f in "$HOME/.bashrc" "$HOME/.bash_profile" "$HOME/.profile"; do
                [[ -f "$f" ]] && { profile_file="$f"; break; }
            done
            [[ -z "$profile_file" ]] && profile_file="$HOME/.bashrc"
            ;;
        *)
            for f in "$HOME/.bashrc" "$HOME/.zshrc" "$HOME/.profile"; do
                [[ -f "$f" ]] && { profile_file="$f"; break; }
            done
            [[ -z "$profile_file" ]] && profile_file="$HOME/.profile"
            ;;
    esac
    if [[ -n "${profile_file:-}" ]] && ! grep -q "$dir" "$profile_file" 2>/dev/null; then
        echo "export PATH=\"\$PATH:$dir\"" >> "$profile_file"
        echo "Added $dir to $profile_file for persistence."
    fi
}

find_npm_binary() {
    local name="$1"
    for d in "$HOME/.npm-global/bin" "/usr/local/bin" "/usr/bin"; do
        local candidate="$d/$name"
        if [[ -x "$candidate" ]]; then
            echo "$candidate"
            return 0
        fi
    done
    return 1
}

# ── Phase 1: Install ──────────────────────────────────────────────
install() {
    echo "[1/3] Installing $BINARY (from $NPM_PACKAGE)..."

    # Already on PATH? (new or legacy name)
    if command -v "$BINARY" &>/dev/null; then
        echo "$BINARY already on PATH at $(command -v "$BINARY")"
        return 0
    fi
    if command -v "$LEGACY_BINARY" &>/dev/null; then
        echo "Legacy $LEGACY_BINARY already on PATH — no need to reinstall"
        return 0
    fi

    # Not on PATH — search npm global bin (new + legacy name)
    local found
    found=$(find_npm_binary "$BINARY") || true
    if [[ -n "$found" ]]; then
        local dir
        dir=$(dirname "$found")
        echo "Found $BINARY at $found — adding $dir to PATH..."
        add_to_path "$dir"
        return 0
    fi
    found=$(find_npm_binary "$LEGACY_BINARY") || true
    if [[ -n "$found" ]]; then
        local dir
        dir=$(dirname "$found")
        echo "Found legacy $LEGACY_BINARY at $found — adding $dir to PATH..."
        add_to_path "$dir"
        return 0
    fi

    # Not installed — install vscode-langservers-extracted
    # This single package provides CSS, HTML, JSON, and ESLint servers.
    if command -v npm &>/dev/null; then
        echo "Installing $NPM_PACKAGE (bundles CSS + HTML + JSON servers)..."
        npm install -g "$NPM_PACKAGE"
    else
        echo "ERROR: npm not found. Install Node.js first: https://nodejs.org"
        exit 1
    fi
}

# ── Phase 2: Verify ───────────────────────────────────────────────
verify() {
    echo "[2/3] Verifying $BINARY is on PATH..."

    if command -v "$BINARY" &>/dev/null; then
        echo "OK: $BINARY found at $(command -v "$BINARY")"
        return 0
    fi
    # Also check legacy name
    if command -v "$LEGACY_BINARY" &>/dev/null; then
        echo "OK: Legacy $LEGACY_BINARY found at $(command -v "$LEGACY_BINARY")"
        return 0
    fi

    # Search npm global bin
    local found
    found=$(find_npm_binary "$BINARY") || true
    if [[ -n "$found" ]]; then
        local dir
        dir=$(dirname "$found")
        echo "Found $BINARY at $found — adding $dir to PATH..."
        add_to_path "$dir"
        if command -v "$BINARY" &>/dev/null; then
            echo "OK: $BINARY found at $(command -v "$BINARY")"
            return 0
        fi
    fi
    found=$(find_npm_binary "$LEGACY_BINARY") || true
    if [[ -n "$found" ]]; then
        local dir
        dir=$(dirname "$found")
        echo "Found legacy $LEGACY_BINARY at $found — adding $dir to PATH..."
        add_to_path "$dir"
        if command -v "$BINARY" &>/dev/null; then
            echo "OK: $BINARY found at $(command -v "$BINARY")"
            return 0
        fi
    fi

    echo "ERROR: $BINARY not found on PATH after install"
    exit 1
}

# ── Phase 3: Health Check ──────────────────────────────────────────
# vscode-css-language-server requires --stdio flag.
health_check() {
    echo "[3/3] Health check: testing stdio handshake..."
    local init_msg
    init_msg='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{},"rootUri":"file:///tmp"}}'
    local header
    header="Content-Length: ${#init_msg}\r\n\r\n"

    local cmd="$BINARY"
    if ! command -v "$cmd" &>/dev/null; then
        cmd="$LEGACY_BINARY"
    fi

    local response
    response=$(printf "${header}${init_msg}" | _timeout 10 "$cmd" --stdio 2>/dev/null | head -c 4096 || true)

    if [[ -n "$response" && "$response" == *"Content-Length"* ]]; then
        echo "OK: $cmd responds to LSP initialize (--stdio mode)"
    else
        echo "WARN: $cmd did not respond to handshake"
    fi
}

# ── Main ────────────────────────────────────────────────────────────
echo "=== ACowork LSP Setup: vscode-css-language-server ==="
install
verify
health_check
echo "=== Done ==="
