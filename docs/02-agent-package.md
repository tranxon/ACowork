# Agent 打包格式（.agent）

> 版本：v3.2 | 更新日期：2026-04-13

---

## 1. 包结构

`.agent` 文件本质是一个 ZIP 压缩包。Agent 包**不含可执行文件**，只包含配置、Prompt 和数据。由 Agent Runtime 二进制加载执行。

```
<agent_id>.agent
├── manifest.toml          # 必需，元数据 + LLM 配置 + 权限 + 工具声明
├── prompts/               # System prompt 模板
│   ├── system.md          # 主系统提示词
│   ├── tools.md           # 工具使用说明
│   └── constraints.md     # 约束和安全规则
├── config/                # 默认配置文件（用户可覆盖）
│   └── settings.toml
├── data/                  # 初始数据（如空 Grafeo 快照）
├── skills/                # Skill 定义（兼容 Agent Skills 开放标准）
│   └── weather-query/
│       ├── SKILL.md       # YAML frontmatter（---）+ Markdown body
│       ├── scripts/       # 可选：Skill 脚本
│       └── references/    # 可选：补充文档
├── tools/                 # 自定义工具（WASM，可选）
│   └── image_filter.wasm
└── resources/             # 图标、本地化等
```

## 2. 包签名机制

.agent 包签名机制参考 **Android APK Signature Scheme v3** 和 **Play App Signing** 的双密钥模型，解决三个核心安全问题：

1. **完整性**：包未被篡改
2. **来源认证**：确认开发者身份
3. **密钥轮换**：支持更新签名密钥而不丢失已有用户

### 2.1 双密钥模型（类比 Play App Signing）

```
┌─────────────────────────────────────────────────────────────────────┐
│                         分发渠道场景                                  │
│                                                                      │
│  开发者端                        商店/平台端                          │
│  ─────────                      ──────────                          │
│                                  ┌──────────────────────────┐         │
│  1. 生成上传密钥 (Upload Key)     │                          │         │
│     rollball-keygen --alias xxx   │   4. 商店用分发密钥       │         │
│         ↓                        │   (Distribution Key)      │         │
│  2. 用上传密钥签名 .agent          │   重新签名包              │         │
│     rollball-sign --key upload.key │         ↓                │         │
│         ↓                        │         ↓                │         │
│  3. 提交 .agent 包到商店           │   5. 分发签名后的包       │         │
│                                  │   给用户                  │         │
│                                  └──────────────────────────┘         │
└─────────────────────────────────────────────────────────────────────┘

关键分离：
- 上传密钥 (Upload Key)：开发者持有，用于签名提交给商店的包
- 分发密钥 (Distribution Key)：商店/平台持有，用于给用户设备安装的实际包
- 好处：开发者丢失上传密钥可重置，不影响已安装用户的更新
```

### 2.2 签名结构——Signing Block

采用与 APK Signature Scheme v3 相同的 Signing Block 格式，在 ZIP 的 Central Directory 之前插入签名数据：

```
.agent ZIP 结构：
┌──────────────────────────┐
│   ZIP Local Files        │  ← 被签名覆盖
│   (manifest.toml,        │
│    prompts/, skills/     │
│    tools/, resources/)   │
├──────────────────────────┤
│   Signing Block (v3)    │  ← 签名数据（在 Central Dir 之前）
│   ┌────────────────────┐│
│   │  Signer #1          ││
│   │  - min_sdk / max_sdk││  支持 SDK 版本范围
│   │  - certificates     ││  X.509 证书链（叶子 → 根）
│   │  - signed_attrs     ││  签名属性（含 proof_of_rotation）│
│   │  - signature       ││  对 signed_attrs 的签名         │
│   └────────────────────┘│
│   ┌────────────────────┐│
│   │  Signer #2 (可选)   ││  支持密钥轮换：旧签名者 → 新签名者
│   │  - proof_of_rotation││  链式结构                       │
│   └────────────────────┘│
├──────────────────────────┤
│   ZIP Central Dir       │  ← 被签名覆盖
├──────────────────────────┤
│   ZIP End of Directory  │  ← 被签名覆盖
└──────────────────────────┘
```

### 2.3 签名数据结构

```rust
struct SigningBlock {
    signers: Vec<Signer>,           // 支持多签名者（用于密钥轮换）
    version: u32,                  // 签名格式版本（3）
}

struct Signer {
    // --- 签名者身份 ---
    min_sdk: u32,                  // 支持的最低 Runtime 版本
    max_sdk: u32,                  // 支持的最高 Runtime 版本
    certificates: Vec<X509Cert>,   // 证书链：叶子证书 → 中间 CA → 根 CA

    // --- 签名数据（覆盖 signed_attrs）---
    signed_attrs: SignedAttributes, // 包含 digests 和 proof_of_rotation
    signature: Vec<u8>,            // 对 signed_attrs 的签名

    // --- 元数据 ---
    signature_algorithm: SigAlg,    // e.g., ECDSA-P256-SHA256, Ed25519
}

struct SignedAttributes {
    // 各 section 的 SHA-256 摘要（覆盖 LocalFiles + CentralDir + EoCD）
    digests: Vec<SectionDigest>,

    // --- v3 新增：密钥轮换证明 ---
    proof_of_rotation: Option<ProofOfRotation>,  // 链式信任结构
}

struct ProofOfRotation {
    // 当前签名者的公钥信息
    current_key: SubjectPublicKeyInfo,

    // 旧签名者信息（签署 current_key 的证书）
    previous_cert: X509Cert,

    // 旧签名者再旧的证书（可选，链式）
    ancestors: Vec<X509Cert>,
}

struct SectionDigest {
    section: Section,     // LocalFiles / CentralDir / EoCD
    algorithm: DigestAlg, // SHA-256
    hash: Vec<u8>,        // 摘要值
}
```

### 2.4 三类签名身份与信任链

| 身份类型 | 持有者 | 用途 | 验证方式 |
|---------|--------|------|---------|
| Upload Key | 开发者 | 签名提交给商店的 .agent 包 | 开发者自签名或 CA 签发 |
| Distribution Key | 商店/平台 | 重新签名后分发给用户的包 | 商店根 CA 验证 |
| Platform Key | Rollball 官方 | 签发系统 Agent（system=true） | 平台内置根公钥 |

**信任链验证流程（安装时）**：

```
安装 .agent 包
       │
       ▼
1. 定位 Signer（按 Runtime 版本范围过滤，选最新）
       │
       ▼
2. 验证证书链（必须被信任根或预置根证书验证）
   ├─ 检查证书有效期
   ├─ 验证证书链：叶子 → 中间 CA → 根 CA
   └─ 验证失败 → 拒绝安装
       │
       ▼
3. 用叶子证书公钥验证 signature
   └─ 验证失败 → 拒绝安装
       │
       ▼
4. 重新计算 ZIP 各 section digest，与 signed_attrs 中的 digests 比对
   └─ 不一致 → 拒绝安装
       │
       ▼
5. 如果有 proof_of_rotation（密钥轮换场景）：
   ├─ 验证 previous_cert 签署了 current_key
   └─ 验证 ancestors 链式完整
       │
       ▼
6. 检查签名身份与 manifest 声明的匹配：
   ├─ manifest.system = true → 必须是 Platform Key 签名
   └─ manifest.system = false → Upload Key 或 Distribution Key
       │
       ▼
7. 检查 agent_id 所有权：
   ├─ 已安装旧版本 → 新包的证书必须在信任链中
   └─ 首次安装 → 记录证书指纹
       │
       ▼
8. 全部通过 → 允许安装
```

### 2.5 密钥轮换（Proof-of-Rotation）

参考 APK Signature Scheme v3，支持签名密钥轮换：

```
场景：开发者需要更换签名密钥

旧密钥 (Key v1)                          新密钥 (Key v2)
      │                                     │
      │ 签署 (SubjectPublicKeyInfo of v2)    │
      ▼                                     │
previous_cert ──────────────────────────────►│
(v1 的证书)                                 │
      │                                     │
      │ 签署 (SubjectPublicKeyInfo of v1)   │
      ▼                                     │
ancestors[0] ──────────────────────────────►│
(v0 的证书，可选)

签名后的 .agent 包包含：
- Signer #1: Key v2 签名的完整数据 + proof_of_rotation
- Signer #2 (可选): Key v1 签名的历史数据（用于旧版本兼容）
```

**轮换规则**：
- 每个 .agent 包至少包含当前密钥的 Signer
- 可选包含历史 Signer（让旧版本 Runtime 也能验证）
- 新旧密钥必须有完整的轮换链（每个密钥签署下一个密钥的公钥）
- 商店/平台可配置是否允许上传包含旧签名的包

### 2.6 manifest 中的签名相关字段

```toml
# manifest.toml 新增字段
agent_id = "com.example.weather"
version = "1.0.0"
system = false                    # true = 系统 Agent，必须 Platform Key 签名

# 签名者信息（由商店注入或自声明）
[signing]
# 声明期望的签名者类型（安装时验证）
expected_signer_type = "distribution"  # "upload" | "distribution" | "platform"
# 上传密钥的 SHA-256 指纹（用于所有权验证）
upload_key_fingerprint = "sha256:AB:CD:EF:..."

# 已安装包的签名指纹记录（由 Gateway 管理）
# 记录首次安装时的分发证书指纹，用于后续更新验证
```

### 2.7 签名与权限的关联

```toml
# ~/.config/agent-gateway/config.toml

[trust]
# 平台签名（Platform Key）的 Agent 自动授予的权限
platform_signer_permissions = [
    "identity:admin",
    "agent:install",
    "sandbox:bypass",
]

# 分发渠道信任规则
[trust.distribution_keys]
# 商店签名的包默认权限
default_permissions = ["network:read", "memory:read"]

# 上传密钥指纹白名单（开发者声明后，商店验证通过）
[[trusted_upload_keys]]
fingerprint = "sha256:AB:CD:EF:..."
permissions = ["network:https://api.weather.com"]
label = "Weather Developer Co."

# 权限计算规则：
# 最终权限 = manifest.permissions ∩ trust 配置
# （取交集，防止 manifest 声明过度权限）
```

### 2.8 签名工具链

```bash
# 1. 开发者生成上传密钥（离线，安全的硬件更好）
rollball-keygen --algorithm Ed25519 --alias upload-key --output ./keys/

# 2. 开发者用上传密钥签名 .agent 包
rollball-sign \
    --key ./keys/upload-key.pem \
    --cert ./keys/upload-key.crt \
    --input ./build/com.example.weather.unsigned.agent \
    --output ./build/com.example.weather.signed.agent

# 3. 提交到商店，商店用分发密钥重新签名
rollball-dist-sign \
    --input ./build/com.example.weather.signed.agent \
    --dist-key ./store/keys/distribution.pem \
    --dist-cert ./store/keys/distribution.crt \
    --output ./release/com.example.weather.agent

# 4. 验证签名（开发者/用户均可）
rollball-verify --verbose ./release/com.example.weather.agent
# 输出示例：
# Signer: CN=Weather Developer Co.
#   Algorithm: Ed25519
#   Valid from: 2026-01-01 to 2027-01-01
#   Key rotation: Yes (previous key signed on 2025-06-01)
#   Certificate chain: 2 certificates (leaf + root)

# 5. 提取签名信息（用于调试）
rollball-signature-info ./release/com.example.weather.agent
```

### 2.9 Debug 签名机制

参考 Android Debug Keystore 机制，为本地开发测试提供便捷签名方式。

#### Debug 密钥管理

```
┌─────────────────────────────────────────────────────────────────────┐
│  首次运行 rollball CLI 时自动生成：                                   │
│  ~/.config/rollball/debug.keystore                                   │
│                                                                      │
│  凭证信息：                                                          │
│  - alias: rollball-debug                                             │
│  - password: rollball（或其他默认凭证，本机存储）                      │
│  - 算法: Ed25519                                                     │
│  - 有效期: 1 年（与 Android 相同）                                    │
│                                                                      │
│  注意：Debug 密钥仅用于本地开发测试，不得用于生产环境分发               │
└─────────────────────────────────────────────────────────────────────┘
```

#### Debug 签名命令

```bash
# 本地开发时使用 --debug 自动选用 debug keystore
rollball-sign --debug \
    --input ./build/com.example.weather.unsigned.agent \
    --output ./build/com.example.weather.debug.agent

# 等价于手动指定 debug key
rollball-sign \
    --key ~/.config/rollball/debug.key \
    --cert ~/.config/rollball/debug.crt \
    --input ./build/com.example.weather.unsigned.agent \
    --output ./build/com.example.weather.debug.agent
```

#### Debug 包安装验证

```toml
# Gateway 安装 debug 包时的行为：
# 1. 验证签名完整性（与正式包相同）
# 2. 检查 signed_attrs 中的 signer_type == "debug"
# 3. system=true 的 debug 包仍被拒绝（debug key 不是 Platform Key）
# 4. 输出警告："Debug 签名，仅限本地开发测试"
# 5. 可在 Gateway 配置中禁用 debug 包安装（生产环境建议）
```

```toml
# ~/.config/agent-gateway/config.toml
[debug]
# 是否允许安装 debug 签名的包
allow_debug_packages = true   # 生产环境设为 false

# Debug 包默认权限（比 trust.distribution_keys 更严格）
debug_default_permissions = ["network:read"]
```

### 2.10 System Agent 调试签名

当 `system = true` 时，必须使用 Platform Key 签名。但这会影响本地调试体验。为开发者提供 **Debug Platform Key**（类比 GitHub Personal Access Token）：

#### Debug Platform Key 机制

```
┌─────────────────────────────────────────────────────────────────────┐
│  开发者向平台申请 Debug Platform Key：                                │
│                                                                      │
│  1. 通过开发者账号认证（平台服务）                                     │
│  2. 平台颁发 Debug Platform Key（不同于生产 Platform Key）           │
│  3. 有效期 30 天，可在线续期                                           │
│  4. 平台记录：开发者身份 + 密钥指纹 + 有效期                            │
│                                                                      │
│  安全属性：                                                           │
│  - 绑定开发者身份（可追责）                                            │
│  - 短期有效（30 天过期）                                              │
│  - 平台可远程吊销                                                     │
│  - 签名后的包明确标记为 "debug platform" 类型                         │
└─────────────────────────────────────────────────────────────────────┘
```

#### 申请与使用流程

```bash
# 1. 通过平台 CLI 申请 Debug Platform Key
rollball key request-debug-platform \
    --developer-token "ghp_xxxx" \
    --output ~/.config/rollball/debug-platform.key

# 2. 签名 system=true 的调试包
rollball-sign \
    --key ~/.config/rollball/debug-platform.key \
    --cert ~/.config/rollball/debug-platform.crt \
    --signer-type debug-platform \
    --input ./build/com.example.system.unsigned.agent \
    --output ./build/com.example.system.debug.agent

# 3. Gateway 安装时验证通过，输出警告：
#    "Debug Platform Key 签名，有效期至 2026-05-13，仅供开发测试"
```

#### Debug Platform Key vs Production Platform Key

| 属性 | Debug Platform Key | Production Platform Key |
|------|---------------------|-------------------------|
| 签发者 | 平台自动颁发 | 平台官方 |
| 有效期 | 30 天 | 长期（随平台） |
| 用途 | 本地开发调试 | 生产环境安装 |
| 吊销 | 开发者可请求，平台可强制 | 仅平台可操作 |
| 商店分发 | 拒绝 | 允许 |
| 签名标记 | debug-platform | platform |

#### Gateway 验证 Debug Platform 签名

```toml
# ~/.config/agent-gateway/config.toml
[platform_keys]
# 生产 Platform Key（平台官方，Gateway 内置）
# platform_public_key = "base64:..."

# 是否信任 Debug Platform Key（默认 true）
allow_debug_platform_keys = true

# Debug Platform Key 指纹列表（从平台 API 同步）
# 由平台颁发时同时提供指纹，用于本地验证
[[debug_platform_keys]]
fingerprint = "sha256:12:34:56:..."
developer_id = "dev_abc123"
expires_at = "2026-05-13"
```

### 2.11 证书吊销与安全事件处理

商店维护吊销列表（CRL），Gateway 安装时检查：

```
安装时检查流程：
1. 解析证书链，获取每个证书的序列号
2. 查询商店 CRL，验证证书未被吊销
3. 如果证书被吊销 → 拒绝安装
4. 开发者可申请吊销并重置上传密钥
   → 商店颁发新上传证书，保留 agent_id 所有权
```

**重置上传密钥流程**：
1. 开发者向商店证明 agent_id 所有权
2. 商店吊销旧上传证书
3. 商店颁发新上传证书（包含新的 proof_of_rotation）
4. 开发者用新密钥签名，旧设备仍可通过历史 Signer 验证

## 3. manifest.toml 架构

```toml
agent_id = "com.example.weather"
version = "1.0.0"
name = "Weather Agent"
description = "查询实时天气并建议穿衣"
author = "example@domain.com"
runtime_version = "^1.0.0"
system = false                   # true = 系统 Agent，必须 Platform Key 签名

# 签名者信息（由商店注入或自声明）
[signing]
expected_signer_type = "upload"  # "upload" | "distribution" | "platform"
upload_key_fingerprint = "sha256:AB:CD:EF:..."  # 上传密钥指纹

permissions = [
    "network:https://api.weather.com",
    "filesystem:read:~/Documents",
    "memory:read",
    "memory:write",
    "intent:send:com.example.calendar",
]

[[triggers]]
type = "schedule"
cron = "0 7 * * *"

[[triggers]]
type = "message"
pattern = "天气|weather"

[llm]
default_provider = "openai"

[llm.providers.openai]
model = "gpt-4o"
api_key_ref = "vault:openai_key"
base_url = "https://api.openai.com/v1"

[llm.providers.openai.params]
temperature = 0.7
max_tokens = 4096

[llm.providers.claude]
model = "claude-sonnet-4-20250514"
api_key_ref = "vault:anthropic_key"

[llm.providers.fallback]
provider = "ollama"
model = "qwen3:8b"
base_url = "http://localhost:11434"

[llm.routing]
strategy = "cost_priority"
fallback_on_error = true

[llm.routing.retry]
max_attempts = 3
backoff = "exponential"

[llm.budget]
daily_token_limit = 100000
daily_cost_limit_usd = 5.0
action_on_exhaust = "fallback_to_local"

[memory]
sync_mode = "auto"
cache_ttl = 3600
required = false

identity_deps = ["name", "city", "language", "timezone"]

[[tools]]
name = "http_get"
type = "builtin"
permissions = ["network:https://api.weather.com"]

[[tools]]
name = "image_filter"
type = "wasm"
binary = "./tools/image_filter.wasm"
permissions = ["memory:read"]

[tools.resource_limits]
max_memory_mb = 50
max_execution_time_ms = 5000

[capabilities.query_weather.input]
city = "string"
date = "date?"

[capabilities.query_weather.output]
temperature = "float"
condition = "string"

[resources]
max_memory_mb = 200
max_cpu_percent = 50
network = true

[sandbox]
enable = true
allow_ptrace = false
read_only_root = true
```

**关键字段说明：**

- `runtime_version`：声明兼容的 Agent Runtime 版本（语义版本约束）。
- `system`：是否为系统 Agent。`true` 时必须由 Platform Key 签名，否则拒绝安装。
- `llm.providers`：支持配置多个 LLM Provider，每个引用 Vault 中的密钥。
- `llm.routing.strategy`：LLM 路由策略（cost_priority / quality_priority / latency_priority）。
- `llm.budget`：Token 和费用预算，超限后的动作（stop / fallback_to_local / warn）。
- `memory`：已移除 shared 概念。用户身份与偏好由系统 Agent 管理，其他 Agent 通过 Intent 查询。
- `identity_deps`：声明启动时需要的用户身份字段（如 name、city、language），Gateway 在启动前向系统 Agent 查询并注入。
- `tools`：工具声明，支持 builtin（内置）和 wasm（自定义沙箱）两种类型。
- `capabilities`：声明本 Agent 可被其他 Agent 通过 Intent 调用的能力，含类型信息。
- `signing.expected_signer_type`：声明期望的签名者类型（upload / distribution / platform），安装时验证。
- `signing.upload_key_fingerprint`：上传密钥指纹，用于证明 agent_id 所有权。
- 签名者权限与 manifest 权限取**交集**，防止过度授权。
