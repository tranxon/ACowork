# Agent 打包格式（.agent）

> 版本：v3.0 | 更新日期：2026-04-09

---

## 1. 包结构

`.agent` 文件本质是一个 ZIP 压缩包。Agent 包**不含可执行文件**，只包含配置、Prompt 和数据。由 Agent Runtime 二进制加载执行。

```
<agent_id>.agent
├── manifest.json          # 必需，元数据 + LLM 配置 + 权限 + 工具声明
├── prompts/               # System prompt 模板
│   ├── system.md          # 主系统提示词
│   ├── tools.md           # 工具使用说明
│   └── constraints.md     # 约束和安全规则
├── config/                # 默认配置文件（用户可覆盖）
│   └── settings.toml
├── data/                  # 初始数据（如空 Grafeo 快照）
├── skills/                # Skill 定义
│   └── weather-query/
│       ├── SKILL.toml
│       └── SKILL.md
├── tools/                 # 自定义工具（WASM，可选）
│   └── image_filter.wasm
└── resources/             # 图标、本地化等
```

## 2. 包签名机制

.agent 包必须经过签名才能被 Gateway 安装和执行，类似 Android APK 签名。签名机制保障三个核心安全属性：**完整性**（包未被篡改）、**来源认证**（确认开发者身份）、**更新防护**（防止恶意包覆盖已安装 Agent）。

### 2.1 签名结构——Signing Block

采用类似 APK Signature Scheme v2 的思路，在 ZIP 的 Central Directory 之前插入一个 Signing Block，签名覆盖整个 ZIP 的所有内容（Local Files + Central Directory + End of Directory），而非将签名文件放在 ZIP 内部的 `META-INF/` 目录中（后者可被 strip 攻击绕过）：

```
.agent ZIP 结构：
┌──────────────────────┐
│   ZIP Local Files    │  ← 被签名覆盖
│   (manifest.json,    │
│    prompts/, skills/)│
├──────────────────────┤
│   Signing Block      │  ← 签名数据（在 Central Dir 之前）
│   ┌────────────────┐ │
│   │  Signer        │ │
│   │  - certificates│ │     X.509 证书链
│   │  - digest list │ │     各 section 的 SHA-256 摘要
│   │  - signature   │ │     对 digest list 的签名
│   └────────────────┘ │
│   (可包含多个 Signer)│ │  ← 支持多签名者（如开发者+平台联合签名）
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

### 2.3 三类签名身份

| 身份类型 | 适用场景 | 说明 | 类比 Android |
|---------|---------|------|-------------|
| Developer | 普通 Agent | 开发者自签名证书，最常见 | 普通应用的 debug/release 签名 |
| Platform | 系统 Agent | Rollball 平台签发的证书，Gateway 内置平台根公钥 | Platform 签名（系统应用） |
| CA Issued | 商店/企业 Agent | 受信 CA 签发的证书，用于商店生态（可选） | Google Play 签名 |

### 2.4 签名验证流程（安装时强制校验）

```
用户安装 .agent 包
       │
       ▼
Package Manager 解析 ZIP:
  ├─ 提取 Signing Block
  ├─ 用证书中的公钥验证签名
  ├─ 按 SHA-256 重新计算各 section digest
  ├─ 对比签名中的 digest 与计算值
  │
  ├─ 验证失败 → 拒绝安装："包已被篡改或签名无效"
  │
  └─ 验证成功 → 检查签名身份:
       │
       ├─ manifest 声明 system=true → 必须是 Platform 签名
       │   └─ 非 Platform 签名 → 拒绝安装："系统 Agent 必须由平台签发"
       │
       ├─ 已安装同 agent_id 的旧版本 → 比对签名证书指纹
       │   └─ 证书指纹不一致 → 拒绝更新："签名者与已安装版本不同"
       │
       └─ 全部通过 → 允许安装
```

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

## 3. manifest.json 架构

```json
{
  "agent_id": "com.example.weather",
  "version": "1.0.0",
  "name": "Weather Agent",
  "description": "查询实时天气并建议穿衣",
  "author": "example@domain.com",
  "runtime_version": "^1.0.0",
  "permissions": [
    "network:https://api.weather.com",
    "filesystem:read:~/Documents",
    "memory:read",
    "memory:write",
    "intent:send:com.example.calendar"
  ],
  "triggers": [
    {"type": "schedule", "cron": "0 7 * * *"},
    {"type": "message", "pattern": "天气|weather"}
  ],
  "llm": {
    "default_provider": "openai",
    "providers": {
      "openai": {
        "model": "gpt-4o",
        "api_key_ref": "vault:openai_key",
        "base_url": "https://api.openai.com/v1",
        "params": {"temperature": 0.7, "max_tokens": 4096}
      },
      "claude": {
        "model": "claude-sonnet-4-20250514",
        "api_key_ref": "vault:anthropic_key"
      },
      "fallback": {
        "provider": "ollama",
        "model": "qwen3:8b",
        "base_url": "http://localhost:11434"
      }
    },
    "routing": {
      "strategy": "cost_priority",
      "fallback_on_error": true,
      "retry": {"max_attempts": 3, "backoff": "exponential"}
    },
    "budget": {
      "daily_token_limit": 100000,
      "daily_cost_limit_usd": 5.0,
      "action_on_exhaust": "fallback_to_local"
    }
  },
  "memory": {
    "sync_mode": "auto",
    "cache_ttl": 3600,
    "required": false
  },
  "identity_deps": ["name", "city", "language", "timezone"],
  "tools": [
    {
      "name": "http_get",
      "type": "builtin",
      "permissions": ["network:https://api.weather.com"]
    },
    {
      "name": "image_filter",
      "type": "wasm",
      "binary": "./tools/image_filter.wasm",
      "permissions": ["memory:read"],
      "resource_limits": {
        "max_memory_mb": 50,
        "max_execution_time_ms": 5000
      }
    }
  ],
  "capabilities": {
    "query_weather": {
      "input": {"city": "string", "date": "date?"},
      "output": {"temperature": "float", "condition": "string"}
    }
  },
  "resources": {
    "max_memory_mb": 200,
    "max_cpu_percent": 50,
    "network": true
  },
  "sandbox": {
    "enable": true,
    "allow_ptrace": false,
    "read_only_root": true
  }
}
```

**关键字段说明：**

- `runtime_version`：声明兼容的 Agent Runtime 版本（语义版本约束）。
- `llm.providers`：支持配置多个 LLM Provider，每个引用 Vault 中的密钥。
- `llm.routing.strategy`：LLM 路由策略（cost_priority / quality_priority / latency_priority）。
- `llm.budget`：Token 和费用预算，超限后的动作（stop / fallback_to_local / warn）。
- `memory.shared`：已移除。用户身份与偏好由系统 Agent 管理，其他 Agent 通过 Intent 查询或提报。
- `identity_deps`：声明启动时需要的用户身份字段（如 name、city、language），Gateway 在启动前向系统 Agent 查询并注入。
- `tools`：工具声明，支持 builtin（内置）和 wasm（自定义沙箱）两种类型。
- `capabilities`：声明本 Agent 可被其他 Agent 通过 Intent 调用的能力，含类型信息。
