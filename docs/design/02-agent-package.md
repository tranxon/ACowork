# Agent 打包格式（.agent）

> 版本：v3.3 | 更新日期：2026-04-17

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
│       └── references/    # 可选：补充文档、模板数据
├── tools/                 # 自定义工具（WASM，可选）
│   └── image_filter.wasm
└── resources/             # 图标、本地化等
```

**约束：**

- 包大小上限 **50 MB**（安装时校验，超出拒绝安装）。
- `skills/*/references/` 仅允许不可执行的数据文件（JSON 模板、Markdown 文档等）。如需动态执行逻辑，应通过 `tools/` 下的 WASM 工具实现。

## 2. 包签名机制

.agent 包必须经过签名才能被 Gateway 安装和执行，类似 Android APK 签名。签名机制保障三个核心安全属性：**完整性**（包未被篡改）、**来源认证**（确认开发者身份）、**更新防护**（防止恶意包覆盖已安装 Agent）。

### 2.1 签名结构——Signing Block

采用类似 APK Signature Scheme v2 的思路，在 ZIP 的 Central Directory 之前插入一个 Signing Block，签名覆盖整个 ZIP 的所有内容（Local Files + Central Directory + End of Directory），而非将签名文件放在 ZIP 内部的 `META-INF/` 目录中（后者可被 strip 攻击绕过）：

```
.agent ZIP 结构：
┌──────────────────────┐
│   ZIP Local Files    │  ← 被签名覆盖
│   (manifest.toml,    │
│    prompts/, skills/)│
├──────────────────────┤
│   Signing Block      │  ← 签名数据（在 Central Dir 之前）
│   ┌────────────────┐ │
│   │  Signer        │ │
│   │  - certificates│ │     X.509 证书链
│   │  - digest list │ │     各 section 的 SHA-256 摘要
│   │  - signature   │ │     对 digest list 的签名
│   └────────────────┘ │
├──────────────────────┤
│   ZIP Central Dir    │  ← 被签名覆盖
├──────────────────────┤
│   ZIP End of Dir     │  ← 被签名覆盖
└──────────────────────┘
```

### 2.2 签名数据结构

```rust
struct SigningBlock {
    signers: Vec<Signer>,
}

struct Signer {
    certificates: Vec<Certificate>,     // X.509 证书链
    digest_algorithm: DigestAlgorithm,  // SHA-256
    digests: Vec<SectionDigest>,        // 各 section 的摘要
    signature: Vec<u8>,                 // 对 digests 的签名
    signed_attrs: SignedAttributes,     // 签名时间戳等属性
}

struct SectionDigest {
    section: Section,       // LocalFiles / CentralDir / EoCD
    hash: [u8; 32],         // SHA-256 digest
}
```

### 2.3 两类签名身份

Phase 1 实现两类签名身份。未来扩展阶段将引入 Distribution Key（商店分发密钥），详见第 5 节。

| 身份类型 | 适用场景 | 说明 | 类比 Android |
|---------|---------|------|-------------|
| Developer | 普通 Agent | 开发者自签名证书，最常见 | 普通应用的 debug/release 签名 |
| Platform | 系统 Agent | Rollball 平台签发的证书，Gateway 内置平台根公钥 | Platform 签名（系统应用） |

### 2.4 签名验证流程（安装时强制校验）

```
用户安装 .agent 包
       │
       ▼
1. 解析 ZIP，提取 Signing Block
       │
       ▼
2. 用证书中的公钥验证签名
   （证明"包确实由持有该私钥的人签名"）
   └─ 验证失败 → 拒绝安装："签名无效"
       │
       ▼
3. 按 SHA-256 重新计算各 section digest，与签名中的 digests 比对
   （证明"包内容未被篡改"）
   └─ 不一致 → 拒绝安装："包已被篡改"
       │
       ▼
4. 验证证书可信性（区分签名身份）:
   │
   ├─ 证书由 Gateway 内置的平台根公钥签发？
   │   └─ 是 → 识别为 Platform 签名
   │
   └─ 否 → 识别为 Developer 自签名
   │        （自签名证书链验证无安全意义，任何人都可以自签名）
       │
       ▼
5. 检查签名身份与 manifest 声明的匹配:
   ├─ manifest 声明 platform.system = true → 必须是 Platform 签名
   │   └─ 非 Platform 签名 → 拒绝安装："系统 Agent 必须由平台签发"
   │
   └─ 已安装同 agent_id 的旧版本 → 比对签名证书指纹
       └─ 证书指纹不一致 → 拒绝更新："签名者与已安装版本不同"
       │
       ▼
6. 全部通过 → 允许安装
```

**两种签名身份的信任模型差异：**

| 验证维度 | Developer 自签名 | Platform Key |
|---------|-----------------|-------------|
| 完整性 | SHA-256 digest 比对 | 同左 |
| 来源认证 | 证书指纹一致性（更新时匹配） | 证书链验证（必须由平台根 CA 签发） |
| 首次安装 | 任何自签名证书都接受 | 必须匹配平台根公钥 |
| 更新安装 | 新包证书指纹 = 已安装指纹 | 同左（平台根 CA 不变） |

Developer 自签名的安全保障靠**指纹锁定**：首次安装时记录证书指纹，后续更新必须一致。这和 Android 早期（v1 签名）对普通应用的做法一致——你不知道开发者是谁，但你能保证更新来自同一个开发者。

### 2.5 签名与权限的关联

Gateway 配置中可定义基于签名者的信任规则，特定签名的 Agent 可自动获得额外权限：

```toml
# ~/.config/agent-gateway/config.toml

[trust]
# 平台签名的 Agent 自动授予的权限
platform_signer_permissions = [
    "identity:admin",       # 可管理用户身份
    "agent:install",        # 可触发安装其他 Agent
    "sandbox:bypass",       # 可跳过沙箱（系统 Agent）
]

# 指定证书指纹的信任规则
[[trusted_signers]]
fingerprint = "sha256:AB:CD:EF:..."
permissions = ["network:*"]
label = "Trusted Weather Developer"
```

### 2.6 签名工具链

```bash
# 生成开发者密钥对
rollball-keygen --alias my-key --output ./keys/

# 签名 .agent 包
rollball-sign \
    --key ./keys/my-key.pem \
    --cert ./keys/my-key.crt \
    --input ./build/com.example.weather.unsigned.agent \
    --output ./build/com.example.weather.agent

# 验证签名
rollball-verify ./build/com.example.weather.agent

# 查看签名信息
rollball-verify --verbose ./build/com.example.weather.agent
# 输出：Signer: CN=Zhang San, O=Example Corp
#       Digest: SHA-256
#       Valid from: 2026-01-01 to 2027-01-01
```

### 2.7 Debug 签名

参考 Android Debug Keystore 机制，为本地开发测试提供便捷签名方式。

首次运行 `rollball` CLI 时自动生成 debug 密钥：

```
~/.config/rollball/debug.key
~/.config/rollball/debug.crt

- 算法: Ed25519
- 有效期: 1 年
- 仅限本地开发测试，不得用于生产分发
```

```bash
# 使用 --debug 自动选用 debug key
rollball-sign --debug \
    --input ./build/com.example.weather.unsigned.agent \
    --output ./build/com.example.weather.debug.agent
```

Gateway 安装 debug 包时的行为：

1. 验证签名完整性（与正式包相同）
2. 检测到 debug 证书指纹 → 输出警告："Debug 签名，仅限本地开发测试"
3. `platform.system = true` 的 debug 包仍被拒绝（debug key 不是 Platform Key）
4. 生产环境可在 Gateway 配置中禁用 debug 包安装：

```toml
# ~/.config/agent-gateway/config.toml
[debug]
allow_debug_packages = true   # 生产环境设为 false
```

### 2.8 系统 Agent 本地调试

`platform.system = true` 的 Agent 必须由 Platform Key 签名，但开发者本地没有 Platform Key。Phase 1 采用**本地信任配置**解决——开发者在 Gateway 配置中显式信任特定指纹的包以 system 权限运行：

```toml
# ~/.config/agent-gateway/config.toml
[debug]
# 允许指定指纹的包以 system 权限运行（仅开发用）
[[debug_platform_overrides]]
fingerprint = "sha256:12:34:56:..."
agent_id = "com.rollball.system"
note = "本地开发调试用"
```

这是纯本地操作，不依赖在线平台服务。更完善的 Debug Platform Key 机制（在线申请、远程吊销等）留到 Phase 5 实现。

## 3. manifest.toml 架构

```toml
manifest_version = 1

agent_id = "com.example.weather"
version = "1.0.0"
name = "Weather Agent"
description = "查询实时天气并建议穿衣"
author = "example@domain.com"

[platform]
runtime_version = "^1.0.0"
system = false                   # true = 系统 Agent，必须 Platform Key 签名

# 平台兼容性声明（可选，默认全平台）
# 采用 Android uses-feature 的 required/optional 模式
[target_platforms]
desktop = true                   # 或 "required"（等同 true）
mobile = "optional"              # true / false / "required" / "optional"

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
name = "http_request"
type = "builtin"
permissions = ["network:https://api.weather.com"]

[[tools]]
name = "image_filter"
type = "wasm"
binary = "./tools/image_filter.wasm"
permissions = ["memory:read"]

[tools.image_filter.resource_limits]
max_memory_mb = 50
max_execution_time_ms = 5000

# 企业 RAG 工具（可选，对接企业自建知识库）
[[tools]]
name = "enterprise_knowledge"
type = "rag"
description = "查询企业产品知识库"
[tools.enterprise_knowledge.rag_config]
endpoint = "https://rag.internal.company.com/api/query"
collection = "product_docs"
auth_ref = "vault:company_rag_token"
auth_type = "bearer"

# capabilities 用映射语法（action → schema），
# 与 tools 用数组语法（tools 可重名、需有序）不同，
# 因为 capabilities 天然是 action 名称到类型的唯一映射。
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

- `manifest_version`：包格式版本号，用于未来格式演进时的兼容性判断。当前为 `1`。
- `platform.runtime_version`：声明兼容的 Agent Runtime 版本（语义版本约束）。
- `platform.system`：是否为系统 Agent。`true` 时必须由 Platform Key 签名，否则拒绝安装。独立于普通元数据，因为它是安全敏感的平台属性。
- `target_platforms`：平台兼容性声明。采用 Android uses-feature 的 required/optional 模式。不声明时默认全平台支持。值：`true` / `"required"` = 必需（移动端安装被拒绝）、`false` = 不支持、`"optional"` = 可选（移动端行为降级）。详见 12-tool-system.md 第 2.1-2.2 节。
- `llm.providers`：支持配置多个 LLM Provider，每个引用 Vault 中的密钥。
- `llm.routing.strategy`：LLM 路由策略（cost_priority / quality_priority / latency_priority）。
- `llm.budget`：Token 和费用预算，超限后的动作（stop / fallback_to_local / warn）。
- `memory`：已移除 shared 概念。用户身份与偏好由系统 Agent 管理，其他 Agent 通过 Intent 查询。
- `identity_deps`：声明启动时需要的用户身份字段（如 name、city、language），Gateway 在启动前向系统 Agent 查询并注入。详见下方 §3.1。
- `tools`：工具声明，支持 builtin（内置）和 wasm（自定义沙箱）两种类型。resource_limits 内联到对应 tool 项下。
- `capabilities`：声明本 Agent 可被其他 Agent 通过 Intent 调用的能力，含类型信息。使用映射语法（action name → schema），因为 action 名称天然唯一。
- `triggers.cron`：标准 5 段 cron 表达式（`分 时 日 月 周`），使用 UTC 时区，不支持秒级精度和特殊宏（`@daily` 等）。示例：`"0 7 * * *"` = 每天 UTC 07:00。Gateway 在解析时校验格式，非法表达式拒绝安装。时区偏移由 Gateway 根据用户配置的本地时区在触发时计算，manifest 中不声明时区。

### 3.1 identity_deps 注入细节

`identity_deps` 是一个字符串数组，声明 Agent 启动时需要的用户身份字段。Gateway 在拉起 Agent Runtime 前，向系统 Agent 查询这些字段的值，通过握手协议的 `identity_delivery` 消息注入。

**字段名约定**：

| 字段名 | 语义 | 来源 | Phase 1 默认值（System Agent 无数据时） |
|--------|------|------|--------------------------------------|
| `display_name` | 用户希望被怎么称呼 | Onboarding 必填 | `""`（空字符串，Agent 应在 prompt 中优雅降级） |
| `language` | 用户语言偏好（BCP 47） | Onboarding 必填 | `"en-US"`（安全回退到英语） |
| `timezone` | 用户时区（IANA） | Onboarding 必填 | `"UTC"`（安全回退到 UTC） |
| `city` | 所在城市 | Onboarding 选填 | `null`（未知，不猜测） |
| `occupation` | 职业/领域 | 对话沉淀 | `null` |
| `communication_style` | 沟通偏好 | 对话沉淀 | `null` |
| `custom:*` | 开放扩展字段 | 各来源 | `null` |

**required vs optional 语义**：

identity_deps 数组中的字段**全部视为 optional**——即使 Agent 声明了 `identity_deps = ["display_name", "city"]`，如果系统 Agent 返回 `city = null`（用户未提供），Gateway 仍然正常启动 Agent，只是 identity_delivery 中该字段值为 null。

这种设计基于以下考量：
- Agent 不应因为用户缺少某个身份信息就拒绝工作（如天气 Agent 不知道用户城市，可以主动询问）
- 必填 vs 选填的控制权在 Onboarding 侧（哪些字段强制采集），而非 Agent 声明侧
- 如果未来确实需要 required 语义，可在字段名后加 `!` 后缀（如 `"city!"`），但 Phase 1 不实现

**identity_delivery 中字段缺失的处理**：

```json
// Agent 声明 identity_deps = ["display_name", "city", "occupation"]
// 系统Agent只知道 display_name

{
    "type": "identity_delivery",
    "fields": {
        "display_name": "张三",
        "city": null,
        "occupation": null
    },
    "confidence": {
        "display_name": 1.0,
        "city": null,
        "occupation": null
    }
}
```

- 已知字段：返回值 + confidence
- 未知字段：值为 `null`，confidence 为 `null`
- 系统 Agent 不认识的字段名：仍返回 `null`（不会报错）

### 3.2 权限匹配语义

permissions 数组中的每条权限字符串遵循统一的模式语法，Gateway 和 Runtime 据此判断工具调用的授权状态。

**权限格式**：

```
<domain>:<resource>[:<qualifier>]
```

| 组成 | 说明 | 示例 |
|------|------|------|
| `domain` | 权限域（大类别） | `network`, `filesystem`, `memory`, `intent` |
| `resource` | 具体资源或操作 | `https://api.weather.com`, `read`, `write`, `send` |
| `qualifier` | 可选限定词 | `~/Documents`, `com.example.calendar` |

**通配符规则**：

| 模式 | 含义 | 匹配示例 | 不匹配 |
|------|------|---------|--------|
| `network:https://api.weather.com` | 精确匹配 | `https://api.weather.com` | `https://api.other.com` |
| `network:https://*.weather.com` | 子域名通配 | `https://api.weather.com`, `https://v2.weather.com` | `https://weather.com`（裸域不匹配） |
| `network:*` | 整个 network 域 | 任意 HTTPS URL | — |
| `filesystem:read:*` | 整个 filesystem:read 域 | `~/Documents`, `/tmp` | — |
| `filesystem:*:*` | 整个 filesystem 域 | read/write 任意路径 | — |
| `intent:send:*` | 可向任何 Agent 发 Intent | `com.example.calendar`, `com.rollball.system` | — |
| `memory:read` | 无需 qualifier | — | — |

**匹配算法**：

```rust
fn matches_permission(declared: &str, requested: &str) -> bool {
    let decl_parts: Vec<&str> = declared.splitn(3, ':').collect();
    let req_parts: Vec<&str> = requested.splitn(3, ':').collect();

    // 1. 域必须精确匹配
    if decl_parts[0] != req_parts[0] { return false; }

    // 2. 资源匹配（支持 * 通配和子域名 *.example.com）
    if decl_parts[1] == "*" { return true; }  // 整个域通配
    if !wildcard_match(decl_parts[1], req_parts[1]) { return false; }

    // 3. qualifier 匹配（如果声明了 qualifier）
    if decl_parts.len() == 3 && req_parts.len() == 3 {
        wildcard_match(decl_parts[2], req_parts[2])
    } else if decl_parts.len() == 3 && req_parts.len() == 2 {
        false  // 声明了 qualifier 但请求没有，不匹配
    } else {
        true   // 都没有 qualifier，匹配
    }
}
```

**权限检查流程**：

1. Agent 发起工具调用（如 `http_request({"method":"GET","url":"https://api.weather.com/..."})`)
2. Runtime 构造请求权限字符串（如 `network:https://api.weather.com`）
3. 遍历 manifest.permissions，调用 `matches_permission`
4. 任一声明权限匹配 → 允许执行
5. 无匹配 → 拒绝执行，返回 PermissionDenied 错误

**Phase 1 实际权限列表**：

| 域 | 权限示例 | 对应工具 |
|----|---------|---------|
| network | `network:https://api.example.com` | http_request |
| filesystem | `filesystem:read:~/Documents`, `filesystem:write:~/Documents` | file_read, file_write |
| filesystem | `filesystem:read:/tmp` | file_read |
| memory | `memory:read`, `memory:write` | memory_query, memory_store |
| intent | `intent:send:com.example.calendar` | intent_send |
| shell | （shell 工具无需声明 permission，由 Approval Gate 控制） | shell |
| search | （search 工具无需声明 permission，只读公开数据） | web_search |
| identity | （identity_store 仅系统 Agent 可用，通过 platform.system 声明授权） | identity_store |

## 4. 设计决策记录

| 决策 | 选择 | 理由 |
|------|------|------|
| 签名方案 | v2 风格（单签名者） | Phase 1 最小实现，密钥轮换等复杂特性延迟到 Phase 5 |
| 签名信息位置 | Signing Block 元数据 | 签名验证数据不属于开发者声明层，不放在 manifest.toml 中 |
| `system` 字段位置 | `[platform]` 节 | 安全敏感属性与普通元数据隔离，减少误操作风险 |
| 包大小上限 | 50 MB | 防止包含过大 Grafeo 快照或 WASM 工具导致安装问题 |
| capabilities 语法 | 映射（非数组） | action 名称天然唯一，映射比数组更直观 |
| 平台兼容性模型 | Android uses-feature（required/optional） | shell 在移动端不可用、文件操作受限，需要声明式降级机制 |
| target_platforms 默认 | 全平台 | 不声明的 Agent 假设全平台兼容，由运行时检测实际可用性 |
| RAG 工具声明 | 独立 type="rag" + rag_config 节 | 企业 RAG 是外部服务，需要 endpoint/auth/collection 等独立配置 |

## 5. 未来扩展（Phase 5+）

以下特性将在云端与生态阶段实现，当前仅记录设计方向，不在 Phase 1 中实现：

### 5.1 双密钥模型（类比 Play App Signing）

引入 Upload Key（开发者持有）和 Distribution Key（商店持有）的分离。开发者用 Upload Key 签名提交给商店，商店用 Distribution Key 重新签名后分发给用户。好处：开发者丢失 Upload Key 可重置，不影响已安装用户的更新。

### 5.2 密钥轮换（Proof-of-Rotation）

参考 APK Signature Scheme v3，支持签名密钥轮换。Signing Block 中可包含多个 Signer，通过 `proof_of_rotation` 字段建立旧密钥到新密钥的链式信任。旧版本 Runtime 仍可通过历史 Signer 验证包。

### 5.3 Distribution Key（商店分发密钥）

新增第三类签名身份。商店/平台持有 Distribution Key，用于重新签名开发者提交的包。安装时 Gateway 根据 Distribution Key 的商店根 CA 验证。

### 5.4 证书吊销与 CRL

商店维护证书吊销列表（CRL），Gateway 安装时在线查询。开发者可申请吊销并重置 Upload Key，商店颁发新证书并包含 proof_of_rotation。

### 5.5 Debug Platform Key（在线申请）

开发者通过平台账号认证申请 Debug Platform Key（有效期 30 天，可远程吊销），用于在本地调试 system=true 的 Agent。替代当前 Phase 1 的本地信任配置方案。
