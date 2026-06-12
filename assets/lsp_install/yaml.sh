#!/usr/bin/env bash
# RollBall LSP install script: yaml-language-server
# Phases: Install → Verify → Health Check

set -euo pipefail

BINARY="yaml-language-server"

# ── Helpers ────────────────────────────────────────────────────────

add_to_path() {
    local dir="$1"
    case ":$PATH:" in
        *:"$dir":*) ;;
        *) export PATH="$PATH:$dir" ;;
    esac
    local profile_file
    for f in "$HOME/.bashrc" "$HOME/.zshrc" "$HOME/.profile"; do
        [[ -f "$f" ]] && { profile_file="$f"; break; }
    done
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

# ── Phase 1: Install ──────────────────────────────────────────────────
install() {
    echo "[1/3] Installing yaml-language-server..."

    # Already on PATH?
    if command -v "$BINARY" &>/dev/null; then
        echo "yaml-language-server already on PATH at $(command -v "$BINARY")"
        return 0
    fi

    # Not on PATH — search npm global bin
    local found
    found=$(find_npm_binary "$BINARY") || true
    if [[ -n "$found" ]]; then
        local dir
        dir=$(dirname "$found")
        echo "Found yaml-language-server at $found — adding $dir to PATH..."
        add_to_path "$dir"
        return 0
    fi

    # Not installed
    if command -v npm &>/dev/null; then
        npm install -g yaml-language-server
    else
        echo "ERROR: npm not found. Install Node.js first: https://nodejs.org"
        exit 1
    fi
}

# ── Phase 2: Verify ──────────────────────────────────────────────────
verify() {
    echo "[2/3] Verifying yaml-language-server is on PATH..."
    if command -v "$BINARY" &>/dev/null; then
        echo "OK: yaml-language-server found at $(command -v "$BINARY")"
        return 0
    fi

    # Search npm global bin
    local found
    found=$(find_npm_binary "$BINARY") || true
    if [[ -n "$found" ]]; then
        local dir
        dir=$(dirname "$found")
        echo "Found yaml-language-server at $found — adding $dir to PATH..."
        add_to_path "$dir"
        if command -v "$BINARY" &>/dev/null; then
            echo "OK: yaml-language-server found at $(command -v "$BINARY")"
            return 0
        fi
    fi

    echo "ERROR: yaml-language-server not found on PATH after install"
    exit 1
}

# ── Phase 3: Health Check ────────────────────────────────────────────
# yaml-language-server requires --stdio flag.
health_check() {
    echo "[3/3] Health check: testing stdio handshake..."
    local init_msg
    init_msg='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{},"rootUri":"file:///tmp"}}'
    local header
    header="Content-Length: ${#init_msg}\r\n\r\n"

    local response
    response=$(printf "${header}${init_msg}" | timeout 10 "$BINARY" --stdio 2>/dev/null | head -c 4096 || true)

    if [[ -n "$response" && "$response" == *"Content-Length"* ]]; then
        echo "OK: yaml-language-server responds to LSP initialize (--stdio mode)"
    else
        echo "WARN: yaml-language-server did not respond to handshake"
    fi
}

# ── Main ──────────────────────────────────────────────────────────────
echo "=== RollBall LSP Setup: yaml-language-server ==="
install
verify
health_check
echo "=== Done ==="
