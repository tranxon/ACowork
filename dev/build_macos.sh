#!/usr/bin/env bash
# build_macos.sh - macOS 一键构建脚本
# 解决问题：
#   1. 跳过 setup_ort.sh 的 GitHub 下载（国内网络慢）
#   2. 用 --features download-ort,coreml 自动下载 Apple Silicon 优化版
#   3. 自动从 Cargo 缓存复制 ONNX Runtime 到 .ort/
#   4. 自动配置 Homebrew + pkg-config + cmake
#
# Usage:
#   ./dev/build_macos.sh             # 默认 Apple Silicon 优化
#   ./dev/build_macos.sh --cpu       # 纯 CPU（兼容性最好）
#   ./dev/build_macos.sh --skip-embed 跳过 embed
#   ./dev/build_macos.sh --help

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
USE_GPU=true         # Apple Silicon 自动启用 CoreML
SKIP_EMBED=false
SHOW_HELP=false

# ── Parse arguments ─────────────────────────────────────────────────────────
for arg in "$@"; do
    case "$arg" in
        --cpu)        USE_GPU=false ;;
        --skip-embed) SKIP_EMBED=true ;;
        -h|--help)
            cat << 'EOF'
Usage: ./dev/build_macos.sh [OPTIONS]

Options:
  --cpu           Use CPU-only ONNX Runtime (no CoreML acceleration)
  --skip-embed    Skip building the embedding runtime entirely
  --help, -h      Show this help

Examples:
  ./dev/build_macos.sh               # Apple Silicon + CoreML (recommended)
  ./dev/build_macos.sh --cpu         # CPU only (Intel Mac or compatibility)
  ./dev/build_macos.sh --skip-embed  # Skip embed, build only Gateway + Runtime
EOF
            exit 0
            ;;
        *) echo -e "${RED}Unknown option: $arg${NC}"; exit 1 ;;
    esac
done

# ── Header ──────────────────────────────────────────────────────────────────
echo -e "${CYAN}╔══════════════════════════════════════════════╗${NC}"
echo -e "${CYAN}║   AgentCowork.AI — macOS 一键构建脚本          ║${NC}"
echo -e "${CYAN}╚══════════════════════════════════════════════╝${NC}"
echo ""
echo -e "${GRAY}  Arch: $ARCH${NC}"
echo -e "${GRAY}  CoreML: $($USE_GPU && echo true || echo false)${NC}"
echo ""

# ── Step 0: 检查必要工具 ────────────────────────────────────────────────────
echo -e "${YELLOW}[0/6] 检查开发工具...${NC}"

# Homebrew
if ! command -v brew &>/dev/null; then
    echo -e "${RED}  ✗ Homebrew 未安装${NC}"
    echo -e "${YELLOW}  安装方法: /bin/bash -c \"\$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)\"${NC}"
    exit 1
fi
echo -e "${GREEN}  ✓ Homebrew $(brew --version | head -1)${NC}"

# pkg-config
if ! command -v pkg-config &>/dev/null; then
    echo -e "${YELLOW}  ⚠ pkg-config 未安装，正在安装...${NC}"
    brew install pkg-config
fi
echo -e "${GREEN}  ✓ pkg-config $(pkg-config --version)${NC}"

# cmake
if ! command -v cmake &>/dev/null; then
    echo -e "${YELLOW}  ⚠ cmake 未安装，正在安装...${NC}"
    brew install cmake
fi
echo -e "${GREEN}  ✓ cmake $(cmake --version | head -1 | awk '{print $3}')${NC}"

# Rust toolchain
if ! command -v cargo &>/dev/null; then
    echo -e "${RED}  ✗ Rust 未安装${NC}"
    echo -e "${YELLOW}  安装方法: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh${NC}"
    exit 1
fi
RUST_VER=$(rustc --version | awk '{print $2}')
echo -e "${GREEN}  ✓ Rust $RUST_VER${NC}"

# Check if nightly (project requires)
if ! rustc --version | grep -q nightly; then
    echo -e "${YELLOW}  ⚠ 当前是 stable，建议切换到 nightly${NC}"
    echo -e "${GRAY}    rustup default nightly${NC}"
fi

# Node.js
if ! command -v node &>/dev/null; then
    echo -e "${YELLOW}  ⚠ Node.js 未安装，建议用 nvm 安装 20.x${NC}"
    echo -e "${GRAY}    curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.1/install.sh | bash${NC}"
    echo -e "${GRAY}    nvm install 20 && nvm use 20${NC}"
else
    echo -e "${GREEN}  ✓ Node.js $(node --version)${NC}"
fi

# Cargo mirror (国内网络加速)
if [ ! -f "$HOME/.cargo/config.toml" ]; then
    echo -e "${YELLOW}  ⚠ 配置 Cargo 国内镜像...${NC}"
    mkdir -p "$HOME/.cargo"
    cat > "$HOME/.cargo/config.toml" << 'EOF'
[source.crates-io]
replace-with = "ustc"

[source.ustc]
registry = "sparse+https://mirrors.ustc.edu.cn/crates.io-index/"

[net]
git-fetch-with-cli = true
EOF
    echo -e "${GREEN}  ✓ Cargo 镜像已配置 (中科大)${NC}"
fi

echo ""

# ── Step 1: 停止旧进程 ──────────────────────────────────────────────────────
echo -e "${YELLOW}[1/6] 停止旧进程...${NC}"
for proc in acowork-gateway acowork-runtime acowork-embed; do
    pids=$(pgrep -f "$proc" 2>/dev/null || true)
    if [ -n "$pids" ]; then
        pkill -f "$proc" 2>/dev/null || true
        echo -e "${GRAY}  停止 $proc: $pids${NC}"
    fi
done
# 释放 embed 端口
if command -v fuser &>/dev/null; then
    fuser -k 18080/tcp 2>/dev/null || true
fi
sleep 1
echo -e "${GREEN}  ✓ 进程清理完成${NC}"
echo ""

# ── Step 2: 构建 Gateway ────────────────────────────────────────────────────
echo -e "${YELLOW}[2/6] 构建 Gateway...${NC}"
cd "$CORE_DIR"
if cargo build --release -p acowork-gateway 2>&1 | tail -20; then
    echo -e "${GREEN}  ✓ Gateway 编译完成${NC}"
else
    echo -e "${RED}  ✗ Gateway 编译失败${NC}"
    exit 1
fi
echo ""

# ── Step 3: 构建 Runtime ────────────────────────────────────────────────────
echo -e "${YELLOW}[3/6] 构建 Runtime...${NC}"
if cargo build --release -p acowork-runtime 2>&1 | tail -20; then
    echo -e "${GREEN}  ✓ Runtime 编译完成${NC}"
else
    echo -e "${RED}  ✗ Runtime 编译失败${NC}"
    exit 1
fi
echo ""

# ── Step 4: 构建 Embed ──────────────────────────────────────────────────────
if [ "$SKIP_EMBED" = "true" ]; then
    echo -e "${YELLOW}[4/6] 跳过 Embed (--skip-embed)${NC}"
    echo ""
else
    echo -e "${YELLOW}[4/6] 构建 Embed（自动下载 ONNX Runtime）...${NC}"

    # 决定用哪个 feature
    EMBED_FEATURES="download-ort"
    if [ "$USE_GPU" = "true" ] && [ "$ARCH" = "arm64" ]; then
        EMBED_FEATURES="download-ort,coreml"
        echo -e "${GRAY}  使用 Apple Silicon CoreML 加速${NC}"
    else
        echo -e "${GRAY}  使用 CPU 模式${NC}"
    fi

    if cargo build --release -p acowork-embed --features "$EMBED_FEATURES" 2>&1 | tail -30; then
        echo -e "${GREEN}  ✓ Embed 编译完成${NC}"
    else
        echo -e "${RED}  ✗ Embed 编译失败${NC}"
        exit 1
    fi

    # Step 4.5: 把下载的 ONNX Runtime 复制到 .ort/（让后续脚本能找到）
    echo -e "${YELLOW}  [4.5] 同步 ONNX Runtime 到 .ort/...${NC}"

    ORT_TARGET_DIR="$WORKSPACE_ROOT/.ort/onnxruntime-osx-aarch64-latest/lib"
    mkdir -p "$ORT_TARGET_DIR"

    # 在 Cargo 缓存中找 ONNX Runtime
    FOUND_LIB=$(find "$HOME/.cargo/registry/cache" -maxdepth 6 \
        -name "libonnxruntime.dylib" -type f 2>/dev/null | head -1)

    if [ -z "$FOUND_LIB" ]; then
        # 也找 .so 或 .a
        FOUND_LIB=$(find "$HOME/.cargo/registry/cache" -maxdepth 6 \
            \( -name "libonnxruntime.dylib" -o -name "libonnxruntime.so" \) -type f 2>/dev/null | head -1)
    fi

    if [ -n "$FOUND_LIB" ]; then
        cp "$FOUND_LIB" "$ORT_TARGET_DIR/"
        echo -e "${GREEN}  ✓ 已复制到 $ORT_TARGET_DIR${NC}"
        echo -e "${GRAY}    源文件: $FOUND_LIB${NC}"
    else
        echo -e "${YELLOW}  ⚠ 未在 Cargo 缓存找到 ONNX Runtime，但 embed 已编译通过${NC}"
        echo -e "${GRAY}    这通常没问题，因为 cargo 已把库静态链接进 binary${NC}"
    fi
    echo ""
fi

# ── Step 5: 复制资源文件 ────────────────────────────────────────────────────
echo -e "${YELLOW}[5/6] 复制资源文件...${NC}"
RELEASE_DIR="$WORKSPACE_ROOT/target/release"
OFFLINE_SRC="$WORKSPACE_ROOT/assets/offline_providers.json"
if [ -f "$OFFLINE_SRC" ]; then
    cp "$OFFLINE_SRC" "$RELEASE_DIR/"
    echo -e "${GREEN}  ✓ offline_providers.json${NC}"
fi

# Copy embedding_models.json
EMBEDDING_MODELS_SRC="$CORE_DIR/acowork-embed/assets/embedding_models.json"
if [ -f "$EMBEDDING_MODELS_SRC" ]; then
    cp "$EMBEDDING_MODELS_SRC" "$RELEASE_DIR/"
    echo -e "${GREEN}  ✓ embedding_models.json${NC}"
fi
echo ""

# ── Step 6: 启动服务（可选） ────────────────────────────────────────────────
echo -e "${YELLOW}[6/6] 完成！${NC}"
echo ""
echo -e "${CYAN}构建产物：${NC}"
ls -lh "$RELEASE_DIR/acowork-gateway" "$RELEASE_DIR/acowork-runtime" "$RELEASE_DIR/acowork-embed" 2>/dev/null | awk '{print "  " $9 " (" $5 ")"}'
echo ""

echo -e "${CYAN}下一步：${NC}"
echo -e "  ${GREEN}启动服务:${NC}"
echo -e "    $RELEASE_DIR/acowork-gateway &"
echo -e "    $RELEASE_DIR/acowork-runtime &"
echo -e "    $RELEASE_DIR/acowork-embed &"
echo ""
echo -e "  ${GREEN}健康检查:${NC}"
echo -e "    curl http://127.0.0.1:19876/health"
echo ""
echo -e "  ${GREEN}启动 Desktop App（浏览器模式）:${NC}"
echo -e "    cd $WORKSPACE_ROOT/apps/acowork-desktop"
echo -e "    npm install"
echo -e "    npm run dev    # → http://localhost:5173"
echo ""
echo -e "  ${GREEN}启动完整 Tauri 桌面 App:${NC}"
echo -e "    cd $WORKSPACE_ROOT/apps/acowork-desktop"
echo -e "    npm install"
echo -e "    npm run tauri dev"
echo ""
