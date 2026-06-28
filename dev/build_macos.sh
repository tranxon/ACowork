#!/usr/bin/env bash
# build_macos.sh - macOS one-click build script
# Solves:
#   1. Skip setup_ort.sh GitHub download (slow on some networks)
#   2. Auto-download Apple Silicon optimized ONNX Runtime via --features download-ort,coreml
#   3. Auto-copy ONNX Runtime from Cargo cache to .ort/
#   4. Auto-configure Homebrew + pkg-config + cmake
#
# Usage:
#   ./dev/build_macos.sh               # Default Apple Silicon optimized, release
#   ./dev/build_macos.sh --debug       # Debug build (auto-enables ACOWORK_GATEWAY_LOG_LEVEL=debug)
#   ./dev/build_macos.sh --release     # Release build (explicit, default)
#   ./dev/build_macos.sh --cpu         # CPU only (best compatibility)
#   ./dev/build_macos.sh --skip-embed  # Skip embed
#   ./dev/build_macos.sh --help
#
# Profile selection: --debug/--release flag > $ACOWORK_BUILD_PROFILE > release

set -e

# ── Colors ──────────────────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
GRAY='\033[0;37m'
NC='\033[0m'

# ── Paths ───────────────────────────────────────────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(dirname "$SCRIPT_DIR")"
CORE_DIR="$WORKSPACE_ROOT/core"

# ── Defaults ────────────────────────────────────────────────────────────────
ARCH="$(uname -m)"
USE_GPU=true         # Apple Silicon auto-enable CoreML
SKIP_EMBED=false
SHOW_HELP=false
PROFILE="release"

# ── Parse arguments ─────────────────────────────────────────────────────────
for arg in "$@"; do
    case "$arg" in
        --debug)      PROFILE="debug" ;;
        --release)    PROFILE="release" ;;
        --cpu)        USE_GPU=false ;;
        --skip-embed) SKIP_EMBED=true ;;
        -h|--help)
            cat << 'EOF'
Usage: ./dev/build_macos.sh [OPTIONS]

Options:
  --debug           Build debug (auto-enables ACOWORK_GATEWAY_LOG_LEVEL=debug)
  --release         Build release (default)
  --cpu             Use CPU-only ONNX Runtime (no CoreML acceleration)
  --skip-embed      Skip building the embedding runtime entirely
  --help, -h        Show this help

Environment:
  ACOWORK_BUILD_PROFILE   debug|release  (overridden by --debug/--release)

Examples:
  ./dev/build_macos.sh               # Apple Silicon + CoreML, release (recommended)
  ./dev/build_macos.sh --debug       # Debug build
  ./dev/build_macos.sh --cpu         # CPU only (Intel Mac or compatibility)
  ./dev/build_macos.sh --skip-embed  # Skip embed, build only Gateway + Runtime
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

# ── Header ──────────────────────────────────────────────────────────────────
echo -e "${CYAN}╔══════════════════════════════════════════════╗${NC}"
echo -e "${CYAN}║   ACowork.AI — macOS One-Click Build Script ║${NC}"
echo -e "${CYAN}╚══════════════════════════════════════════════╝${NC}"
echo ""
echo -e "${GRAY}  Arch: $ARCH${NC}"
echo -e "${GRAY}  CoreML: $($USE_GPU && echo true || echo false)${NC}"
echo -e "${GRAY}  Profile: $PROFILE${NC}"
echo ""

# ── Step 0: Check required tools ────────────────────────────────────────────
echo -e "${YELLOW}[0/6] Checking development tools...${NC}"

# Homebrew
if ! command -v brew &>/dev/null; then
    echo -e "${RED}  ✗ Homebrew is not installed${NC}"
    echo -e "${YELLOW}  Install: /bin/bash -c \"\$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)\"${NC}"
    exit 1
fi
echo -e "${GREEN}  ✓ Homebrew $(brew --version | head -1)${NC}"

# pkg-config
if ! command -v pkg-config &>/dev/null; then
    echo -e "${YELLOW}  ⚠ pkg-config not installed, installing...${NC}"
    brew install pkg-config
fi
echo -e "${GREEN}  ✓ pkg-config $(pkg-config --version)${NC}"

# cmake
if ! command -v cmake &>/dev/null; then
    echo -e "${YELLOW}  ⚠ cmake not installed, installing...${NC}"
    brew install cmake
fi
echo -e "${GREEN}  ✓ cmake $(cmake --version | head -1 | awk '{print $3}')${NC}"

# Rust toolchain
if ! command -v cargo &>/dev/null; then
    echo -e "${RED}  ✗ Rust is not installed${NC}"
    echo -e "${YELLOW}  Install: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh${NC}"
    exit 1
fi
RUST_VER=$(rustc --version | awk '{print $2}')
echo -e "${GREEN}  ✓ Rust $RUST_VER${NC}"

# Check if nightly (project requires)
if ! rustc --version | grep -q nightly; then
    echo -e "${YELLOW}  ⚠ Currently on stable, recommend switching to nightly${NC}"
    echo -e "${GRAY}    rustup default nightly${NC}"
fi

# Node.js
if ! command -v node &>/dev/null; then
    echo -e "${YELLOW}  ⚠ Node.js not installed, recommend using nvm to install 20.x${NC}"
    echo -e "${GRAY}    curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.1/install.sh | bash${NC}"
    echo -e "${GRAY}    nvm install 20 && nvm use 20${NC}"
else
    echo -e "${GREEN}  ✓ Node.js $(node --version)${NC}"
fi

# Cargo mirror (mirror for faster access)
if [ ! -f "$HOME/.cargo/config.toml" ]; then
    echo -e "${YELLOW}  ⚠ Configuring Cargo mirror...${NC}"
    mkdir -p "$HOME/.cargo"
    cat > "$HOME/.cargo/config.toml" << 'EOF'
[source.crates-io]
replace-with = "ustc"

[source.ustc]
registry = "sparse+https://mirrors.ustc.edu.cn/crates.io-index/"

[net]
git-fetch-with-cli = true
EOF
    echo -e "${GREEN}  ✓ Cargo mirror configured (USTC)${NC}"
fi

echo ""

# ── Step 1: Stop old processes ──────────────────────────────────────────────
echo -e "${YELLOW}[1/6] Stopping old processes...${NC}"
# LSP Relay is included because it runs in its own process group (see
# core/acowork-gateway/src/lifecycle/lsp_relay.rs: cmd.process_group(0)),
# so a Gateway shutdown does NOT cascade termination to it — we must kill it
# explicitly to avoid leaving an orphan binding port 19878, which would
# otherwise be attached by the new gateway via attach_existing_lsp_relay()
# but owned by a now-dead parent.
for proc in acowork-gateway acowork-runtime acowork-embed acowork-lsp-relay; do
    pids=$(pgrep -f "$proc" 2>/dev/null || true)
    if [ -n "$pids" ]; then
        pkill -f "$proc" 2>/dev/null || true
        echo -e "${GRAY}  Stopped $proc: $pids${NC}"
    fi
done
# Free embed port
if command -v fuser &>/dev/null; then
    fuser -k 18080/tcp 2>/dev/null || true
fi
# Free LSP Relay port 19878 (see process_group note above).
if command -v fuser &>/dev/null; then
    fuser -k 19878/tcp 2>/dev/null || true
fi
sleep 1
echo -e "${GREEN}  ✓ Process cleanup complete${NC}"
echo ""

# ── Step 2: Build Gateway ───────────────────────────────────────────────────
echo -e "${YELLOW}[2/6] Building Gateway ($PROFILE)...${NC}"
cd "$CORE_DIR"
if [ "$PROFILE" = "release" ]; then
    cargo_args=(cargo build --release -p acowork-gateway)
else
    cargo_args=(cargo build -p acowork-gateway)
fi
if "${cargo_args[@]}" 2>&1 | tail -20; then
    echo -e "${GREEN}  ✓ Gateway compiled successfully${NC}"
else
    echo -e "${RED}  ✗ Gateway compile failed${NC}"
    exit 1
fi
echo ""

# ── Step 3: Build Runtime ───────────────────────────────────────────────────
echo -e "${YELLOW}[3/6] Building Runtime ($PROFILE)...${NC}"
if [ "$PROFILE" = "release" ]; then
    cargo_args=(cargo build --release -p acowork-runtime)
else
    cargo_args=(cargo build -p acowork-runtime)
fi
if "${cargo_args[@]}" 2>&1 | tail -20; then
    echo -e "${GREEN}  ✓ Runtime compiled successfully${NC}"
else
    echo -e "${RED}  ✗ Runtime compile failed${NC}"
    exit 1
fi
echo ""

# ── Step 4: Build Embed ─────────────────────────────────────────────────────
if [ "$SKIP_EMBED" = "true" ]; then
    echo -e "${YELLOW}[4/6] Skipping Embed (--skip-embed)${NC}"
    echo ""
else
    echo -e "${YELLOW}[4/6] Building Embed ($PROFILE, auto-download ONNX Runtime)...${NC}"

    # Determine which feature to use
    EMBED_FEATURES="download-ort"
    if [ "$USE_GPU" = "true" ] && [ "$ARCH" = "arm64" ]; then
        EMBED_FEATURES="download-ort,coreml"
        echo -e "${GRAY}  Using Apple Silicon CoreML acceleration${NC}"
    else
        echo -e "${GRAY}  Using CPU mode${NC}"
    fi

    if [ "$PROFILE" = "release" ]; then
        cargo_args=(cargo build --release -p acowork-embed --features "$EMBED_FEATURES")
    else
        cargo_args=(cargo build -p acowork-embed --features "$EMBED_FEATURES")
    fi
    if "${cargo_args[@]}" 2>&1 | tail -30; then
        echo -e "${GREEN}  ✓ Embed compiled successfully${NC}"
    else
        echo -e "${RED}  ✗ Embed compile failed${NC}"
        exit 1
    fi

    # Step 4.1: Copy downloaded ONNX Runtime to .ort/ (for subsequent scripts)
    echo -e "${YELLOW}  [4.1] Syncing ONNX Runtime to .ort/...${NC}"

    ORT_TARGET_DIR="$WORKSPACE_ROOT/.ort/onnxruntime-osx-aarch64-latest/lib"
    mkdir -p "$ORT_TARGET_DIR"

    # Find ONNX Runtime in Cargo cache
    FOUND_LIB=$(find "$HOME/.cargo/registry/cache" -maxdepth 6 \
        -name "libonnxruntime.dylib" -type f 2>/dev/null | head -1)

    if [ -z "$FOUND_LIB" ]; then
        # Also look for .so or .a
        FOUND_LIB=$(find "$HOME/.cargo/registry/cache" -maxdepth 6 \
            \( -name "libonnxruntime.dylib" -o -name "libonnxruntime.so" \) -type f 2>/dev/null | head -1)
    fi

    if [ -n "$FOUND_LIB" ]; then
        cp "$FOUND_LIB" "$ORT_TARGET_DIR/"
        echo -e "${GREEN}  ✓ Copied to $ORT_TARGET_DIR${NC}"
        echo -e "${GRAY}    Source: $FOUND_LIB${NC}"
    else
        echo -e "${YELLOW}  ⚠ ONNX Runtime not found in Cargo cache, but embed compiled successfully${NC}"
        echo -e "${GRAY}    This is usually fine — cargo statically linked the library into the binary${NC}"
    fi
    echo ""
fi

# ── Step 4.5: Build LSP Relay ───────────────────────────────────────────────
#
# The Gateway spawns `acowork-lsp-relay` as a sibling process (ADR-019). It
# locates the binary via `current_exe().parent().join(...)` — see
# core/acowork-gateway/src/lifecycle/lsp_relay.rs::spawn_lsp_relay. If the
# sibling binary is missing, Gateway startup fails with:
#   GatewayError::Lifecycle("acowork-lsp-relay binary not found at ...")
#
# Unconditional: every Gateway needs an LSP Relay process to serve the
# runtime codebase tool and the desktop Monaco client.
echo -e "${YELLOW}[4.5/6] Building LSP Relay ($PROFILE)...${NC}"
if [ "$PROFILE" = "release" ]; then
    cargo_args=(cargo build --release -p acowork-lsp-relay)
else
    cargo_args=(cargo build -p acowork-lsp-relay)
fi
if "${cargo_args[@]}" 2>&1 | tail -20; then
    echo -e "${GREEN}  ✓ LSP Relay compiled successfully${NC}"
else
    echo -e "${RED}  ✗ LSP Relay compile failed${NC}"
    exit 1
fi
echo ""

# ── Step 5: Copy resource files ─────────────────────────────────────────────
#
# The gateway (and embed) read these from `{exe_dir}/`. We only stage into the
# directory matching the active profile — staging to a directory that does not
# yet exist would either fail (`cp` here, which uses `mkdir -p` below) or, in
# the Windows `Copy-Item` equivalent, silently create a stray file.
echo -e "${YELLOW}[5/6] Copying resource files to $TARGET_DIR...${NC}"
mkdir -p "$TARGET_DIR"
OFFLINE_SRC="$WORKSPACE_ROOT/assets/offline_providers.json"
if [ -f "$OFFLINE_SRC" ]; then
    cp "$OFFLINE_SRC" "$TARGET_DIR/"
    echo -e "${GREEN}  ✓ offline_providers.json${NC}"
fi

# Copy embedding_models.json
EMBEDDING_MODELS_SRC="$CORE_DIR/acowork-embed/assets/embedding_models.json"
if [ -f "$EMBEDDING_MODELS_SRC" ]; then
    cp "$EMBEDDING_MODELS_SRC" "$TARGET_DIR/"
    echo -e "${GREEN}  ✓ embedding_models.json${NC}"
fi
echo ""

# ── Step 6: Done ────────────────────────────────────────────────────────────
echo -e "${YELLOW}[6/6] Done!${NC}"
echo ""
echo -e "${CYAN}Build artifacts:${NC}"
ls -lh "$TARGET_DIR/acowork-gateway" "$TARGET_DIR/acowork-runtime" "$TARGET_DIR/acowork-embed" "$TARGET_DIR/acowork-lsp-relay" 2>/dev/null | awk '{print "  " $9 " (" $5 ")"}'
echo ""

echo -e "${CYAN}Next steps:${NC}"
echo -e "  ${GREEN}Start services:${NC}"
echo -e "    $TARGET_DIR/acowork-gateway &"
echo -e "    $TARGET_DIR/acowork-runtime &"
echo -e "    $TARGET_DIR/acowork-embed &"
echo ""
echo -e "  ${GREEN}Health check:${NC}"
echo -e "    curl http://127.0.0.1:19876/health"
echo ""
echo -e "  ${GREEN}Start Desktop App (browser mode):${NC}"
echo -e "    cd $WORKSPACE_ROOT/apps/acowork-desktop"
echo -e "    npm install"
echo -e "    npm run dev    # → http://localhost:5173"
echo ""
echo -e "  ${GREEN}Start full Tauri Desktop App:${NC}"
echo -e "    cd $WORKSPACE_ROOT/apps/acowork-desktop"
echo -e "    npm install"
echo -e "    npm run tauri dev"
echo ""
