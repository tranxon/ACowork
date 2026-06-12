#!/usr/bin/env bash
# RollBall LSP install script: pylsp (Python)
# Phases: Install → Verify → Health Check

set -euo pipefail

BINARY="pylsp"

# ── Phase 1: Install ──────────────────────────────────────────────────
install() {
    echo "[1/3] Installing python-lsp-server..."
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
        local path
        path=$(command -v "$BINARY")
        echo "OK: pylsp found at $path"
    else
        echo "ERROR: pylsp not found on PATH after install"
        exit 1
    fi
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