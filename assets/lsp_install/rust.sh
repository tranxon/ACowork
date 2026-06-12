#!/usr/bin/env bash
# RollBall LSP install script: rust-analyzer
# Phases: Install → Verify → Health Check

set -euo pipefail

BINARY="rust-analyzer"

# ── Phase 1: Install ──────────────────────────────────────────────────
install() {
    echo "[1/3] Installing rust-analyzer..."
    if command -v rustup &>/dev/null; then
        rustup component add rust-analyzer
    else
        echo "ERROR: rustup not found. Install Rust first: https://rustup.rs"
        exit 1
    fi
}

# ── Phase 2: Verify ──────────────────────────────────────────────────
verify() {
    echo "[2/3] Verifying rust-analyzer is on PATH..."
    if command -v "$BINARY" &>/dev/null; then
        local path
        path=$(command -v "$BINARY")
        echo "OK: rust-analyzer found at $path"
    else
        echo "ERROR: rust-analyzer not found on PATH after install"
        exit 1
    fi
}

# ── Phase 3: Health Check ────────────────────────────────────────────
# rust-analyzer defaults to stdio mode; no --stdio flag needed.
health_check() {
    echo "[3/3] Health check: testing stdio handshake..."
    # Send a minimal LSP initialize request; expect a response header.
    local init_msg
    init_msg='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{},"rootUri":"file:///tmp"}}'
    local header
    header="Content-Length: ${#init_msg}\r\n\r\n"

    local response
    response=$(printf "${header}${init_msg}" | timeout 10 "$BINARY" 2>/dev/null | head -c 4096 || true)

    if [[ -n "$response" && "$response" == *"Content-Length"* ]]; then
        echo "OK: rust-analyzer responds to LSP initialize (stdio mode)"
    else
        echo "WARN: rust-analyzer did not respond to handshake (may need project context)"
    fi
}

# ── Main ──────────────────────────────────────────────────────────────
echo "=== RollBall LSP Setup: rust-analyzer ==="
install
verify
health_check
echo "=== Done ==="