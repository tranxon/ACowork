#!/usr/bin/env bash
# ACowork LSP install script: rust-analyzer
# Phases: Install → Verify → Health Check

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

BINARY="rust-analyzer"

# ── Helpers ────────────────────────────────────────────────────────

# Search VS Code extension for bundled rust-analyzer binary.
find_vscode_rust_analyzer() {
    for ext_dir in "$HOME/.vscode/extensions/rust-lang.rust-analyzer-"*; do
        local candidate="$ext_dir/server/rust-analyzer"
        if [[ -x "$candidate" ]]; then
            echo "$candidate"
            return 0
        fi
    done
    for ext_dir in "$HOME/.vscode-insiders/extensions/rust-lang.rust-analyzer-"*; do
        local candidate="$ext_dir/server/rust-analyzer"
        if [[ -x "$candidate" ]]; then
            echo "$candidate"
            return 0
        fi
    done
    return 1
}

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

    # Check VS Code extension
    local vscode_bin
    if vscode_bin=$(find_vscode_rust_analyzer); then
        local vscode_dir
        vscode_dir=$(dirname "$vscode_bin")
        echo "Found rust-analyzer from VS Code extension at $vscode_bin — adding to PATH..."
        add_to_path "$vscode_dir"
        if command -v "$BINARY" &>/dev/null && "$BINARY" --version &>/dev/null; then
            echo "OK: rust-analyzer found at $(command -v "$BINARY")"
            return 0
        fi
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
    response=$(printf "${header}${init_msg}" | _timeout 10 "$BINARY" 2>/dev/null | head -c 4096 || true)

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