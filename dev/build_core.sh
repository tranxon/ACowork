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
echo -e "${YELLOW}[1/4] Stopping running Gateway and Runtime processes...${NC}"
stop_process "rollball-gateway" "Gateway"
stop_process "rollball-runtime" "Runtime"
echo ""

# Step 2: Build Gateway
echo -e "${YELLOW}[2/4] Building Gateway (release mode)...${NC}"
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
echo -e "${YELLOW}[3/4] Building Runtime (release mode)...${NC}"
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

# Step 4: Start Gateway
echo -e "${YELLOW}[4/4] Starting Gateway in daemon mode (debug logging)...${NC}"
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
