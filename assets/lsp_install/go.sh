#!/usr/bin/env bash
# RollBall LSP install script: gopls (Go)
# Phases: Install → Verify → Health Check

set -euo pipefail

BINARY="gopls"

# ── Phase 1: Install ──────────────────────────────────────────────────
install() {
    echo "[1/3] Installing gopls..."
    if command -v go &>/dev/null; then
        go install golang.org/x/tools/gopls@latest
    else
        echo "ERROR: go not found. Install Go first: https://go.dev/dl/"
        exit 1
    fi
}

# ── Phase 2: Verify ──────────────────────────────────────────────────
verify() {
    echo "[2/3] Verifying gopls is on PATH..."
    if command -v "$BINARY" &>/dev/null; then
        local path
        path=$(command -v "$BINARY")
        echo "OK: gopls found at $path"
    else
        echo "ERROR: gopls not found on PATH after install (GOPATH/bin may not be on PATH)"
        echo "Try: export PATH=\$PATH:\$(go env GOPATH)/bin"
        exit 1
    fi
}

# ── Phase 3: Health Check ────────────────────────────────────────────
# gopls uses 'serve' subcommand (not --stdio flag).
health_check() {
    echo "[3/3] Health check: testing stdio handshake..."
    local init_msg
    init_msg='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{},"rootUri":"file:///tmp"}}'
    local header
    header="Content-Length: ${#init_msg}\r\n\r\n"

    local response
    response=$(printf "${header}${init_msg}" | timeout 10 "$BINARY" serve 2>/dev/null | head -c 4096 || true)

    if [[ -n "$response" && "$response" == *"Content-Length"* ]]; then
        echo "OK: gopls responds to LSP initialize (serve mode)"
    else
        echo "WARN: gopls did not respond to handshake (may need Go project context)"
    fi
}

# ── Main ──────────────────────────────────────────────────────────────
echo "=== RollBall LSP Setup: gopls (Go) ==="
install
verify
health_check
echo "=== Done ==="