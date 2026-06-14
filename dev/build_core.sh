#!/usr/bin/env bash
# build_core.sh - Cross-platform rebuild and restart Gateway + Runtime
# Usage: ./dev/build_core.sh
# Supports: Linux, macOS, Windows (Git Bash, WSL, MSYS2)

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
GRAY='\033[0;37m'
NC='\033[0m' # No Color

# Determine workspace root
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(dirname "$SCRIPT_DIR")"
CORE_DIR="$WORKSPACE_ROOT/core"

# Detect OS
OS="unknown"
case "$(uname -s)" in
    Linux*)     OS="linux";;
    Darwin*)    OS="macos";;
    CYGWIN*)    OS="windows";;
    MINGW*)     OS="windows";;
    MSYS*)      OS="windows";;
    *)          OS="unknown";;
esac

echo -e "${CYAN}========================================${NC}"
echo -e "${CYAN}Rollball Core Rebuild & Restart Script${NC}"
echo -e "${CYAN}OS: $OS${NC}"
echo -e "${CYAN}========================================${NC}"
echo ""

# Function to stop process by name
stop_process() {
    local proc_name="$1"
    local display_name="$2"
    
    if [ "$OS" = "windows" ]; then
        # Windows: use taskkill or PowerShell
        local pids=$(powershell -Command "Get-Process -Name '$proc_name' -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Id" 2>/dev/null || true)
        if [ -n "$pids" ]; then
            echo -e "${GRAY}  Found $display_name processes: $pids${NC}"
            powershell -Command "Stop-Process -Name '$proc_name' -Force -ErrorAction SilentlyContinue" 2>/dev/null || true
            echo -e "${GREEN}  $display_name stopped.${NC}"
        else
            echo -e "${GRAY}  No $display_name process running.${NC}"
        fi
    else
        # Linux/macOS: use pkill or kill
        local pids=$(pgrep -f "$proc_name" 2>/dev/null || true)
        if [ -n "$pids" ]; then
            echo -e "${GRAY}  Found $display_name processes: $pids${NC}"
            pkill -f "$proc_name" 2>/dev/null || true
            # Wait for process to actually terminate
            sleep 1
            echo -e "${GREEN}  $display_name stopped.${NC}"
        else
            echo -e "${GRAY}  No $display_name process running.${NC}"
        fi
    fi
}

# Step 1: Stop running processes
echo -e "${YELLOW}[1/5] Stopping running Gateway, Runtime, and Embed processes...${NC}"
stop_process "rollball-gateway" "Gateway"
stop_process "rollball-runtime" "Runtime"
stop_process "rollball-embed"  "Embed"
echo ""

# Step 2: Build Gateway
echo -e "${YELLOW}[2/5] Building Gateway (release mode)...${NC}"
cd "$CORE_DIR"
if cargo build --release -p rollball-gateway 2>&1 | tee /tmp/gateway_build.log; then
    if grep -q "error" /tmp/gateway_build.log 2>/dev/null; then
        echo -e "${RED}  Gateway build failed with errors.${NC}"
        exit 1
    fi
    echo -e "${GREEN}  Gateway build completed.${NC}"
else
    echo -e "${RED}  Gateway build failed.${NC}"
    exit 1
fi
echo ""

# Step 3: Build Runtime
echo -e "${YELLOW}[3/5] Building Runtime (release mode)...${NC}"
if cargo build --release -p rollball-runtime 2>&1 | tee /tmp/runtime_build.log; then
    if grep -q "error" /tmp/runtime_build.log 2>/dev/null; then
        echo -e "${RED}  Runtime build failed with errors.${NC}"
        exit 1
    fi
    echo -e "${GREEN}  Runtime build completed.${NC}"
else
    echo -e "${RED}  Runtime build failed.${NC}"
    exit 1
fi
echo ""

# Step 3.5: Build Embedding Runtime
#
# ORT linking is auto-detected by rollball-embed's build.rs at compile time.
# No manual env vars or strategy selection needed.
#
# Users can skip this step entirely with: ./dev/build_core.sh --skip-embed

SKIP_EMBED=false
for arg in "$@"; do
    case "$arg" in
        --skip-embed) SKIP_EMBED=true ;;
    esac
done

if [ "$SKIP_EMBED" = "true" ]; then
    echo -e "${YELLOW}[3.5/5] Skipping Embedding Runtime (--skip-embed).${NC}"
else
    echo -e "${YELLOW}[3.5/5] Building Embedding Runtime (release mode)...${NC}"

    if cargo build --release -p rollball-embed 2>&1 | tee /tmp/embed_build.log; then
        if grep -q "error" /tmp/embed_build.log 2>/dev/null; then
            echo -e "${RED}  Embedding Runtime build failed with errors.${NC}"
            exit 1
        fi
        echo -e "${GREEN}  Embedding Runtime build completed.${NC}"
    else
        echo -e "${RED}  Embedding Runtime build failed.${NC}"
        exit 1
    fi

fi # end SKIP_EMBED check
rm -f /tmp/embed_build.log
echo ""

# Step 4: Copy offline_providers.json from assets to target dirs
echo -e "${YELLOW}[4/5] Copying offline_providers.json to target directories...${NC}"
OFFLINE_SRC="$WORKSPACE_ROOT/assets/offline_providers.json"
RELEASE_DIR="$WORKSPACE_ROOT/target/release"
DEBUG_DIR="$WORKSPACE_ROOT/target/debug"
if [ -f "$OFFLINE_SRC" ]; then
    cp "$OFFLINE_SRC" "$RELEASE_DIR/"
    echo -e "${GREEN}  Copied to $RELEASE_DIR${NC}"
    cp "$OFFLINE_SRC" "$DEBUG_DIR/"
    echo -e "${GREEN}  Copied to $DEBUG_DIR${NC}"
else
    echo -e "${RED}  WARNING: offline_providers.json not found at $OFFLINE_SRC${NC}"
fi

# Step 4.5: Copy embedding_models.json next to the gateway + embed binaries
#
# The gateway (and embed) read this from `{exe_dir}/embedding_models.json`.
# Whoever distributes the binary (this script for dev, the package installer
# for release, the Tauri bundler for desktop) is responsible for placing it
# there. Source of truth is core/rollball-embed/assets/embedding_models.json.
echo -e "${YELLOW}[4.5/5] Copying embedding_models.json next to binaries...${NC}"
EMBED_MODELS_SRC="$WORKSPACE_ROOT/core/rollball-embed/assets/embedding_models.json"
if [ -f "$EMBED_MODELS_SRC" ]; then
    for DIR in "$RELEASE_DIR" "$DEBUG_DIR"; do
        if [ -d "$DIR" ]; then
            cp "$EMBED_MODELS_SRC" "$DIR/embedding_models.json"
            echo -e "${GREEN}  Copied to $DIR${NC}"
        fi
    done
else
    echo -e "${RED}  WARNING: embedding_models.json not found at $EMBED_MODELS_SRC${NC}"
fi

echo ""

# Step 5: Start Gateway
echo -e "${YELLOW}[5/5] Starting Gateway in daemon mode (debug logging)...${NC}"
export ROLLBALL_GATEWAY_DAEMON="true"
export ROLLBALL_GATEWAY_LOG_LEVEL="debug"

GATEWAY_EXE=""
if [ "$OS" = "windows" ]; then
    GATEWAY_EXE="$WORKSPACE_ROOT/target/release/rollball-gateway.exe"
else
    GATEWAY_EXE="$WORKSPACE_ROOT/target/release/rollball-gateway"
fi

if [ -f "$GATEWAY_EXE" ]; then
    if [ "$OS" = "windows" ]; then
        # Windows: start in background
        start //b //min "$GATEWAY_EXE" 2>/dev/null || "$GATEWAY_EXE" &
    else
        # Linux/macOS: start in background, suppress output
        "$GATEWAY_EXE" > /dev/null 2>&1 &
    fi
    echo -e "${GREEN}  Gateway started (PID: $!).${NC}"
else
    echo -e "${RED}  Gateway executable not found at: $GATEWAY_EXE${NC}"
    exit 1
fi

echo ""
echo -e "${CYAN}========================================${NC}"
echo -e "${CYAN}Done! Gateway is running.${NC}"
echo -e "${CYAN}HTTP API: http://127.0.0.1:19876${NC}"
echo -e "${CYAN}========================================${NC}"

# Return to workspace root
cd "$WORKSPACE_ROOT"

# Cleanup temp files
rm -f /tmp/gateway_build.log /tmp/runtime_build.log
