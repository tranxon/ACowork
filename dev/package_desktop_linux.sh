#!/usr/bin/env bash
# package_desktop_linux.sh - Build ACowork Desktop package for Linux

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
CYAN='\033[0;36m'
NC='\033[0m'

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(dirname "$SCRIPT_DIR")"
DESKTOP_DIR="$WORKSPACE_ROOT/apps/acowork-desktop"
BIN_DIR="$DESKTOP_DIR/src-tauri/bin"

if [ "$(uname -s)" != "Linux" ]; then
    echo -e "${RED}This script is for Linux only.${NC}"
    exit 1
fi

echo -e "${CYAN}========================================${NC}"
echo -e "${CYAN}ACowork Desktop Package (Linux)${NC}"
echo -e "${CYAN}========================================${NC}"
echo ""

find_ort_lib() {
    local found=""
    for ort_dir in "$WORKSPACE_ROOT"/.ort/onnxruntime-linux-*; do
        [ -d "$ort_dir" ] || continue
        if [ -f "$ort_dir/lib/libonnxruntime.so" ]; then
            found="$ort_dir/lib/libonnxruntime.so"
            break
        fi
    done
    echo "$found"
}

ORT_LIB="$(find_ort_lib)"
if [ -z "$ORT_LIB" ]; then
    "$SCRIPT_DIR/setup_ort.sh"
    ORT_LIB="$(find_ort_lib)"
fi

if [ -z "$ORT_LIB" ]; then
    echo -e "${RED}ONNX Runtime library not found. Run ./dev/setup_ort.sh first.${NC}"
    exit 1
fi

export ORT_LIB_LOCATION="$(dirname "$ORT_LIB")"
export ORT_DYLIB_PATH="$ORT_LIB"
export ORT_PREFER_DYNAMIC_LINK=1
export LD_LIBRARY_PATH="$ORT_LIB_LOCATION:${LD_LIBRARY_PATH:-}"

mkdir -p "$BIN_DIR"
cp "$ORT_LIB" "$BIN_DIR/libonnxruntime.so"
echo -e "${GREEN}Bundled ORT library: $ORT_LIB${NC}"

# Bundle LSP Relay binary (sibling of acowork-gateway, ADR-019).
# The Gateway locates it via `current_exe().parent().join("acowork-lsp-relay")`,
# so without this copy the Tauri app's Gateway supervisor will fail to spawn LSP
# and Monaco / runtime codebase tool will silently lose all LSP features.
LSP_RELAY_BIN="$WORKSPACE_ROOT/target/release/acowork-lsp-relay"
if [ -f "$LSP_RELAY_BIN" ]; then
    cp "$LSP_RELAY_BIN" "$BIN_DIR/acowork-lsp-relay"
    echo -e "${GREEN}Bundled LSP Relay binary: $LSP_RELAY_BIN${NC}"
else
    echo -e "${YELLOW}WARN: acowork-lsp-relay not found at $LSP_RELAY_BIN.${NC}"
    echo -e "${YELLOW}      Run ./dev/build_core.sh (release) first.${NC}"
    echo -e "${YELLOW}      Without it, Gateway startup will fail with:${NC}"
    echo -e "${YELLOW}        acowork-lsp-relay binary not found${NC}"
fi

cd "$DESKTOP_DIR"
npm run tauri build
