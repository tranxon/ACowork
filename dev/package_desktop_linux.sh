#!/usr/bin/env bash
# package_desktop_linux.sh - Build AgentCowork Desktop package for Linux

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
echo -e "${CYAN}AgentCowork Desktop Package (Linux)${NC}"
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

cd "$DESKTOP_DIR"
npm run tauri build
