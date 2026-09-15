#!/usr/bin/env bash
# start_node.sh - Start a local acowork-node daemon connected to a remote Gateway
#
# Usage:
#   ./dev/start_node.sh <GATEWAY_HOST:PORT> [options]
#
# Examples:
#   ./dev/start_node.sh 192.168.1.10:19875
#   ./dev/start_node.sh gateway.local:19875 --addr auto
#   ./dev/start_node.sh 10.0.0.5:19875 --addr 10.0.0.5:19900 --debug
#   ./dev/start_node.sh 192.168.1.10:19875 --no-build
#
# Stops an existing local Node / Runtimes / LSP relay, builds
# acowork-node + runtime + lsp-relay (incremental; default profile:
# release), then forwards `acowork-node start --gateway HOST:PORT
# --addr HOST:PORT`. The node auto-enrolls on first boot — one command
# = deployed (ADR-055 §6.13.2).
#
# Options:
#   --addr HOST:PORT     This node's public reverse-proxy address.
#                        Default: auto (live-detect LAN IP, ADR-055 §6.3.3).
#   --debug              Build/run the debug profile (default: release).
#   --release            Build/run the release profile (explicit).
#   --no-stop            Do NOT stop a running Node / Runtimes / LSP relay
#                        (diagnostics only — a second Node fights the first
#                        one over the broker session).
#   --no-build           Fail if the binary is missing instead of building.
#   -h, --help           Show this help.
#
# Supports: Linux, macOS, Windows (Git Bash / WSL / MSYS2).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
CORE_DIR="$PROJECT_ROOT/core"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
GRAY='\033[0;37m'
NC='\033[0m'

log()  { echo -e "${CYAN}[node]${NC} $*"; }
ok()   { echo -e "${GREEN}[node]${NC} $*"; }
warn() { echo -e "${YELLOW}[node]${NC} $*"; }
die()  { echo -e "${RED}[node]${NC} $*" >&2; exit 1; }

# OS detection (matches dev/build_core.sh so the binary path convention is
# consistent across all dev/ scripts).
OS="unknown"
case "$(uname -s)" in
    Linux*)     OS="linux";;
    Darwin*)    OS="macos";;
    CYGWIN*|MINGW*|MSYS*) OS="windows";;
    *)          OS="unknown";;
esac

usage() {
    # Print the leading comment block (line 2 through the first blank line)
    # with the leading `#` stripped. Sed range because the block contains
    # backticks that here-doc would try to expand.
    sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
}

# ── Argument parsing ────────────────────────────────────────────────

if [ $# -eq 0 ]; then usage; fi

GATEWAY_HOST=""
ADDR="auto"
PROFILE="release"
NO_STOP=0
NO_BUILD=0

while [ $# -gt 0 ]; do
    case "$1" in
        -h|--help)
            usage
            ;;
        --addr)
            [ $# -ge 2 ] || die "--addr requires a value"
            ADDR="$2"; shift 2
            ;;
        --addr=*)
            ADDR="${1#*=}"; shift
            ;;
        --debug)
            PROFILE="debug"; shift
            ;;
        --release)
            PROFILE="release"; shift
            ;;
        --no-stop)
            NO_STOP=1; shift
            ;;
        --no-build)
            NO_BUILD=1; shift
            ;;
        --*)
            die "unknown option: $1"
            ;;
        *)
            if [ -z "$GATEWAY_HOST" ]; then
                GATEWAY_HOST="$1"; shift
            else
                die "unexpected positional argument: $1"
            fi
            ;;
    esac
done

[ -n "$GATEWAY_HOST" ] || die "missing GATEWAY_HOST:PORT (positional arg 1)"
case "$GATEWAY_HOST" in
    *:*) : ;;
    *) die "GATEWAY_HOST must include a port (e.g. 192.168.1.10:19875)" ;;
esac

# ── Resolve binary path ─────────────────────────────────────────────

EXE_NAME="acowork-node"
SUFFIX=""
if [ "$OS" = "windows" ]; then EXE_NAME="acowork-node.exe"; SUFFIX=".exe"; fi
TARGET_DIR="$PROJECT_ROOT/target/$PROFILE"
BIN="$TARGET_DIR/$EXE_NAME"

# ── Stop (default) ──────────────────────────────────────────────────

# Stop the Node BEFORE building: a running binary may be file-locked
# (notably on Windows). This sweeps every acowork-* process on the
# machine, matching build_core.sh's stop step:
#   - acowork-runtime: the Node keeps Runtimes alive across its own death
#     by design (ADR-055 §6.10), and a Node killed abruptly never ran its
#     graceful shutdown — the survivors would otherwise race the new
#     Node's Runtimes over the broker (same instance identity => mutual
#     MQTT takeover).
#   - acowork-lsp-relay: sidecar on port 19878; a stale relay makes the
#     next Node's relay spawn fail on the port.
# `pgrep -f` instead of `pgrep -x`: the kernel truncates process names
# (comm) to 15 chars, so exact-name matching misses "acowork-lsp-relay"
# entirely (build_core.sh uses -f for the same reason).
stop_process() {
    local proc_name="$1"
    local display_name="$2"
    local pids
    if [ "$OS" = "windows" ]; then
        pids=$(powershell -Command "Get-Process -Name '$proc_name' -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Id" 2>/dev/null || true)
        if [ -n "$pids" ]; then
            echo -e "${GRAY}  Found $display_name processes: $pids${NC}"
            powershell -Command "Stop-Process -Name '$proc_name' -Force -ErrorAction SilentlyContinue" 2>/dev/null || true
        else
            echo -e "${GRAY}  No $display_name process running.${NC}"
        fi
    else
        pids=$(pgrep -f "$proc_name" 2>/dev/null || true)
        if [ -n "$pids" ]; then
            echo -e "${GRAY}  Found $display_name processes: $pids${NC}"
            pkill -f "$proc_name" 2>/dev/null || true
        else
            echo -e "${GRAY}  No $display_name process running.${NC}"
        fi
    fi
}

if [ "$NO_STOP" != "1" ]; then
    log "stopping existing Node / Runtimes / LSP relay (if any)"
    stop_process "acowork-node"      "Node Agent"
    stop_process "acowork-runtime"   "Runtime"
    stop_process "acowork-lsp-relay" "LSP Relay"
    # Give the OS a moment to release the proxy (19900) / relay (19878)
    # listeners before the new Node binds them.
    sleep 1
    ok "stop step done"
fi

# ── Build (incremental, on by default) ──────────────────────────────

# Always invoke cargo: cargo is the only reliable arbiter of "needs a
# rebuild" (source freshness, profile, features). With an up-to-date
# target dir this takes a few seconds; --no-build skips it entirely.
if [ "$NO_BUILD" != "1" ]; then
    log "building acowork-node + runtime + lsp-relay ($PROFILE, incremental)"
    if [ "$PROFILE" = "release" ]; then
        cargo build --manifest-path "$CORE_DIR/Cargo.toml" -p acowork-node -p acowork-runtime -p acowork-lsp-relay --release
    else
        cargo build --manifest-path "$CORE_DIR/Cargo.toml" -p acowork-node -p acowork-runtime -p acowork-lsp-relay
    fi
fi

# Fail loudly when a required binary is still missing (e.g. --no-build on
# a fresh checkout).
[ -f "$BIN" ] || die "binary not found at $BIN — run dev/build_core.sh first (or drop --no-build)"
for s in acowork-runtime acowork-lsp-relay; do
    [ -f "$TARGET_DIR/$s$SUFFIX" ] || die "sibling binary not found at $TARGET_DIR/$s$SUFFIX — run dev/build_core.sh first (or drop --no-build)"
done

# ── Start ───────────────────────────────────────────────────────────

log "starting acowork-node -> gateway $GATEWAY_HOST  (profile=$PROFILE)"
log "Ctrl+C to stop. Logs: $HOME/.acowork/acowork-node/logs"

exec "$BIN" start --gateway "$GATEWAY_HOST" --addr "$ADDR"
