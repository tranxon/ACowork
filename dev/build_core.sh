#!/usr/bin/env bash
# build_core.sh - Cross-platform rebuild (and optional start) Gateway + Runtime
# Usage: ./dev/build_core.sh [OPTIONS]
#   (no args)                Build release (default) + start Gateway
#   --debug                  Build debug + start Gateway
#   --release                Build release + start Gateway (explicit, default)
#   --no-start               Build only, do not start Gateway
#   --skip-embed             Skip building the embedding runtime
#   -h, --help               Show this help
#
# Profile selection: --debug/--release flag > $ACOWORK_BUILD_PROFILE > release
# In debug profile, $ACOWORK_GATEWAY_LOG_LEVEL is auto-exported to "debug" so
# any gateway process spawned from this script inherits verbose logging.
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

# Parse arguments
PROFILE="release"
START_GATEWAY=true
SKIP_EMBED=false
for arg in "$@"; do
    case "$arg" in
        --debug)      PROFILE="debug" ;;
        --release)    PROFILE="release" ;;
        --no-start)   START_GATEWAY=false ;;
        --skip-embed) SKIP_EMBED=true ;;
        -h|--help)
            cat <<'EOF'
Usage: ./dev/build_core.sh [OPTIONS]

Options:
  --debug           Build debug (auto-enables ACOWORK_GATEWAY_LOG_LEVEL=debug)
  --release         Build release (default)
  --no-start        Build only, do not start Gateway
  --skip-embed      Skip the embedding runtime build
  -h, --help        Show this help

Environment:
  ACOWORK_BUILD_PROFILE   debug|release  (overridden by --debug/--release)

Default behavior: build release, then start the gateway daemon in background.
EOF
            exit 0
            ;;
        *) echo -e "${RED}Unknown option: $arg${NC}"; exit 1 ;;
    esac
done

# Env var fallback for profile (CLI flag wins).
if [ -n "$ACOWORK_BUILD_PROFILE" ]; then
    env_profile="$(echo "$ACOWORK_BUILD_PROFILE" | tr '[:upper:]' '[:lower:]' | xargs)"
    case "$env_profile" in
        debug|release) PROFILE="$env_profile" ;;
        *) echo -e "${YELLOW}WARN: ignoring unknown ACOWORK_BUILD_PROFILE='$env_profile' (expected 'debug' or 'release')${NC}" ;;
    esac
fi

# Runtime env linkage: debug profile auto-enables gateway verbose logging for
# any child process spawned from this script.
if [ "$PROFILE" = "debug" ]; then
    export ACOWORK_GATEWAY_LOG_LEVEL="debug"
fi

TARGET_DIR="$WORKSPACE_ROOT/target/$PROFILE"

echo -e "${CYAN}========================================${NC}"
echo -e "${CYAN}ACowork Core Rebuild & Restart Script${NC}"
echo -e "${CYAN}OS: $OS   Profile: $PROFILE${NC}"
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

# Step 1: Stop running processes (only when we are about to start a new one)
if [ "$START_GATEWAY" = "true" ]; then
    echo -e "${YELLOW}[1/5] Stopping running Gateway, Runtime, Embed, and LSP Relay processes...${NC}"
    stop_process "acowork-gateway" "Gateway"
    stop_process "acowork-runtime" "Runtime"
    stop_process "acowork-embed"  "Embed"
    # The LSP Relay runs in its own process group (see
    # core/acowork-gateway/src/lifecycle/lsp_relay.rs: cmd.process_group(0)),
    # so a Gateway shutdown does NOT cascade a SIGHUP/SIGTERM to it — we must
    # explicitly kill it to avoid leaving an orphan binding port 19878, which
    # would otherwise be attached by the new gateway via
    # attach_existing_lsp_relay() but owned by a now-dead parent.
    stop_process "acowork-lsp-relay" "LSP Relay"

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

        # Free LSP Relay port 19878 (see note above about process_group(0)).
        # Independent counter so embed-port wait doesn't pre-empt the relay-port wait.
        if command -v fuser &>/dev/null; then
            fuser -k 19878/tcp 2>/dev/null || true
        fi
        lsp_waited=0
        while command -v ss &>/dev/null && ss -tlnp 2>/dev/null | grep -q ":19878 "; do
            sleep 0.5
            lsp_waited=$((lsp_waited + 1))
            if [ $lsp_waited -ge 6 ]; then
                echo -e "${RED}  WARNING: Port 19878 still in use after 3s${NC}"
                break
            fi
        done
    fi
    echo ""
fi

# Step 2: Build Gateway
echo -e "${YELLOW}[2/5] Building Gateway ($PROFILE mode)...${NC}"
cd "$CORE_DIR"
if [ "$PROFILE" = "release" ]; then
    cargo_args=(cargo build --release -p acowork-gateway)
else
    cargo_args=(cargo build -p acowork-gateway)
fi
if "${cargo_args[@]}" 2>&1 | tee /tmp/gateway_build.log; then
    if grep -q "error\[" /tmp/gateway_build.log 2>/dev/null; then
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
echo -e "${YELLOW}[3/5] Building Runtime ($PROFILE mode)...${NC}"
if [ "$PROFILE" = "release" ]; then
    cargo_args=(cargo build --release -p acowork-runtime)
else
    cargo_args=(cargo build -p acowork-runtime)
fi
if "${cargo_args[@]}" 2>&1 | tee /tmp/runtime_build.log; then
    if grep -q "error\[" /tmp/runtime_build.log 2>/dev/null; then
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

if [ "$SKIP_EMBED" = "true" ]; then
    echo -e "${YELLOW}[3.5/5] Skipping Embedding Runtime (--skip-embed).${NC}"
else
    echo -e "${YELLOW}[3.5/5] Building Embedding Runtime ($PROFILE mode)...${NC}"

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
        for cached_ort in $(find ~/.cargo/registry/cache -maxdepth 5 -type d \( -name "onnxruntime-osx-aarch64-*" -o -name "onnxruntime-osx-x64-*" \) 2>/dev/null); do
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
        if [ "$PROFILE" = "release" ]; then
            echo -e "${RED}  Alternative: cargo build --release -p acowork-embed --features download-ort${NC}"
        else
            echo -e "${RED}  Alternative: cargo build -p acowork-embed --features download-ort${NC}"
        fi
        exit 1
    fi

    if [ "$PROFILE" = "release" ]; then
        cargo_args=(cargo build --release -p acowork-embed)
    else
        cargo_args=(cargo build -p acowork-embed)
    fi
    if "${cargo_args[@]}" 2>&1 | tee /tmp/embed_build.log; then
        if grep -q "error\[" /tmp/embed_build.log 2>/dev/null; then
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

# Step 3.6: Build LSP Relay (standalone binary, sibling of acowork-gateway)
#
# The Gateway spawns `acowork-lsp-relay` as a sibling process (ADR-019). It
# locates the binary via `current_exe().parent().join(...)` — see
# core/acowork-gateway/src/lifecycle/lsp_relay.rs::spawn_lsp_relay. If the
# sibling binary is missing, Gateway startup fails with:
#   GatewayError::Lifecycle("acowork-lsp-relay binary not found at ...")
#
# We unconditionally build (no --skip-lsp-relay flag) because every Gateway
# needs an LSP Relay process to serve the runtime codebase tool and the
# desktop Monaco client.
echo -e "${YELLOW}[3.6/5] Building LSP Relay ($PROFILE mode)...${NC}"
if [ "$PROFILE" = "release" ]; then
    cargo_args=(cargo build --release -p acowork-lsp-relay)
else
    cargo_args=(cargo build -p acowork-lsp-relay)
fi
if "${cargo_args[@]}" 2>&1 | tee /tmp/lsp_relay_build.log; then
    if grep -q "error\[" /tmp/lsp_relay_build.log 2>/dev/null; then
        echo -e "${RED}  LSP Relay build failed with errors.${NC}"
        exit 1
    fi
    echo -e "${GREEN}  LSP Relay build completed.${NC}"
else
    echo -e "${RED}  LSP Relay build failed.${NC}"
    exit 1
fi
rm -f /tmp/lsp_relay_build.log
echo ""

# Step 4: Copy offline_providers.json from assets to target dir
#
# The gateway (and embed) read this from `{exe_dir}/offline_providers.json`.
# Whoever distributes the binary (this script for dev, the package installer
# for release, the Tauri bundler for desktop) is responsible for placing it
# there.
#
# We only stage into the directory matching the active profile — the previous
# "stage to both target/release and target/debug" pattern required `mkdir -p`
# to avoid the silent stray-file behavior of `cp` (and the silent wrong-target
# behavior of PowerShell `Copy-Item`).
echo -e "${YELLOW}[4/5] Copying offline_providers.json to target/$PROFILE...${NC}"
OFFLINE_SRC="$WORKSPACE_ROOT/assets/offline_providers.json"
mkdir -p "$TARGET_DIR"
if [ -f "$OFFLINE_SRC" ]; then
    cp "$OFFLINE_SRC" "$TARGET_DIR/"
    echo -e "${GREEN}  Copied to $TARGET_DIR${NC}"
else
    echo -e "${RED}  WARNING: offline_providers.json not found at $OFFLINE_SRC${NC}"
fi

# Step 4.5: Copy embedding_models.json next to the gateway + embed binaries
#
# The gateway (and embed) read this from `{exe_dir}/embedding_models.json`.
# Whoever distributes the binary (this script for dev, the package installer
# for release, the Tauri bundler for desktop) is responsible for placing it
# there. Source of truth is core/acowork-embed/assets/embedding_models.json.
echo -e "${YELLOW}[4.5/5] Copying embedding_models.json to target/$PROFILE...${NC}"
EMBED_MODELS_SRC="$WORKSPACE_ROOT/core/acowork-embed/assets/embedding_models.json"
if [ -f "$EMBED_MODELS_SRC" ]; then
    cp "$EMBED_MODELS_SRC" "$TARGET_DIR/embedding_models.json"
    echo -e "${GREEN}  Copied to $TARGET_DIR${NC}"
else
    echo -e "${RED}  WARNING: embedding_models.json not found at $EMBED_MODELS_SRC${NC}"
fi

echo ""

# Step 5: Start Gateway (only when not --no-start)
if [ "$START_GATEWAY" = "true" ]; then
    log_level="${ACOWORK_GATEWAY_LOG_LEVEL:-info}"
    echo -e "${YELLOW}[5/5] Starting Gateway in daemon mode (log level: $log_level)...${NC}"
    export ACOWORK_GATEWAY_DAEMON="true"

    GATEWAY_EXE=""
    if [ "$OS" = "windows" ]; then
        GATEWAY_EXE="$TARGET_DIR/acowork-gateway.exe"
    else
        GATEWAY_EXE="$TARGET_DIR/acowork-gateway"
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
else
    echo ""
    echo -e "${CYAN}========================================${NC}"
    echo -e "${CYAN}Build complete (not started, --no-start).${NC}"
    echo -e "${CYAN}To start: $TARGET_DIR/acowork-gateway${NC}"
    echo -e "${CYAN}========================================${NC}"
fi

# Return to workspace root
cd "$WORKSPACE_ROOT"

# Cleanup temp files
rm -f /tmp/gateway_build.log /tmp/runtime_build.log
