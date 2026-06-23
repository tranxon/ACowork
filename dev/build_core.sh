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
echo -e "${CYAN}AgentCowork Core Rebuild & Restart Script${NC}"
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
stop_process "acowork-gateway" "Gateway"
stop_process "acowork-runtime" "Runtime"
stop_process "acowork-embed"  "Embed"

# Ensure embed port is released before starting a new gateway.
# On Unix, pkill may not have finished releasing port 18080 within the
# 1s sleep; the new gateway spawns its own embed immediately and if the
# old one is still binding, the new embed panics with AddrInUse.
if [ "$OS" = "linux" ] || [ "$OS" = "macos" ]; then
    if command -v fuser &>/dev/null; then
        fuser -k 18080/tcp 2>/dev/null || true
    fi
    # Wait up to 3s for the port to be released
    waited=0
    while command -v ss &>/dev/null && ss -tlnp 2>/dev/null | grep -q ":18080 "; do
        sleep 0.5
        waited=$((waited + 1))
        if [ $waited -ge 6 ]; then
            echo -e "${RED}  WARNING: Port 18080 still in use after 3s${NC}"
            break
        fi
    done
fi
echo ""

# Step 2: Build Gateway
echo -e "${YELLOW}[2/5] Building Gateway (release mode)...${NC}"
cd "$CORE_DIR"
if cargo build --release -p acowork-gateway 2>&1 | tee /tmp/gateway_build.log; then
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
if cargo build --release -p acowork-runtime 2>&1 | tee /tmp/runtime_build.log; then
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
# This script probes .ort/onnxruntime-*/lib for a local ONNX Runtime install
# and exports ORT_LIB_LOCATION / ORT_DYLIB_PATH / ORT_PREFER_DYNAMIC_LINK
# before invoking cargo. Run dev/setup_ort.sh first to download ORT.
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

    # Auto-detect local ONNX Runtime install under .ort/
    if [ -z "$ORT_LIB_LOCATION" ]; then
        for ort_dir in "$WORKSPACE_ROOT"/.ort/onnxruntime-*; do
            [ -d "$ort_dir" ] || continue
            local_lib=""
            case "$OS" in
                macos)   local_lib="$ort_dir/lib/libonnxruntime.dylib" ;;
                windows) local_lib="$ort_dir/lib/onnxruntime.dll" ;;
                *)       local_lib="$ort_dir/lib/libonnxruntime.so" ;;
            esac
            if [ -f "$local_lib" ]; then
                export ORT_LIB_LOCATION="$ort_dir/lib"
                export ORT_DYLIB_PATH="$local_lib"
                export ORT_PREFER_DYNAMIC_LINK=1
                echo -e "${GREEN}  Detected local ORT: $ort_dir/lib${NC}"
                break
            fi
        done
    fi
    
    # Fallback: Auto-detect from Cargo cache if download-ort feature was used
    if [ -z "$ORT_LIB_LOCATION" ]; then
        echo -e "${YELLOW}  .ort/ not found, searching Cargo registry cache...${NC}"
        for cached_ort in ~/.cargo/registry/cache/*/onnxruntime-osx-aarch64-* ~/.cargo/registry/cache/*/onnxruntime-osx-x64-* 2>/dev/null; do
            [ -d "$cached_ort" ] || continue
            if [ "$OS" = "macos" ]; then
                lib_path="$cached_ort/lib/libonnxruntime.dylib"
            else
                lib_path="$cached_ort/lib/libonnxruntime.so"
            fi
            if [ -f "$lib_path" ]; then
                echo -e "${GREEN}  Found ONNX Runtime in Cargo cache: $lib_path${NC}"
                # Create a symlink in .ort/ so build_core.sh can use it
                WORKSPACE_ROOT_ORT="$WORKSPACE_ROOT/.ort/onnxruntime-osx-aarch64-latest"
                mkdir -p "$WORKSPACE_ROOT_ORT/lib"
                cp "$lib_path" "$WORKSPACE_ROOT_ORT/lib/"
                echo -e "${GREEN}  Copied to $WORKSPACE_ROOT_ORT${NC}"
                export ORT_LIB_LOCATION="$WORKSPACE_ROOT_ORT/lib"
                export ORT_DYLIB_PATH="$lib_path"
                export ORT_PREFER_DYNAMIC_LINK=1
                break
            fi
        done
    fi
    if [ -z "$ORT_LIB_LOCATION" ]; then
        echo -e "${RED}  ONNX Runtime not found. Run ./dev/setup_ort.sh first.${NC}"
        echo -e "${RED}  Alternative: cargo build --release -p acowork-embed --features download-ort${NC}"
        exit 1
    fi

    if cargo build --release -p acowork-embed 2>&1 | tee /tmp/embed_build.log; then
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
# there. Source of truth is core/acowork-embed/assets/embedding_models.json.
echo -e "${YELLOW}[4.5/5] Copying embedding_models.json next to binaries...${NC}"
EMBED_MODELS_SRC="$WORKSPACE_ROOT/core/acowork-embed/assets/embedding_models.json"
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
export ACOWORK_GATEWAY_DAEMON="true"
export ACOWORK_GATEWAY_LOG_LEVEL="debug"

GATEWAY_EXE=""
if [ "$OS" = "windows" ]; then
    GATEWAY_EXE="$WORKSPACE_ROOT/target/release/acowork-gateway.exe"
else
    GATEWAY_EXE="$WORKSPACE_ROOT/target/release/acowork-gateway"
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
