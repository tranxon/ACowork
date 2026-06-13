#!/usr/bin/env bash
# setup_ort.sh - Auto-detect system and install compatible ONNX Runtime
#
# Usage:
#   source dev/setup_ort.sh           # Detect, install, export env vars
#   ./dev/setup_ort.sh                # Detect, install, generate env file only
#   ./dev/setup_ort.sh --reinstall    # Force re-download even if already installed
#   ./dev/setup_ort.sh --version 1.21.0  # Override ORT version
#   ./dev/setup_ort.sh --no-mirror    # Skip China mirrors, use GitHub directly
#
# After running, either:
#   source .ort_env                    # Load env vars into current shell
#   # or the build_core.sh will auto-detect ORT_LIB_LOCATION
#
# Supported platforms:
#   Linux  - glibc auto-detection, selects compatible ORT version
#   macOS  - uses latest ORT
#   Windows (Git Bash/WSL) - not recommended, use setup_ort.ps1 instead

set -e

# ── Colors ──────────────────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
GRAY='\033[0;37m'
BOLD='\033[1m'
NC='\033[0m'

# ── Paths ───────────────────────────────────────────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(dirname "$SCRIPT_DIR")"
CORE_DIR="$WORKSPACE_ROOT/core"

# ── Defaults ────────────────────────────────────────────────────────────────
# Default ORT versions per glibc range
ORT_VERSION_MODERN="1.22.0"      # glibc >= 2.38 (Ubuntu 23.10+, Fedora 39+)
ORT_VERSION_UBUNTU2204="1.21.0"  # glibc 2.35  (Ubuntu 22.04)
ORT_VERSION_LEGACY="1.19.2"      # glibc 2.31  (Ubuntu 20.04)
ORT_VERSION=""                   # Will be auto-detected
FORCE_REINSTALL=false
CUSTOM_VERSION=""
NO_MIRROR=false

# ── Parse arguments ─────────────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
    case "$1" in
        --reinstall|--force)
            FORCE_REINSTALL=true
            shift
            ;;
        --version)
            CUSTOM_VERSION="$2"
            shift 2
            ;;
        --version=*)
            CUSTOM_VERSION="${1#*=}"
            shift
            ;;
        --no-mirror)
            NO_MIRROR=true
            shift
            ;;
        -h|--help)
            head -17 "$0" | tail -15
            exit 0
            ;;
        *)
            echo -e "${RED}Unknown option: $1${NC}"
            echo "Use --help for usage information."
            exit 1
            ;;
    esac
done

# ── Detect OS ───────────────────────────────────────────────────────────────
OS="unknown"
ARCH="$(uname -m)"
case "$(uname -s)" in
    Linux*)     OS="linux";;
    Darwin*)    OS="macos";;
    CYGWIN*|MINGW*|MSYS*) OS="windows";;
    *)          OS="unknown";;
esac

# Normalize architecture
case "$ARCH" in
    x86_64|amd64)  ARCH="x64";;
    aarch64|arm64) ARCH="aarch64";;
    *)
        echo -e "${RED}Unsupported architecture: $ARCH${NC}"
        exit 1
        ;;
esac

echo -e "${CYAN}╔══════════════════════════════════════════════╗${NC}"
echo -e "${CYAN}║   ONNX Runtime Setup for RollBall.AI        ║${NC}"
echo -e "${CYAN}╚══════════════════════════════════════════════╝${NC}"
echo ""
echo -e "  OS:   ${BOLD}$OS${NC}"
echo -e "  Arch: ${BOLD}$ARCH${NC}"

# ── Check if we need ORT at all ─────────────────────────────────────────────
if [ "$OS" = "windows" ]; then
    echo ""
    echo -e "${YELLOW}On Windows, use the PowerShell script instead:${NC}"
    echo -e "  ${CYAN}.\\dev\\setup_ort.ps1${NC}"
    echo ""
    echo -e "Or simply use download-ort feature (no manual setup needed):"
    echo -e "  ${CYAN}cargo build --release -p rollball-embed --features download-ort${NC}"
    exit 0
fi

# ── Detect glibc version (Linux only) ───────────────────────────────────────
GLIBC_MAJOR=0
GLIBC_MINOR=0
GLIBC_VERSION=""

if [ "$OS" = "linux" ]; then
    GLIBC_VERSION="$(ldd --version 2>&1 | head -1 | grep -oP '\d+\.\d+$' || true)"

    if [ -z "$GLIBC_VERSION" ]; then
        # Fallback: try parsing "ldd (GNU libc) X.YZ"
        GLIBC_VERSION="$(ldd --version 2>&1 | head -1 | grep -oP '(\d+\.\d+)' | head -1 || true)"
    fi

    if [ -n "$GLIBC_VERSION" ]; then
        GLIBC_MAJOR="$(echo "$GLIBC_VERSION" | cut -d. -f1)"
        GLIBC_MINOR="$(echo "$GLIBC_VERSION" | cut -d. -f2)"
        echo -e "  glibc: ${BOLD}$GLIBC_VERSION${NC}"
    else
        echo -e "  glibc: ${YELLOW}could not detect (assuming >= 2.38)${NC}"
        GLIBC_MAJOR=2
        GLIBC_MINOR=38
    fi
elif [ "$OS" = "macos" ]; then
    echo -e "  macOS: ${BOLD}$(sw_vers -productVersion 2>/dev/null || echo 'unknown')${NC}"
fi

echo ""

# ── Determine ORT version ───────────────────────────────────────────────────
if [ -n "$CUSTOM_VERSION" ]; then
    ORT_VERSION="$CUSTOM_VERSION"
    echo -e "${CYAN}Using user-specified ORT version: $ORT_VERSION${NC}"
elif [ "$OS" = "macos" ]; then
    ORT_VERSION="$ORT_VERSION_MODERN"
    echo -e "${CYAN}macOS detected, using ORT $ORT_VERSION${NC}"
elif [ "$OS" = "linux" ]; then
    # glibc version comparison
    # glibc 2.38+ → modern ORT (Ubuntu 23.10+, Fedora 39+, Debian 13+)
    # glibc 2.35  → ORT 1.21.x (Ubuntu 22.04)
    # glibc 2.31  → ORT 1.19.x (Ubuntu 20.04)
    # glibc < 2.31 → unsupported
    GLIBC_SCORE=$(( GLIBC_MAJOR * 100 + GLIBC_MINOR ))

    if [ "$GLIBC_SCORE" -ge 238 ]; then
        ORT_VERSION="$ORT_VERSION_MODERN"
        echo -e "${GREEN}glibc $GLIBC_VERSION >= 2.38 — modern system${NC}"
        echo -e "${CYAN}Using ORT $ORT_VERSION (compatible)${NC}"
        echo ""
        echo -e "${GRAY}Tip: Your system supports download-ort, you can also just run:${NC}"
        echo -e "${GRAY}  cargo build --release -p rollball-embed --features download-ort${NC}"
    elif [ "$GLIBC_SCORE" -ge 235 ]; then
        ORT_VERSION="$ORT_VERSION_UBUNTU2204"
        echo -e "${YELLOW}glibc $GLIBC_VERSION < 2.38 — older system (e.g. Ubuntu 22.04)${NC}"
        echo -e "${CYAN}Using ORT $ORT_VERSION (last version compatible with glibc $GLIBC_VERSION)${NC}"
    elif [ "$GLIBC_SCORE" -ge 231 ]; then
        ORT_VERSION="$ORT_VERSION_LEGACY"
        echo -e "${YELLOW}glibc $GLIBC_VERSION < 2.35 — legacy system (e.g. Ubuntu 20.04)${NC}"
        echo -e "${CYAN}Using ORT $ORT_VERSION (compatible with glibc $GLIBC_VERSION)${NC}"
    else
        echo -e "${RED}glibc $GLIBC_VERSION is too old. Minimum required: 2.31 (Ubuntu 20.04).${NC}"
        echo -e "${RED}Please upgrade your OS.${NC}"
        exit 1
    fi
fi

echo ""

# ── Determine platform suffix and download URL ──────────────────────────────
if [ "$OS" = "linux" ]; then
    ORT_PLATFORM="linux"
    ORT_LIB_NAME="libonnxruntime.so"
    ORT_ARCHIVE_EXT="tgz"
elif [ "$OS" = "macos" ]; then
    ORT_PLATFORM="osx"
    ORT_LIB_NAME="libonnxruntime.dylib"
    ORT_ARCHIVE_EXT="tgz"
fi

ORT_ARCHIVE="onnxruntime-${ORT_PLATFORM}-${ARCH}-${ORT_VERSION}"
ORT_GITHUB_URL="https://github.com/microsoft/onnxruntime/releases/download/v${ORT_VERSION}/${ORT_ARCHIVE}.${ORT_ARCHIVE_EXT}"

# ── GitHub mirror proxies (China mainland acceleration) ──────────────────────
# Tries multiple mirrors concurrently and uses the fastest one.
# Set ORT_NO_MIRROR=1 to skip mirrors and use GitHub directly.
ORT_MIRROR_PREFIXES=(
    "https://ghfast.top/"
    "https://gh-proxy.com/"
    "https://mirror.ghproxy.com/"
    "https://ghproxy.net/"
)

# Build ordered URL list: mirrors first (unless disabled), then direct GitHub
ORT_URLS=()
if [ "${ORT_NO_MIRROR:-0}" != "1" ] && [ "$NO_MIRROR" = "false" ] && [ "$OS" = "linux" ]; then
    for prefix in "${ORT_MIRROR_PREFIXES[@]}"; do
        ORT_URLS+=("${prefix}${ORT_GITHUB_URL}")
    done
fi
ORT_URLS+=("$ORT_GITHUB_URL")

# ── Install directory ───────────────────────────────────────────────────────
ORT_INSTALL_DIR="$WORKSPACE_ROOT/.ort"
ORT_EXTRACTED_DIR="$ORT_INSTALL_DIR/onnxruntime-${ORT_PLATFORM}-${ARCH}-${ORT_VERSION}"

# ── Check if already installed ──────────────────────────────────────────────
if [ -f "$ORT_EXTRACTED_DIR/lib/$ORT_LIB_NAME" ] && [ "$FORCE_REINSTALL" = "false" ]; then
    echo -e "${GREEN}ORT $ORT_VERSION already installed at:${NC}"
    echo -e "  $ORT_EXTRACTED_DIR"
    echo ""
    # Still export env vars and generate env file
    export ORT_LIB_LOCATION="$ORT_EXTRACTED_DIR/lib"
    export ORT_DYLIB_PATH="$ORT_EXTRACTED_DIR/lib/$ORT_LIB_NAME"
else
    # ── Download (with mirror fallback) ──────────────────────────────────────
    echo -e "${YELLOW}[1/4] Downloading ONNX Runtime $ORT_VERSION...${NC}"

    DOWNLOAD_OK=false
    TMP_FILE="/tmp/${ORT_ARCHIVE}.${ORT_ARCHIVE_EXT}"

    if ! command -v curl &>/dev/null && ! command -v wget &>/dev/null; then
        echo -e "${RED}Neither curl nor wget found. Please install one of them.${NC}"
        exit 1
    fi

    # Race: try all mirrors concurrently, first successful download wins
    if [ "${#ORT_URLS[@]}" -gt 1 ]; then
        echo -e "  Trying ${#ORT_URLS[@]} sources concurrently..."

        # Disable errexit for background process management
        # (kill/wait return non-zero when processes fail)
        set +e

        PIDS=()
        URLS_TRIED=()
        for url in "${ORT_URLS[@]}"; do
            local_tmp="${TMP_FILE}.$$.$RANDOM"
            URLS_TRIED+=("$local_tmp")
            if command -v curl &>/dev/null; then
                ( curl -fSL --connect-timeout 10 --max-time 600 -o "$local_tmp" "$url" 2>/dev/null && echo "OK" > "${local_tmp}.status" || echo "FAIL" > "${local_tmp}.status" ) &
            else
                ( wget -q --timeout=10 -O "$local_tmp" "$url" 2>/dev/null && echo "OK" > "${local_tmp}.status" || echo "FAIL" > "${local_tmp}.status" ) &
            fi
            PIDS+=($!)
        done

        # Poll: wait for first success or all to finish
        WINNER=""
        CHECKED=()
        for _ in "${PIDS[@]}"; do CHECKED+=("0"); done

        while true; do
            ALL_DONE=true
            for i in "${!PIDS[@]}"; do
                [ "${CHECKED[$i]}" = "1" ] && continue
                # Check if process has finished
                kill -0 "${PIDS[$i]}" 2>/dev/null
                if [ $? -ne 0 ]; then
                    CHECKED[$i]="1"
                    wait "${PIDS[$i]}" 2>/dev/null
                    STATUS_FILE="${URLS_TRIED[$i]}.status"
                    if [ -f "$STATUS_FILE" ] && grep -q "^OK$" "$STATUS_FILE" 2>/dev/null; then
                        WINNER="${URLS_TRIED[$i]}"
                        # Kill remaining downloads
                        for j in "${!PIDS[@]}"; do
                            [ "$j" != "$i" ] && kill "${PIDS[$j]}" 2>/dev/null
                        done
                        break 2
                    fi
                else
                    ALL_DONE=false
                fi
            done
            if $ALL_DONE; then
                break
            fi
            sleep 0.5
        done

        # Collect any remaining processes
        for i in "${!PIDS[@]}"; do
            wait "${PIDS[$i]}" 2>/dev/null
            if [ -z "$WINNER" ]; then
                STATUS_FILE="${URLS_TRIED[$i]}.status"
                if [ -f "$STATUS_FILE" ] && grep -q "^OK$" "$STATUS_FILE" 2>/dev/null; then
                    WINNER="${URLS_TRIED[$i]}"
                fi
            fi
        done

        # Re-enable errexit
        set -e

        if [ -n "$WINNER" ] && [ -f "$WINNER" ]; then
            mv "$WINNER" "$TMP_FILE"
            DOWNLOAD_OK=true
        fi

        # Cleanup temp files
        for tmp in "${URLS_TRIED[@]}"; do
            rm -f "$tmp" "${tmp}.status"
        done
    else
        # Single URL (direct GitHub or mirror disabled)
        echo -e "  URL: ${GRAY}${ORT_URLS[0]}${NC}"
        if command -v curl &>/dev/null; then
            curl -fSL --progress-bar -o "$TMP_FILE" "${ORT_URLS[0]}" && DOWNLOAD_OK=true
        else
            wget -q --show-progress -O "$TMP_FILE" "${ORT_URLS[0]}" && DOWNLOAD_OK=true
        fi
    fi

    if [ "$DOWNLOAD_OK" = "false" ]; then
        echo -e "${RED}  All download sources failed.${NC}"
        echo -e "${RED}  Direct URL: $ORT_GITHUB_URL${NC}"
        echo -e "${YELLOW}  Try downloading manually and placing the archive in:${NC}"
        echo -e "    /tmp/${ORT_ARCHIVE}.${ORT_ARCHIVE_EXT}"
        echo -e "${YELLOW}  Then re-run this script.${NC}"
        exit 1
    fi

    echo -e "${GREEN}  Download complete.${NC}"
    echo ""

    # ── Extract ──────────────────────────────────────────────────────────────
    echo -e "${YELLOW}[2/4] Extracting...${NC}"
    mkdir -p "$ORT_INSTALL_DIR"

    # Remove old version if force reinstall
    if [ "$FORCE_REINSTALL" = "true" ] && [ -d "$ORT_INSTALL_DIR" ]; then
        rm -rf "$ORT_INSTALL_DIR"/*
    fi

    tar xzf "/tmp/${ORT_ARCHIVE}.${ORT_ARCHIVE_EXT}" -C "$ORT_INSTALL_DIR"
    rm -f "/tmp/${ORT_ARCHIVE}.${ORT_ARCHIVE_EXT}"

    if [ ! -d "$ORT_EXTRACTED_DIR" ]; then
        echo -e "${RED}Extraction failed. Expected directory not found:${NC}"
        echo -e "  $ORT_EXTRACTED_DIR"
        echo -e "${RED}Contents of $ORT_INSTALL_DIR:${NC}"
        ls -la "$ORT_INSTALL_DIR"
        exit 1
    fi

    echo -e "${GREEN}  Extracted to: $ORT_EXTRACTED_DIR${NC}"
    echo ""

    # ── Verify ───────────────────────────────────────────────────────────────
    echo -e "${YELLOW}[3/4] Verifying installation...${NC}"
    if [ -f "$ORT_EXTRACTED_DIR/lib/$ORT_LIB_NAME" ]; then
        LIB_SIZE=$(du -sh "$ORT_EXTRACTED_DIR/lib/$ORT_LIB_NAME" 2>/dev/null | cut -f1)
        echo -e "${GREEN}  Library: $ORT_LIB_NAME ($LIB_SIZE)${NC}"
    else
        echo -e "${RED}  Library not found: $ORT_EXTRACTED_DIR/lib/$ORT_LIB_NAME${NC}"
        echo -e "${RED}  Directory contents:${NC}"
        find "$ORT_EXTRACTED_DIR" -maxdepth 3 -type f | head -20
        exit 1
    fi

    if [ -d "$ORT_EXTRACTED_DIR/include" ]; then
        HEADER_COUNT=$(find "$ORT_EXTRACTED_DIR/include" -name "*.h" | wc -l)
        echo -e "${GREEN}  Headers: $HEADER_COUNT files${NC}"
    fi
    echo ""

    # ── Set environment variables ────────────────────────────────────────────
    export ORT_LIB_LOCATION="$ORT_EXTRACTED_DIR/lib"
    export ORT_DYLIB_PATH="$ORT_EXTRACTED_DIR/lib/$ORT_LIB_NAME"

    # ── Clean cached downloads ───────────────────────────────────────────────
    echo -e "${YELLOW}[4/4] Cleaning cached ORT downloads...${NC}"
    if [ -d "$HOME/.cache/ort.pyke.io" ]; then
        rm -rf "$HOME/.cache/ort.pyke.io"
        echo -e "${GREEN}  Removed ~/.cache/ort.pyke.io${NC}"
    else
        echo -e "${GRAY}  No cache to clean.${NC}"
    fi
    echo ""
fi

# ── Generate env file ───────────────────────────────────────────────────────
ENV_FILE="$WORKSPACE_ROOT/.ort_env"
cat > "$ENV_FILE" << ENVEOF
# ONNX Runtime environment variables
# Generated by dev/setup_ort.sh on $(date -Iseconds)
# Source this file before building:
#   source .ort_env
#
# ORT version: $ORT_VERSION
# Platform:    $OS ($ARCH)
# glibc:       ${GLIBC_VERSION:-n/a}

export ORT_LIB_LOCATION="$ORT_LIB_LOCATION"
export ORT_DYLIB_PATH="$ORT_DYLIB_PATH"

# Add to LD_LIBRARY_PATH so the runtime linker can find libonnxruntime.so
if [ -d "$ORT_LIB_LOCATION" ]; then
    case ":\${LD_LIBRARY_PATH:-}:" in
        *:"$ORT_LIB_LOCATION":*) ;;
        *) export LD_LIBRARY_PATH="$ORT_LIB_LOCATION:\${LD_LIBRARY_PATH:-}" ;;
    esac
fi
ENVEOF

# ── Summary ─────────────────────────────────────────────────────────────────
echo -e "${CYAN}╔══════════════════════════════════════════════╗${NC}"
echo -e "${CYAN}║   Setup Complete                            ║${NC}"
echo -e "${CYAN}╚══════════════════════════════════════════════╝${NC}"
echo ""
echo -e "  ORT Version : ${BOLD}$ORT_VERSION${NC}"
echo -e "  Install Dir : $ORT_EXTRACTED_DIR"
echo -e "  Library     : $ORT_LIB_LOCATION/$ORT_LIB_NAME"
echo -e "  Env File    : ${CYAN}$ENV_FILE${NC}"
echo ""
echo -e "${BOLD}Next steps:${NC}"
echo ""
echo -e "  ${YELLOW}Option A${NC} — Source env file, then build:"
echo -e "    ${CYAN}source .ort_env${NC}"
echo -e "    ${CYAN}cd core && cargo build --release -p rollball-embed${NC}"
echo ""
echo -e "  ${YELLOW}Option B${NC} — Use build_core.sh (auto-detects ORT_LIB_LOCATION):"
echo -e "    ${CYAN}source .ort_env && ./dev/build_core.sh${NC}"
echo ""
echo -e "  ${YELLOW}Option C${NC} — Add to your shell profile for persistence:"
echo -e "    ${CYAN}echo 'source $WORKSPACE_ROOT/.ort_env' >> ~/.bashrc${NC}"
echo ""
