#!/usr/bin/env bash
# RollBall LSP install script: marksman (Markdown)
# Phases: Install → Verify → Health Check
# marksman is a standalone binary (not npm). Defaults to stdio mode.

set -euo pipefail

BINARY="marksman"

# ── Phase 1: Install ──────────────────────────────────────────────────
install() {
    echo "[1/3] Installing marksman..."

    # Already on PATH?
    if command -v "$BINARY" &>/dev/null; then
        echo "marksman already on PATH at $(command -v "$BINARY")"
        return 0
    fi

    # Not on PATH — search common locations
    for d in "$HOME/.local/bin" "/usr/local/bin" "/usr/bin"; do
        local candidate="$d/$BINARY"
        if [[ -x "$candidate" ]]; then
            echo "Found marksman at $candidate — adding $d to PATH..."
            case ":$PATH:" in
                *:"$d":*) ;;
                *) export PATH="$PATH:$d" ;;
            esac
            return 0
        fi
    done

    # Not installed — try brew or download
    if [[ "$(uname -s)" == "Darwin" ]] && command -v brew &>/dev/null; then
        brew install marksman
    elif command -v curl &>/dev/null; then
        echo "Downloading marksman from GitHub releases..."
        local os arch url
        case "$(uname -s)" in
            Linux)  os="linux" ;;
            Darwin) os="macos" ;;
            *)      echo "ERROR: Unsupported OS" && exit 1 ;;
        esac
        case "$(uname -m)" in
            x86_64)  arch="x64" ;;
            aarch64) arch="arm64" ;;
            arm64)   arch="arm64" ;;
            *)       echo "ERROR: Unsupported architecture" && exit 1 ;;
        esac
        url="https://github.com/artempyanykh/marksman/releases/latest/download/marksman-${os}-${arch}"
        # Try a system-wide install first (requires sudo on Linux).
        # Fall back to user-local install (~/.local/bin) which is commonly on PATH.
        local dest
        if [[ -w "/usr/local/bin" ]]; then
            dest="/usr/local/bin/marksman"
        elif command -v sudo &>/dev/null; then
            dest="/usr/local/bin/marksman"
            echo "Downloading $url → $dest (sudo)"
            if curl -fsSL "$url" | sudo tee "$dest" >/dev/null; then
                sudo chmod +x "$dest"
                echo "OK: marksman installed to $dest"
                return
            fi
        fi
        # Fallback: user-local install
        dest="$HOME/.local/bin/marksman"
        mkdir -p "$HOME/.local/bin"
        echo "Downloading $url → $dest"
        if curl -fsSL "$url" -o "$dest"; then
            chmod +x "$dest"
            # Ensure ~/.local/bin is on PATH for this session
            case ":$PATH:" in
                *:"$HOME/.local/bin":*) ;;
                *) export PATH="$HOME/.local/bin:$PATH" ;;
            esac
            echo "OK: marksman installed to $dest (~/.local/bin added to PATH)"
        else
            echo "ERROR: Download failed. Install manually: https://github.com/artempyanykh/marksman/releases"
            exit 1
        fi
    else
        echo "ERROR: Neither brew nor curl found."
        echo "Install manually: https://github.com/artempyanykh/marksman/releases"
        exit 1
    fi
}

# ── Phase 2: Verify ──────────────────────────────────────────────────
verify() {
    echo "[2/3] Verifying marksman is on PATH..."
    if command -v "$BINARY" &>/dev/null; then
        echo "OK: marksman found at $(command -v "$BINARY")"
        return 0
    fi

    # Search common locations
    for d in "$HOME/.local/bin" "/usr/local/bin" "/usr/bin"; do
        local candidate="$d/$BINARY"
        if [[ -x "$candidate" ]]; then
            echo "Found marksman at $candidate — adding $d to PATH..."
            case ":$PATH:" in
                *:"$d":*) ;;
                *) export PATH="$PATH:$d" ;;
            esac
            if command -v "$BINARY" &>/dev/null; then
                echo "OK: marksman found at $(command -v "$BINARY")"
                return 0
            fi
        fi
    done

    echo "ERROR: marksman not found on PATH after install"
    exit 1
}

# ── Phase 3: Health Check ────────────────────────────────────────────
# marksman defaults to stdio mode; no --stdio flag needed.
health_check() {
    echo "[3/3] Health check: testing stdio handshake..."
    local init_msg
    init_msg='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{},"rootUri":"file:///tmp"}}'
    local header
    header="Content-Length: ${#init_msg}\r\n\r\n"

    local response
    response=$(printf "${header}${init_msg}" | timeout 10 "$BINARY" 2>/dev/null | head -c 4096 || true)

    if [[ -n "$response" && "$response" == *"Content-Length"* ]]; then
        echo "OK: marksman responds to LSP initialize (stdio mode)"
    else
        echo "WARN: marksman did not respond to handshake (may need workspace context)"
    fi
}

# ── Main ──────────────────────────────────────────────────────────────
echo "=== RollBall LSP Setup: marksman (Markdown) ==="
install
verify
health_check
echo "=== Done ==="
