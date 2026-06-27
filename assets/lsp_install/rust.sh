#!/usr/bin/env bash
# ACowork LSP install script: rust-analyzer
# Phases: Install → Verify → Health Check

set -euo pipefail

BINARY="rust-analyzer"

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

# ── Phase 1: Install ──────────────────────────────────────────────────
install() {
    echo "[1/3] Installing rust-analyzer..."

    # Already on PATH and actually runnable?
    # rustup creates a proxy at ~/.cargo/bin/rust-analyzer that exists on PATH
    # but fails at runtime if the component isn't installed for the active
    # toolchain.  `--version` catches this — the proxy exits non-zero.
    if command -v "$BINARY" &>/dev/null && "$BINARY" --version &>/dev/null; then
        echo "rust-analyzer already on PATH at $(command -v "$BINARY")"
        return 0
    fi

    # Not on PATH — search ~/.cargo/bin
    local cargo_bin="$HOME/.cargo/bin"
    local candidate="$cargo_bin/$BINARY"
    if [[ -x "$candidate" ]]; then
        echo "Found rust-analyzer at $candidate — adding $cargo_bin to PATH..."
        add_to_path "$cargo_bin"
        # Re-check after PATH update
        if command -v "$BINARY" &>/dev/null && "$BINARY" --version &>/dev/null; then
            return 0
        fi
    fi

    # Not installed or broken proxy — install the component
    if command -v rustup &>/dev/null; then
        echo "Installing rust-analyzer component via rustup..."
        rustup component add rust-analyzer
    else
        echo "ERROR: rustup not found. Install Rust first: https://rustup.rs"
        exit 1
    fi
}

# ── Phase 2: Verify ──────────────────────────────────────────────────
verify() {
    echo "[2/3] Verifying rust-analyzer is on PATH and runnable..."
    if command -v "$BINARY" &>/dev/null && "$BINARY" --version &>/dev/null; then
        echo "OK: rust-analyzer found at $(command -v "$BINARY")"
        return 0
    fi

    # Search ~/.cargo/bin
    local cargo_bin="$HOME/.cargo/bin"
    local candidate="$cargo_bin/$BINARY"
    if [[ -x "$candidate" ]]; then
        echo "Found rust-analyzer at $candidate — adding $cargo_bin to PATH..."
        add_to_path "$cargo_bin"
        if command -v "$BINARY" &>/dev/null && "$BINARY" --version &>/dev/null; then
            echo "OK: rust-analyzer found at $(command -v "$BINARY")"
            return 0
        fi
    fi

    echo "ERROR: rust-analyzer not found or not runnable on PATH after install"
    exit 1
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
echo "=== ACowork LSP Setup: rust-analyzer ==="
install
verify
health_check
echo "=== Done ==="