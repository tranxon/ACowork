#!/usr/bin/env bash
# ACowork LSP install script: pylsp (Python)
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

BINARY="pylsp"

# ── Helpers ────────────────────────────────────────────────────────

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

find_pylsp() {
    for d in \
        "$HOME/.local/bin" \
        "/usr/local/bin" \
        "/opt/homebrew/bin" \
        "$HOME/.local/pipx/venvs/python-lsp-server/bin" \
        "$HOME/Library/Python/3."*"/bin" \
        "/Library/Frameworks/Python.framework/Versions/3."*"/bin"; do
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
    echo "[1/3] Installing python-lsp-server..."

    # Already on PATH?
    if command -v "$BINARY" &>/dev/null; then
        echo "pylsp already on PATH at $(command -v "$BINARY")"
        return 0
    fi

    # Not on PATH — search common locations
    local found
    found=$(find_pylsp) || true
    if [[ -n "$found" ]]; then
        local dir
        dir=$(dirname "$found")
        echo "Found pylsp at $found — adding $dir to PATH..."
        add_to_path "$dir"
        return 0
    fi

    # Not installed — try multiple strategies in order of preference.
    # macOS Homebrew Python enables PEP 668 (externally-managed-environment),
    # which blocks bare `pip install`. We try each strategy and stop on
    # the first one that succeeds.

    # Strategy 1: Homebrew (macOS) — has a bottled formula, no compilation
    if command -v brew &>/dev/null && [[ "$(uname -s)" == "Darwin" ]]; then
        echo "Trying: brew install python-lsp-server..."
        if brew install python-lsp-server 2>/dev/null; then
            echo "Installed via Homebrew."
            # Homebrew puts it in a versioned path; find it
            local brew_pylsp
            brew_pylsp=$(find /opt/homebrew/Cellar/python-lsp-server -name pylsp -type f 2>/dev/null | head -1) || true
            if [[ -z "$brew_pylsp" ]]; then
                brew_pylsp=$(find /usr/local/Cellar/python-lsp-server -name pylsp -type f 2>/dev/null | head -1) || true
            fi
            if [[ -n "$brew_pylsp" ]]; then
                local brew_dir
                brew_dir=$(dirname "$brew_pylsp")
                add_to_path "$brew_dir"
            fi
            return 0
        fi
        echo "Homebrew install failed, trying next strategy..."
    fi

    # Strategy 2: pipx (recommended by PEP 668 for CLI tools)
    if command -v pipx &>/dev/null; then
        echo "Trying: pipx install python-lsp-server..."
        if pipx install python-lsp-server 2>/dev/null; then
            pipx ensurepath 2>/dev/null || true
            # pipx puts symlinks in ~/.local/bin
            add_to_path "$HOME/.local/bin"
            echo "Installed via pipx."
            return 0
        fi
        echo "pipx install failed, trying next strategy..."
    fi

    # Strategy 3: pip install --user (per-user site-packages, PEP 668 compliant)
    if command -v pip3 &>/dev/null; then
        echo "Trying: pip3 install --user python-lsp-server..."
        if pip3 install --user python-lsp-server 2>/dev/null; then
            # Find the user site-packages bin directory
            local user_base
            user_base=$(python3 -c "import site; print(site.USER_BASE)" 2>/dev/null) || true
            if [[ -n "$user_base" ]]; then
                add_to_path "$user_base/bin"
            fi
            echo "Installed via pip3 --user."
            return 0
        fi
        echo "pip3 --user failed, trying next strategy..."
    elif command -v pip &>/dev/null; then
        echo "Trying: pip install --user python-lsp-server..."
        if pip install --user python-lsp-server 2>/dev/null; then
            local user_base
            user_base=$(python3 -c "import site; print(site.USER_BASE)" 2>/dev/null) || true
            if [[ -n "$user_base" ]]; then
                add_to_path "$user_base/bin"
            fi
            echo "Installed via pip --user."
            return 0
        fi
        echo "pip --user failed, trying next strategy..."
    fi

    # Strategy 4: pip install --break-system-packages (last resort, macOS Homebrew)
    if command -v pip3 &>/dev/null; then
        echo "Trying: pip3 install --break-system-packages python-lsp-server..."
        if pip3 install --break-system-packages python-lsp-server 2>/dev/null; then
            echo "Installed via pip3 --break-system-packages."
            return 0
        fi
    fi

    echo "ERROR: All installation strategies failed."
    echo ""
    echo "Manual options:"
    echo "  1. brew install python-lsp-server"
    echo "  2. pipx install python-lsp-server"
    echo "  3. pip3 install --user python-lsp-server"
    echo "  4. Or install VS Code Python extension (bundles Pyright)"
    exit 1
}

# ── Phase 2: Verify ──────────────────────────────────────────────────
verify() {
    echo "[2/3] Verifying pylsp is on PATH..."
    if command -v "$BINARY" &>/dev/null; then
        echo "OK: pylsp found at $(command -v "$BINARY")"
        return 0
    fi

    # Search common locations
    local found
    found=$(find_pylsp) || true
    if [[ -n "$found" ]]; then
        local dir
        dir=$(dirname "$found")
        echo "Found pylsp at $found — adding $dir to PATH..."
        add_to_path "$dir"
        if command -v "$BINARY" &>/dev/null; then
            echo "OK: pylsp found at $(command -v "$BINARY")"
            return 0
        fi
    fi

    echo "ERROR: pylsp not found on PATH after install"
    exit 1
}

# ── Phase 3: Health Check ────────────────────────────────────────────
# pylsp defaults to stdio mode — do NOT pass --stdio (it rejects it).
# pyright-langserver requires --stdio. We detect which binary is on PATH
# and use the correct args.
health_check() {
    echo "[3/3] Health check: testing stdio handshake..."
    local init_msg
    init_msg='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{},"rootUri":"file:///tmp"}}'
    local header
    header="Content-Length: ${#init_msg}\r\n\r\n"

    # Determine which LSP binary we have and what args it needs
    local lsp_cmd
    local lsp_args=()
    if command -v pyright-langserver &>/dev/null; then
        lsp_cmd="pyright-langserver"
        lsp_args=("--stdio")
    elif command -v pylsp &>/dev/null; then
        lsp_cmd="pylsp"
        # pylsp defaults to stdio, rejects --stdio
    elif command -v python-lsp-server &>/dev/null; then
        lsp_cmd="python-lsp-server"
        # python-lsp-server defaults to stdio, rejects --stdio
    else
        echo "WARN: No Python LSP binary found for health check"
        return 0
    fi

    local response
    if [[ ${#lsp_args[@]} -gt 0 ]]; then
        response=$(printf "${header}${init_msg}" | _timeout 10 "$lsp_cmd" "${lsp_args[@]}" 2>/dev/null | head -c 4096 || true)
    else
        response=$(printf "${header}${init_msg}" | _timeout 10 "$lsp_cmd" 2>/dev/null | head -c 4096 || true)
    fi

    if [[ -n "$response" && "$response" == *"Content-Length"* ]]; then
        echo "OK: $lsp_cmd responds to LSP initialize"
    else
        echo "WARN: $lsp_cmd did not respond to handshake"
    fi
}

# ── Main ──────────────────────────────────────────────────────────────
echo "=== ACowork LSP Setup: pylsp (Python) ==="
install
verify
health_check
echo "=== Done ==="