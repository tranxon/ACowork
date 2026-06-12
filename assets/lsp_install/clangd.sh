#!/usr/bin/env bash
# RollBall LSP install script: clangd (C/C++)
# Phases: Install → Verify → Health Check

set -euo pipefail

BINARY="clangd"

# ── Phase 1: Install ──────────────────────────────────────────────────
install() {
    echo "[1/3] Installing clangd..."

    # Already on PATH?
    if command -v "$BINARY" &>/dev/null; then
        echo "clangd already on PATH at $(command -v "$BINARY")"
        return
    fi

    # Not on PATH — check common LLVM install locations
    local llvm_paths=(
        "/usr/bin/clangd"
        "/usr/local/bin/clangd"
        "/opt/homebrew/opt/llvm/bin/clangd"
        "/usr/local/opt/llvm/bin/clangd"
    )
    for p in "${llvm_paths[@]}"; do
        if [[ -x "$p" ]]; then
            local dir
            dir=$(dirname "$p")
            echo "Found clangd at $p — adding $dir to PATH..."
            # Add to current session
            export PATH="$PATH:$dir"
            # Add to shell profile for persistence
            local profile_file
            if [[ -f "$HOME/.bashrc" ]]; then
                profile_file="$HOME/.bashrc"
            elif [[ -f "$HOME/.zshrc" ]]; then
                profile_file="$HOME/.zshrc"
            elif [[ -f "$HOME/.profile" ]]; then
                profile_file="$HOME/.profile"
            fi
            if [[ -n "${profile_file:-}" ]] && ! grep -q "$dir" "$profile_file" 2>/dev/null; then
                echo "export PATH=\"\$PATH:$dir\"" >> "$profile_file"
                echo "Added to $profile_file for persistence."
            fi
            return
        fi
    done

    # Not installed at all — try package managers
    if command -v apt-get &>/dev/null; then
        sudo apt-get update && sudo apt-get install -y clangd
    elif command -v brew &>/dev/null; then
        brew install llvm
        # Homebrew's llvm is keg-only; add to PATH
        local brew_llvm_bin
        if [[ "$(uname -m)" == "arm64" ]]; then
            brew_llvm_bin="/opt/homebrew/opt/llvm/bin"
        else
            brew_llvm_bin="/usr/local/opt/llvm/bin"
        fi
        if [[ -d "$brew_llvm_bin" ]]; then
            export PATH="$PATH:$brew_llvm_bin"
            echo "Added Homebrew LLVM bin to PATH: $brew_llvm_bin"
        fi
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
        return
    fi

    # Still not found — search common LLVM install locations
    local llvm_paths=(
        "/usr/bin/clangd"
        "/usr/local/bin/clangd"
        "/opt/homebrew/opt/llvm/bin/clangd"
        "/usr/local/opt/llvm/bin/clangd"
    )
    for p in "${llvm_paths[@]}"; do
        if [[ -x "$p" ]]; then
            local dir
            dir=$(dirname "$p")
            echo "Found clangd at $p — adding $dir to PATH..."
            export PATH="$PATH:$dir"
            local profile_file
            if [[ -f "$HOME/.bashrc" ]]; then
                profile_file="$HOME/.bashrc"
            elif [[ -f "$HOME/.zshrc" ]]; then
                profile_file="$HOME/.zshrc"
            elif [[ -f "$HOME/.profile" ]]; then
                profile_file="$HOME/.profile"
            fi
            if [[ -n "${profile_file:-}" ]] && ! grep -q "$dir" "$profile_file" 2>/dev/null; then
                echo "export PATH=\"\$PATH:$dir\"" >> "$profile_file"
            fi
            path=$(command -v "$BINARY")
            echo "OK: clangd found at $path"
            return
        fi
    done

    echo "ERROR: clangd not found on PATH after install"
    echo "Make sure LLVM bin directory is on your PATH"
    exit 1
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