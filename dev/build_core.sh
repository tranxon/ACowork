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
echo -e "${YELLOW}[1/5] Stopping running Gateway and Runtime processes...${NC}"
stop_process "rollball-gateway" "Gateway"
stop_process "rollball-runtime" "Runtime"
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
# Auto-detect the best ORT linking strategy for the current system:
#   1. ORT_LIB_LOCATION already set in env       → dynamic link (user override)
#   2. .ort_env file exists                      → source it, dynamic link
#   3. .ort/ directory has a valid ORT install   → auto-detect, dynamic link
#   4. macOS / Windows                           → download-binaries (always works)
#   5. Linux glibc >= 2.38                       → download-binaries
#   6. Linux glibc <  2.38                       → auto-run setup_ort.sh, then dynamic link
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

# ── resolve_ort_strategy: detect glibc, probe local ORT, decide strategy ──
detect_glibc_score() {
    local glibc_ver
    glibc_ver="$(ldd --version 2>&1 | head -1 | grep -oP '\d+\.\d+$' || true)"
    if [ -z "$glibc_ver" ]; then
        glibc_ver="$(ldd --version 2>&1 | head -1 | grep -oP '(\d+\.\d+)' | head -1 || true)"
    fi
    if [ -n "$glibc_ver" ]; then
        local major minor
        major="$(echo "$glibc_ver" | cut -d. -f1)"
        minor="$(echo "$glibc_ver" | cut -d. -f2)"
        echo $(( major * 100 + minor ))
    else
        echo "0"
    fi
}

probe_local_ort() {
    local ort_dir
    for ort_dir in "$WORKSPACE_ROOT"/.ort/onnxruntime-*; do
        [ -d "$ort_dir" ] || continue
        if [ -f "$ort_dir/lib/libonnxruntime.so" ] || [ -f "$ort_dir/lib/libonnxruntime.dylib" ]; then
            echo "$ort_dir/lib"
            return 0
        fi
    done
    return 1
}

EMBED_FEATURES=""
ORT_STRATEGY=""

# Priority 1: ORT_LIB_LOCATION already set in environment
if [ -n "$ORT_LIB_LOCATION" ]; then
    ORT_STRATEGY="local"

# Priority 2: .ort_env file exists (generated by setup_ort.sh)
elif [ -f "$WORKSPACE_ROOT/.ort_env" ]; then
    source "$WORKSPACE_ROOT/.ort_env"
    if [ -n "$ORT_LIB_LOCATION" ]; then
        ORT_STRATEGY="local"
    fi

# Priority 3: .ort/ directory has a valid ORT installation
fi
if [ -z "$ORT_STRATEGY" ]; then
    if LOCAL_ORT_LIB="$(probe_local_ort)"; then
        export ORT_LIB_LOCATION="$LOCAL_ORT_LIB"
        if [ -f "$LOCAL_ORT_LIB/libonnxruntime.so" ]; then
            export ORT_DYLIB_PATH="$LOCAL_ORT_LIB/libonnxruntime.so"
        elif [ -f "$LOCAL_ORT_LIB/libonnxruntime.dylib" ]; then
            export ORT_DYLIB_PATH="$LOCAL_ORT_LIB/libonnxruntime.dylib"
        fi
        ORT_STRATEGY="local"
    fi
fi

# Priority 4/5/6: platform-based decision
if [ -z "$ORT_STRATEGY" ]; then
    case "$OS" in
        macos|windows)
            ORT_STRATEGY="download"
            ;;
        linux)
            GLIBC_SCORE="$(detect_glibc_score)"
            if [ "$GLIBC_SCORE" -ge 238 ]; then
                ORT_STRATEGY="download"
            else
                echo -e "${YELLOW}  glibc < 2.38 detected — auto-installing compatible ONNX Runtime...${NC}"
                if bash "$WORKSPACE_ROOT/dev/setup_ort.sh" 2>&1; then
                    source "$WORKSPACE_ROOT/.ort_env"
                    ORT_STRATEGY="local"
                else
                    echo -e "${RED}  Failed to auto-install ONNX Runtime.${NC}"
                    echo -e "${RED}  Run manually: ./dev/setup_ort.sh${NC}"
                    exit 1
                fi
            fi
            ;;
        *)
            ORT_STRATEGY="download"
            ;;
    esac
fi

# Apply strategy
if [ "$ORT_STRATEGY" = "local" ]; then
    export ORT_PREFER_DYNAMIC_LINK=1
    EMBED_FEATURES=""
    echo -e "${YELLOW}[3.5/5] Building Embedding Runtime (local ORT: $ORT_LIB_LOCATION)...${NC}"
else
    EMBED_FEATURES="--features download-ort"
    echo -e "${YELLOW}[3.5/5] Building Embedding Runtime (download prebuilt ORT)...${NC}"
fi

if cargo build --release -p rollball-embed $EMBED_FEATURES 2>&1 | tee /tmp/embed_build.log; then
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
