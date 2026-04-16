# Rollball-AI 模块设计 — 总览

> 版本：v1.2 | 更新日期：2026-04-16

---

## 1. 设计原则

### 1.1 从 ZeroClaw 借鉴什么、不借鉴什么

| 维度 | ZeroClaw 做法 | Rollball 决策 | 理由 |
|------|-------------|-------------|------|
| Crate 结构 | 单 crate + 2 个辅助 crate | **多 crate workspace** | Rollball 有 Gateway 和 Agent Runtime 两个独立二进制，且需要严格的进程级代码隔离；多 crate 天然强制边界 |
| Provider 模式 | 工厂函数 + trait 动态分发 | **借鉴** | Provider trait + 工厂模式成熟可靠，直接复用 |
| Tool 模式 | 每个工具一个文件 + Tool trait + 工厂注册 | **全面借鉴** | 核心 Tool trait 保留，完整复用 ZeroClaw 工具池，分为 builtin / memory / schedule / integration / agent / browser / dev / skill / mcp / wasm / sop 等类别，manifest 声明驱动按需激活 |
| Memory 模式 | SQLite/Qdrant/Markdown 多后端 + Memory trait | **替换为 MemoryStore trait + GrafeoStore** | MemoryStore trait 定义在独立 crate（rollball-memory），Grafeo 作为 trait 实现，存储可替换 |
| Config | 单一巨型 schema.rs（572KB） | **按 crate 拆分** | 每个 crate 只有自己的配置结构，避免巨型配置文件 |
| Gateway | Axum HTTP 服务器 | **借鉴** | Axum 成熟可靠；Rollball Gateway 同时提供 Socket API（Agent Runtime 用）和 HTTP API（Desktop App / CLI 用） |
| 安全 | SecurityPolicy + sandbox 多后端 | **借鉴** | 保留安全策略模式，增加权限声明驱动 |
| Feature Flags | 30+ 个 feature | **少量 feature** | Rollball 场景更聚焦，feature flag 控制在 10 个以内 |

### 1.2 Workspace 拆分原则

1. **二进制边界 = Crate 边界**：Gateway 和 Agent Runtime 是不同进程，必须有独立 crate
2. **共享类型独立 crate**：协议消息、manifest 结构等被多个 crate 使用的类型，放入 `rollball-core`
3. **重型依赖隔离**：Grafeo（图数据库 + ONNX Runtime）、WASM 运行时等重型依赖，封装在独立 crate 中以便条件编译和交叉编译
4. **可测试性**：每个 crate 可独立测试，不依赖其他 crate 的运行时

---

## 2. Workspace 结构

```
rollball-ai/
├── Cargo.toml                    # workspace root
├── crates/
│   ├── rollball-core/            # 共享类型、协议、工具 trait
│   ├── rollball-memory/          # MemoryStore trait + 共享记忆类型（v3.4 新增）
│   ├── rollball-runtime/         # Agent Runtime 二进制 + 库
│   ├── rollball-gateway/         # Gateway 二进制 + 库
│   ├── rollball-grafeo/          # Grafeo 图数据库引擎（实现 MemoryStore trait）
│   ├── rollball-vault/           # 密钥加密存储
│   └── rollball-sign/            # .agent 包签名/验签工具
├── apps/
│   └── rollball-desktop/         # Tauri v2 桌面应用（Phase 5）
│       ├── src-tauri/            # Rust backend (Gateway/Debug 客户端 + 托盘)
│       └── web/                  # React 前端 (四栏布局 UI)
├── docs/                         # 设计文档
├── tests/                        # 集成测试
└── examples/                     # 示例 Agent 包
```

### 2.1 Workspace Cargo.toml

```toml
[workspace]
members = [
    "crates/rollball-core",
    "crates/rollball-memory",
    "crates/rollball-runtime",
    "crates/rollball-gateway",
    "crates/rollball-grafeo",
    "crates/rollball-vault",
    "crates/rollball-sign",
    "apps/rollball-desktop",
]
resolver = "2"

[workspace.package]
version = "0.1.0"
edition = "2024"
license = "Apache-2.0"
rust-version = "1.87"

[workspace.dependencies]
# 异步运行时
tokio = { version = "1.50", default-features = false, features = ["rt-multi-thread", "macros", "time", "net", "io-util", "sync", "process", "io-std", "fs", "signal"] }

# 序列化
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"

# 错误处理
anyhow = "1.0"
thiserror = "2.0"

# 日志
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["fmt", "ansi", "env-filter"] }

# 异步 trait
async-trait = "0.1"

# 加密
chacha20poly1305 = "0.10"
rand = "0.10"

# 配置
toml = "1.0"
directories = "6.0"

# 时间
chrono = { version = "0.4", features = ["clock", "std", "serde"] }

# CLI
clap = { version = "4.5", features = ["derive"] }

# 数据库
rusqlite = { version = "0.37", features = ["bundled"] }

# HTTP
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls", "stream"] }

# ZIP
zip = { version = "8.1", default-features = false, features = ["deflate-flate2"] }

# 并发原语
parking_lot = "0.12"

# UUID
uuid = { version = "1.22", features = ["v4", "std"] }

# 内部 crate 引用
rollball-core = { path = "crates/rollball-core" }
rollball-grafeo = { path = "crates/rollball-grafeo" }
rollball-vault = { path = "crates/rollball-vault" }
rollball-sign = { path = "crates/rollball-sign" }
```

---

> **各 Crate 详细设计**见子文档：
> - [01-core.md](01-core.md) — rollball-core：共享类型与协议
> - [02-runtime.md](02-runtime.md) — rollball-runtime：Agent Runtime
> - [03-gateway.md](03-gateway.md) — rollball-gateway：Gateway
> - [04-grafeo.md](04-grafeo.md) — rollball-grafeo：Grafeo 图数据库引擎
> - [05-vault-sign.md](05-vault-sign.md) — rollball-vault + rollball-sign
> - [06-architecture.md](06-architecture.md) — 依赖关系、数据流、路线图、编译产物、测试策略
