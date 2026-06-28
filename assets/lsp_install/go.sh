#!/usr/bin/env bash
# ACowork LSP install script: gopls (Go)
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

BINARY="gopls"

# ── Helpers ────────────────────────────────────────────────────────

# Search VS Code extension for bundled gopls binary.
find_vscode_gopls() {
    for ext_dir in "$HOME/.vscode/extensions/golang.go-"*; do
        local candidate="$ext_dir/dist/gopls"
        if [[ -x "$candidate" ]]; then
            echo "$candidate"
            return 0
        fi
    done
    for ext_dir in "$HOME/.vscode-insiders/extensions/golang.go-"*; do
        local candidate="$ext_dir/dist/gopls"
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

find_gopls() {
    local gopath="${GOPATH:-$HOME/go}"
    for d in "$gopath/bin" "$HOME/go/bin"; do
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
    echo "[1/3] Installing gopls..."

    # Already on PATH?
    if command -v "$BINARY" &>/dev/null; then
        echo "gopls already on PATH at $(command -v "$BINARY")"
        return 0
    fi

    # Check VS Code extension (golang.go bundles gopls)
    local vscode_bin
    if vscode_bin=$(find_vscode_gopls); then
        local vscode_dir
        vscode_dir=$(dirname "$vscode_bin")
        echo "Found gopls from VS Code extension at $vscode_bin — adding to PATH..."
        add_to_path "$vscode_dir"
        if command -v "$BINARY" &>/dev/null; then
            return 0
        fi
    fi

    # Not on PATH — search GOPATH/bin
    local found
    found=$(find_gopls) || true
    if [[ -n "$found" ]]; then
        local dir
        dir=$(dirname "$found")
        echo "Found gopls at $found — adding $dir to PATH..."
        add_to_path "$dir"
        return 0
    fi

    # Not installed
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
        echo "OK: gopls found at $(command -v "$BINARY")"
        return 0
    fi

    # Check VS Code extension
    local vscode_bin
    if vscode_bin=$(find_vscode_gopls); then
        local vscode_dir
        vscode_dir=$(dirname "$vscode_bin")
        echo "Found gopls from VS Code extension at $vscode_bin — adding to PATH..."
        add_to_path "$vscode_dir"
        if command -v "$BINARY" &>/dev/null; then
            echo "OK: gopls found at $(command -v "$BINARY")"
            return 0
        fi
    fi

    # Search GOPATH/bin
    local found
    found=$(find_gopls) || true
    if [[ -n "$found" ]]; then
        local dir
        dir=$(dirname "$found")
        echo "Found gopls at $found — adding $dir to PATH..."
        add_to_path "$dir"
        if command -v "$BINARY" &>/dev/null; then
            echo "OK: gopls found at $(command -v "$BINARY")"
            return 0
        fi
    fi

    echo "ERROR: gopls not found on PATH after install (GOPATH/bin may not be on PATH)"
    echo "Try: export PATH=\$PATH:\$(go env GOPATH)/bin"
    exit 1
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
    response=$(printf "${header}${init_msg}" | _timeout 10 "$BINARY" serve 2>/dev/null | head -c 4096 || true)

    if [[ -n "$response" && "$response" == *"Content-Length"* ]]; then
        echo "OK: gopls responds to LSP initialize (serve mode)"
    else
        echo "WARN: gopls did not respond to handshake (may need Go project context)"
    fi
}

# ── Main ──────────────────────────────────────────────────────────────
echo "=== ACowork LSP Setup: gopls (Go) ==="
install
verify
health_check
echo "=== Done ==="