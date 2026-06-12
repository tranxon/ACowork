#!/usr/bin/env bash
# RollBall LSP install script: pylsp (Python)
# Phases: Install → Verify → Health Check

set -euo pipefail

BINARY="pylsp"

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

find_pylsp() {
    for d in "$HOME/.local/bin" "/usr/local/bin"; do
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
    echo "[1/3] Installing python-lsp-server..."

    # Already on PATH?
    if command -v "$BINARY" &>/dev/null; then
        echo "pylsp already on PATH at $(command -v "$BINARY")"
        return 0
    fi

    # Not on PATH — search common locations
    local found
    found=$(find_pylsp) || true
    if [[ -n "$found" ]]; then
        local dir
        dir=$(dirname "$found")
        echo "Found pylsp at $found — adding $dir to PATH..."
        add_to_path "$dir"
        return 0
    fi

    # Not installed
    if command -v pip &>/dev/null; then
        pip install python-lsp-server
    elif command -v pip3 &>/dev/null; then
        pip3 install python-lsp-server
    else
        echo "ERROR: pip not found. Install Python first: https://python.org"
        exit 1
    fi
}

# ── Phase 2: Verify ──────────────────────────────────────────────────
verify() {
    echo "[2/3] Verifying pylsp is on PATH..."
    if command -v "$BINARY" &>/dev/null; then
        echo "OK: pylsp found at $(command -v "$BINARY")"
        return 0
    fi

    # Search common locations
    local found
    found=$(find_pylsp) || true
    if [[ -n "$found" ]]; then
        local dir
        dir=$(dirname "$found")
        echo "Found pylsp at $found — adding $dir to PATH..."
        add_to_path "$dir"
        if command -v "$BINARY" &>/dev/null; then
            echo "OK: pylsp found at $(command -v "$BINARY")"
            return 0
        fi
    fi

    echo "ERROR: pylsp not found on PATH after install"
    exit 1
}

# ── Phase 3: Health Check ────────────────────────────────────────────
# pylsp requires --stdio flag for LSP communication.
health_check() {
    echo "[3/3] Health check: testing stdio handshake..."
    local init_msg
    init_msg='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{},"rootUri":"file:///tmp"}}'
    local header
    header="Content-Length: ${#init_msg}\r\n\r\n"

    local response
    response=$(printf "${header}${init_msg}" | timeout 10 "$BINARY" --stdio 2>/dev/null | head -c 4096 || true)

    if [[ -n "$response" && "$response" == *"Content-Length"* ]]; then
        echo "OK: pylsp responds to LSP initialize (--stdio mode)"
    else
        echo "WARN: pylsp did not respond to handshake"
    fi
}

# ── Main ──────────────────────────────────────────────────────────────
echo "=== RollBall LSP Setup: pylsp (Python) ==="
install
verify
health_check
echo "=== Done ==="