#!/usr/bin/env bash
# RollBall LSP install script: typescript-language-server
# Phases: Install → Verify → Health Check

set -euo pipefail

BINARY="typescript-language-server"

# ── Phase 1: Install ──────────────────────────────────────────────────
install() {
    echo "[1/3] Installing typescript-language-server..."
    if command -v npm &>/dev/null; then
        npm install -g typescript-language-server typescript
    else {
        echo "ERROR: npm not found. Install Node.js first: https://nodejs.org"
        exit 1
    fi
}

# ── Phase 2: Verify ──────────────────────────────────────────────────
verify() {
    echo "[2/3] Verifying typescript-language-server is on PATH..."
    if command -v "$BINARY" &>/dev/null; then
        local path
        path=$(command -v "$BINARY")
        echo "OK: typescript-language-server found at $path"
    else
        echo "ERROR: typescript-language-server not found on PATH after install"
        exit 1
    fi
}

# ── Phase 3: Health Check ────────────────────────────────────────────
# typescript-language-server requires --stdio flag.
health_check() {
    echo "[3/3] Health check: testing stdio handshake..."
    local init_msg
    init_msg='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{},"rootUri":"file:///tmp"}}'
    local header
    header="Content-Length: ${#init_msg}\r\n\r\n"

    local response
    response=$(printf "${header}${init_msg}" | timeout 10 "$BINARY" --stdio 2>/dev/null | head -c 4096 || true)

    if [[ -n "$response" && "$response" == *"Content-Length"* ]]; then
        echo "OK: typescript-language-server responds to LSP initialize (--stdio mode)"
    else
        echo "WARN: typescript-language-server did not respond to handshake"
    fi
}

# ── Main ──────────────────────────────────────────────────────────────
echo "=== RollBall LSP Setup: typescript-language-server ==="
install
verify
health_check
echo "=== Done ==="