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
# Auto-builds acowork-node on first run (default profile: release). On
# subsequent runs it skips the build unless --force-build is given.
# Forwards `acowork-node start --gateway HOST:PORT --addr HOST:PORT`. The
# node auto-enrolls on first boot — one command = deployed (ADR-055 §6.13.2).
#
# Options:
#   --addr HOST:PORT     This node's public reverse-proxy address.
#                        Default: auto (live-detect LAN IP, ADR-055 §6.3.3).
#   --debug              Build debug profile (default: release).
#   --release            Build release profile (explicit).
#   --force-build        Rebuild even if the binary already exists.
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
FORCE_BUILD=0
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
        --force-build)
            FORCE_BUILD=1; shift
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

# ── Resolve binary path ──────────────���──────────────────────────────

EXE_NAME="acowork-node"
if [ "$OS" = "windows" ]; then EXE_NAME="acowork-node.exe"; fi
TARGET_DIR="$CORE_DIR/target/$PROFILE"
BIN="$TARGET_DIR/$EXE_NAME"

# ── Build if needed ─────────────────────────────────────────────────

if [ ! -f "$BIN" ]; then
    if [ "$NO_BUILD" = "1" ]; then
        die "binary not found at $BIN and --no-build was given; run dev/build_core.sh first"
    fi
    warn "binary not found, building acowork-node ($PROFILE) — first run takes a few minutes"
    cargo build --manifest-path "$CORE_DIR/Cargo.toml" -p acowork-node --profile "$PROFILE"
fi

if [ "$FORCE_BUILD" = "1" ] && [ "$NO_BUILD" != "1" ]; then
    cargo build --manifest-path "$CORE_DIR/Cargo.toml" -p acowork-node --profile "$PROFILE"
fi

[ -f "$BIN" ] || die "binary still not found at $BIN after build"

# ── Start ───────────────────────────────────────────────────────────

log "starting acowork-node -> gateway $GATEWAY_HOST  (profile=$PROFILE)"
case "$OS" in
    windows)
        # Git Bash: log dir follows the user's $HOME. Mirror what the .ps1
        # script prints so users on either shell see the same line.
        log "Ctrl+C to stop. Logs: $HOME/.acowork/acowork-node/data/logs"
        ;;
    *)
        log "Ctrl+C to stop. Logs: $HOME/.acowork/acowork-node/data/logs"
        ;;
esac

exec "$BIN" start --gateway "$GATEWAY_HOST" --addr "$ADDR"