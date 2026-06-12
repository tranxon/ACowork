#!/usr/bin/env bash
# RollBall LSP install script: clangd (C/C++)
# Phases: Install → Verify → Health Check

set -euo pipefail

BINARY="clangd"

# ── Phase 1: Install ──────────────────────────────────────────────────
install() {
    echo "[1/3] Installing clangd..."

    # Try multiple installation methods
    if command -v apt-get &>/dev/null; then
        sudo apt-get update && sudo apt-get install -y clangd
    elif command -v brew &>/dev/null; then
        brew install llvm
    elif command -v dnf &>/dev/null; then
        sudo dnf install -y clang-tools-extra
    elif command -v pacman &>/dev/null; then
        sudo pacman -S clang
    else
        echo "ERROR: No supported package manager found."
        echo "Install clangd manually: https://clangd.llvm.org/installation"
        exit 1
    fi
}

# ── Phase 2: Verify ──────────────────────────────────────────────────
verify() {
    echo "[2/3] Verifying clangd is on PATH..."
    if command -v "$BINARY" &>/dev/null; then
        local path
        path=$(command -v "$BINARY")
        echo "OK: clangd found at $path"
    else
        echo "ERROR: clangd not found on PATH after install"
        exit 1
    fi
}

# ── Phase 3: Health Check ────────────────────────────────────────────
# clangd defaults to stdio mode; no --stdio flag needed.
health_check() {
    echo "[3/3] Health check: testing stdio handshake..."
    local init_msg
    init_msg='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{},"rootUri":"file:///tmp"}}'
    local header
    header="Content-Length: ${#init_msg}\r\n\r\n"

    local response
    response=$(printf "${header}${init_msg}" | timeout 10 "$BINARY" 2>/dev/null | head -c 4096 || true)

    if [[ -n "$response" && "$response" == *"Content-Length"* ]]; then
        echo "OK: clangd responds to LSP initialize (stdio mode)"
    else
        echo "WARN: clangd did not respond to handshake (may need project context)"
    fi
}

# ── Main ──────────────────────────────────────────────────────────────
echo "=== RollBall LSP Setup: clangd (C/C++) ==="
install
verify
health_check
echo "=== Done ==="