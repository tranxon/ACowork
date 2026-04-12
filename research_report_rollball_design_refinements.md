# Rollball 技术方案细化设计：四个关键问题分析报告

## 概要

本报告针对 Rollball "Agent as APP" 平台设计中的四个细化问题进行深度分析：manifest 文件格式选型（TOML vs JSON）、Tools 跨平台兼容性声明、Agent 重新打包时的用户隐私保护、以及记忆的云端选择性同步。综合技术调研和现有设计文档的分析，报告为每个问题提出了具体的解决方案和设计建议。

---

## 问题一：Manifest 文件格式——TOML vs JSON

### 现状

当前设计文档 v3.0 中，manifest 使用 JSON 格式（`manifest.json`），而配置文件（`settings.toml`）和 Skill 定义（`SKILL.toml`）已经使用 TOML 格式。项目中存在格式混用的情况。

### TOML 与 JSON 结构能力对比

TOML 在结构表达上确实存在一些 JSON 没有的限制，但需要区分"理论限制"和"实践影响"：

**TOML 的结构限制**：

1. **混合类型数组被禁止**（TOML 1.0+ 规范）：`[1, "string"]` 非法。但这对 manifest 场景几乎不是问题——manifest 中的数组都是同质的（字符串数组、对象数组等）。
2. **内联表不可扩展**：`type = { name = "Nail" }` 定义后不能追加属性。这只影响多行展开的写法，不影响表达能力。
3. **表不可重复定义**：一旦定义 `[llm.providers.openai]`，不能再写同名表头。这是规范约束，不是表达能力缺陷。
4. **深层嵌套变得冗长**：4层以上嵌套需要重复表头路径如 `[llm.providers.openai.routing.retry]`，可读性下降。这是实际影响最大的一个限制。
5. **禁止向静态数组追加**：写了 `tools = []` 后就不能再用 `[[tools]]` 追加。需要二选一写法。

**TOML 不比 JSON 差的方面**：

- 树状/嵌套结构：TOML 通过点分键和表头完全支持嵌套，表达力等价于 JSON 对象。当前 manifest.json 中的结构最深层约 4 层（如 `llm.providers.openai.params.temperature`），在 TOML 中可以用 `[llm.providers.openai.params]` 清晰表达。
- 类型系统：TOML 比 JSON 更强——原生支持日期时间、严格区分整数和浮点数。
- 注释：TOML 原生支持 `#` 注释，JSON 完全不支持。这对人工维护的 manifest 至关重要。

### Rust 生态支持

TOML 是 Rust 生态的事实标准配置格式。Cargo.toml、rustfmt.toml、clippy.toml、rust-analyzer.toml 等全部使用 TOML。`toml` crate 和 `serde_json` crate 都基于 serde，反序列化为 Rust 结构体的代码完全一致——只需改 derive 的格式名。解析性能上 JSON 略快，但 manifest 文件通常只解析一次，性能差异可忽略。

### 建议方案

**将 manifest 从 JSON 迁移到 TOML，统一整个项目的配置格式。**

具体设计：

```toml
# manifest.toml

agent_id = "com.example.weather"
version = "1.0.0"
name = "Weather Agent"
description = "查询实时天气并建议穿衣"
author = "example@domain.com"
runtime_version = "^1.0.0"

permissions = [
    "network:https://api.weather.com",
    "filesystem:read:~/Documents",
    "memory:read",
    "memory:write",
    "intent:send:com.example.calendar",
]

identity_deps = ["name", "city", "language", "timezone"]

[triggers]
schedule = { cron = "0 7 * * *" }
message = { pattern = "天气|weather" }

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
daily_token_limit = 100_000
daily_cost_limit_usd = 5.0
action_on_exhaust = "fallback_to_local"

[memory]
sync_mode = "auto"
cache_ttl = 3600
required = false

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

[capabilities.query_weather]
# 声明本 Agent 可被其他 Agent 通过 Intent 调用的能力

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

对比 JSON 版本，TOML 版本注释友好、层次清晰、数值类型精确（整数就是整数）、大数字可用下划线分隔（`100_000`）。唯一需要注意的是 `[[tools]]` 数组表语法——每个 tool 是一个 `[[tools]]` 块，这比 JSON 的数组写法稍显冗长但更清晰。

**迁移风险极低**：因为 serde 的统一抽象，Rust 代码只需将 `serde_json::from_str` 改为 `toml::from_str`，结构体定义完全不变。

---

## 问题二：Tools/Skills 的跨平台兼容性声明

### 问题分析

Agent 的核心设计理念是"一次打包，多平台运行"，但 Tools 存在平台依赖——例如 `shell` 工具只在桌面端可用，`bluetooth` 工具只在移动端可用。需要一个机制让 Agent 声明这些差异，并在不兼容平台上给出适当处理。

### 业界参考

**Android `<uses-feature>` 机制**是最成熟的参考模型：

- `android:required="true"`：没有此功能的设备无法安装（商店过滤）
- `android:required="false"`：没有此功能的设备可以安装，但应用必须在运行时检查 `PackageManager.hasSystemFeature()`

**npm 的 `os`/`cpu` 字段**提供了另一种模式：

- 白名单：`"os": ["darwin", "linux"]`
- 黑名单：`"os": ["!win32"]`
- 不匹配时阻止安装

**Flutter pubspec.yaml** 的 `platforms:` 块按平台分别声明插件实现，不支持的平台直接不列出。

### 建议方案

采用 **Android 的 required/optional 模式 + npm 的平台枚举模式**，在 manifest.toml 中为 tools 和 skills 增加 `platforms` 声明：

```toml
[[tools]]
name = "shell"
type = "builtin"
platforms = ["desktop:windows", "desktop:macos", "desktop:linux"]
# 布尔值升级为列表，支持子平台精确声明

[[tools]]
name = "bluetooth"
type = "builtin"
platforms = ["mobile:android", "mobile:ios"]
required = false  # 可选工具，不支持时跳过

[[tools]]
name = "http_get"
type = "builtin"
# 不声明 platforms 表示全平台可用

[[tools]]
name = "image_filter"
type = "wasm"
binary = "./tools/image_filter.wasm"
platforms = ["wasm"]  # 明确标注为 WASM 字节码，天然跨平台

[[tools]]
name = "local_model"
type = "builtin"
platforms = ["desktop", "server"]  # desktop/desktop:all 作为简写
version_constraint = ">=1.5.0"  # 声明最低版本要求
```

**Skill 也同理**：

```toml
# skills/image-edit/SKILL.toml
name = "image-edit"
description = "图片编辑能力"

[skill]
# Skill 依赖的 tools，如果某个 tool 在当前平台不可用，
# 该 skill 自动降级为不可用
tool_deps = ["image_filter", "shell"]

platforms = ["desktop"]
required = false  # 可选 skill
```

**平台枚举定义**：

```rust
enum PlatformLevel {
    Os,        // 操作系统级：windows, macos, linux, android, ios
    Form,      // 形态级：desktop, mobile, web, server
    Arch,      // 架构级：x86_64, arm64, wasm32
}

struct PlatformSpec {
    level: PlatformLevel,
    values: Vec<String>,
    version_constraint: Option<VersionConstraint>,
}

struct PlatformSupport {
    desktop: Vec<String>,      // ["windows", "macos", "linux"] 或 ["all"]
    mobile: Vec<String>,       // ["android", "ios"] 或 ["all"]
    web: bool,                 // WebAssembly in browser
    server: Vec<String>,       // ["linux", "windows"] 或 ["all"]
}
```

**平台声明的简写与完整写法**：

| 简写 | 展开 |
|------|------|
| `desktop` | `desktop:all`（所有桌面平台）|
| `mobile` | `mobile:all`（所有移动平台）|
| `["desktop", "mobile"]` | 桌面和移动全平台 |
| `["server"]` | 所有服务器平台 |
| `wasm` | 特殊标记：WASM 字节码，与具体 OS 无关 |

**版本约束**：

```toml
# 单个版本约束
version_constraint = ">=1.0.0"

# 多个约束
version_constraint = ">=1.0.0, <2.0.0"
platforms = ["desktop:windows", "desktop:macos >=1.5.0"]
```

**WASM 的明确声明**：

`platforms = ["wasm"]` 是独立于 OS 层级的特殊标记，表示该工具以 WebAssembly 字节码分发。Agent Runtime 在任何平台上遇到 `wasm` 标记，都尝试加载 WASM 运行时（wasmer/wasmtime）执行，无需关心底层 OS。

**Server 平台的澄清**：

Server（Headless）模式下的工具可用性规则：
- `shell` 工具：可用（Linux/macOS server 环境）
- `filesystem` 工具：可用（服务器有文件系统）
- `bluetooth` 工具：不可用（服务器无蓝牙硬件）
- `http_get` 工具：可用

```toml
[[tools]]
name = "server_metrics"
type = "builtin"
platforms = ["server"]
description = "服务器指标采集，仅 server 模式可用"
```

**运行时行为**：

1. **安装时检查**：Gateway 的 Package Manager 解析 manifest，将 `platforms` 列表与当前平台进行匹配（支持子平台精确匹配和 `desktop`/`mobile` 等简写匹配）。如果有 `required = true` 的 tool 在当前平台不匹配，安装时给出警告："此 Agent 的以下功能在当前平台不可用：shell（仅 windows/macos/linux）。是否继续安装？"
2. **版本约束检查**：如果 tool 声明了 `version_constraint`，Runtime 在加载时验证当前平台版本是否满足约束，不满足则视为该 tool 不可用。
3. **运行时降级**：对于 `required = false` 的 tools/skills，Agent Runtime 启动时根据当前平台过滤可用工具列表，不可用的 tool 不注入 system prompt，LLM 不会尝试调用它。
4. **Skill 级联降级**：如果 Skill 依赖的 tool 在当前平台不可用，该 Skill 自动降级为不可用，并在 Agent 的能力描述中标注。
5. **WASM 工具加载**：Agent Runtime 检测到 `platforms = ["wasm"]` 的 tool 时，初始化 WASM 运行时（wasmer/wasmtime），无论底层 OS 是什么。
6. **能力声明同步**：Agent 注册到 Gateway 的 capabilities 中，也需要反映平台差异。其他 Agent 通过 Intent 调用时，Gateway 可以在路由时检查目标 Agent 的当前平台能力。

**安装体验设计**：

```
$ rollball install com.example.weather

✅ Agent: Weather Agent v1.0.0
✅ Core tools available: http_get (all platforms), shell (desktop:windows/macos/linux)
⚠️  Optional tool "bluetooth" unavailable (mobile:android/ios only)
⚠️  Optional skill "image-edit" unavailable: dependency "shell" not available on web

Install anyway? [Y/n]
```

这套方案的优势：声明式（与 manifest 风格一致）、渐进式降级（不是全有或全无）、Skill 级联自动处理（不需要开发者手动处理每个依赖链）。

---

## 问题三：Agent 重新打包时的用户隐私保护

### 问题分析

Agent 可以"重新打包分享"是 Rollball 的核心分发模型。但一个运行过的 Agent 在其私有 Grafeo 中积累了大量与用户的交互记忆，其中必然包含用户个人信息。如果直接将整个工作区打包，用户隐私就会泄漏。

### 两种策略的对比

**策略 A：用户信息只保留在系统 Agent**

- 所有用户个人信息由系统 Agent 集中管理，其他 Agent 不存储原始用户信息
- Agent 需要用户信息时通过 Intent 查询系统 Agent，结果只在工作记忆（进程内存）中使用，不写入私有 Grafeo

优点：从根本上避免隐私泄漏，打包时不需要任何过滤逻辑
缺点：过度约束——Agent 的私有 Grafeo 中自然会产生与用户相关的记忆（如"用户喜欢简洁的回复"），这些不是 identity 级别的信息但仍然是个人偏好；每次都查询系统 Agent 增加延迟；Agent 离线时无法获取用户信息

**策略 B：打包时过滤用户敏感信息**

- Agent 正常运行时可以在私有 Grafeo 中存储用户相关记忆
- 打包时通过过滤机制剥离敏感信息

优点：Agent 运行时自治性好，不需要每次都查询系统 Agent
缺点：如何准确定义"敏感信息"是个难题；过滤可能不彻底；增加打包工具复杂度

### 建议方案：分层标记 + 打包过滤

采用**策略 B 为主、策略 A 为辅**的混合方案：

**第一层：Grafeo 中的记忆标记**

在每个记忆节点上增加元数据标签，标记其隐私级别：

```rust
enum PrivacyLevel {
    Public,     // 通用知识，可分享（如"北京冬天很冷"）
    Personal,   // 个人偏好，非敏感（如"用户喜欢简洁回复"）
    Sensitive,  // 敏感信息（如"用户叫张三，住北京"）
}
```

记忆写入时由 Agent 的 LLM 自动判断隐私级别（类似系统 Agent 用 LLM 判断身份信息的思路），也可以在 Agent 的 prompt 中通过规则辅助判断：

```markdown
## 记忆隐私规则
- 用户姓名、地址、电话、邮箱等 → Sensitive
- 用户偏好、习惯、风格 → Personal
- 通用事实、知识 → Public
```

**第二层：系统 Agent 的身份信息守门**

现有设计已经确立"身份信息由系统 Agent 管理"。强化这一点：

- 系统 Agent 管理的身份字段（name、city、language 等）是 Sensitive 级别
- 其他 Agent 通过 `identity_deps` 在启动时获取注入，注入信息只存在于工作记忆中，Agent 应避免将其写入私有 Grafeo
- 如果 Agent 的 LLM 确实需要记住某个身份相关信息（如天气 Agent 记住用户城市以便主动推送），应通过系统 Agent 的 observe 机制订阅变更，而非自行存储副本

**第三层：打包时的过滤机制**

`rollball-pack` 工具提供隐私过滤选项：

```bash
# 打包时自动剥离 Sensitive 级别的记忆
rollball pack --source ./workspace --privacy-filter=strip-sensitive

# 打包时剥离 Sensitive + Personal 级别（只保留 Public）
rollball pack --source ./workspace --privacy-filter=strip-personal

# 不做过滤（仅用于开发/测试）
rollball pack --source ./workspace --privacy-filter=none
```

过滤逻辑：

1. 扫描 Grafeo 中所有记忆节点
2. 移除 PrivacyLevel >= 配置阈值的节点及其关联边
3. 检查剩余图的连通性，修复因移除节点导致的孤立子图
4. 对移除节点后的空洞进行一致性检查

**第四层：Agent manifest 中的隐私声明**

```toml
[privacy]
# 声明此 Agent 会存储哪些类型的用户信息
stores_personal_data = true
data_categories = ["user_preferences", "interaction_history"]
# 声明打包时的默认隐私过滤策略
default_pack_filter = "strip-sensitive"
```

这让用户和审核系统在安装时就知道这个 Agent 的隐私行为。

### 为什么不完全采用策略 A

策略 A 虽然更安全，但与"Agent 是独立数字人"的仿生设计哲学冲突——一个数字人应该能记住与用户的交互经历。天气 Agent 记住用户住北京不是存储"身份信息"，而是它的"经验"。策略 A 会把这种经验也消灭掉。分层标记方案既保护了敏感信息，又保留了 Agent 的个性化能力。

---

## 问题四：记忆的云端选择性同步

### 问题分析

云端记忆同步是可选功能，但存在矛盾需求：企业用户希望通用知识同步到云端以便团队共享，个人敏感信息必须留在本地。这要求数据库设计严格区分个人敏感记忆和通用记忆，只有非个人信息才上云。

### 业界参考

**Apple CloudKit 的分区模型**提供了优雅的参考：

- Private Database：仅用户本人可访问
- Shared Database：通过 CKShare 机制选择性共享
- Zone（区域）：数据库内部的逻辑分区，支持增量同步
- 共享数据的物理存储始终在所有者的 Private Database 中，被分享者通过映射查看

**Obsidian Sync 的选择性同步**采用文件级别控制：

- 按文件类型选择性同步（图片、音频、PDF 等）
- 按文件夹排除
- 设备特定设置永不同步

**CRDT 局部复制**模式（来自 Local-First 社区实践）：

- 按子树路径/分片订阅，客户端只同步需要的数据
- 使用 Macaroon 风格的能力令牌控制访问范围
- 端到端加密 + 密钥分发控制可见性

### 建议方案：Zone-Based 分区 + Privacy Tag 过滤

**核心思路**：在 Grafeo 中引入 Zone 概念，Zone 是记忆的逻辑分区，每个 Zone 有独立的同步策略和隐私级别。

**Grafeo Zone 设计**：

```rust
struct GrafeoZone {
    id: ZoneId,
    name: String,
    privacy_level: PrivacyLevel,  // Public / Personal / Sensitive
    sync_policy: SyncPolicy,
}

enum SyncPolicy {
    LocalOnly,           // 永不上云
    CloudSync,           // 允许云端同步
    CloudSyncEncrypted,  // 端到端加密后上云
}
```

**默认 Zone 划分**：

| Zone | 内容 | Privacy Level | Sync Policy | 示例 |
|------|------|---------------|-------------|------|
| `identity` | 用户身份相关记忆 | Sensitive | LocalOnly | "用户叫张三" |
| `preferences` | 用户偏好 | Personal | LocalOnly | "用户喜欢简洁回复" |
| `knowledge` | 通用知识 | Public | CloudSync | "北京冬天平均温度-5°C" |
| `enterprise` | 企业知识库 | Public | CloudSyncEncrypted | "公司季度目标" |
| `interaction` | 交互历史 | Personal | LocalOnly | "上周用户问了天气" |

**Agent 可在 manifest 中声明自定义 Zone**：

```toml
[memory]
sync_mode = "auto"  # auto / manual / disabled

# 覆盖默认 Zone 的同步策略
[[memory.zones]]
name = "enterprise"
sync_policy = "cloud_sync_encrypted"
encryption_key_ref = "vault:enterprise_key"

# 声明自定义 Zone
[[memory.zones]]
name = "project_alpha"
privacy_level = "public"
sync_policy = "cloud_sync"
```

**记忆写入时的 Zone 路由**：

Agent 的 LLM 在存储记忆时，同时决定记忆归属的 Zone。这可以通过 prompt 规则引导：

```markdown
## 记忆分区规则
- 涉及用户姓名、地址、联系方式 → identity zone
- 涉及用户偏好、习惯 → preferences zone
- 通用事实、领域知识 → knowledge zone
- 企业特定信息 → enterprise zone
- 交互历史 → interaction zone
```

记忆节点的数据结构：

```rust
struct MemoryNode {
    id: NodeId,
    zone: ZoneId,          // 所属 Zone
    content: String,
    embedding: Vec<f32>,
    privacy_level: PrivacyLevel,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    // ... 其他字段
}
```

**同步流程**：

```
Agent 写入记忆
    │
    ▼
Grafeo 根据 Zone + PrivacyLevel 路由
    │
    ├─ Zone.sync_policy = LocalOnly → 仅写入本地 Grafeo 文件
    │
    └─ Zone.sync_policy = CloudSync / CloudSyncEncrypted
        │
        ├─ 写入本地 Grafeo 文件
        └─ 异步推送到 Memory Sync Service
            │
            ├─ CloudSync: 明文传输（TLS 加密传输层）
            └─ CloudSyncEncrypted: 端到端加密后传输
```

**增量同步机制**：

每个 Zone 维护独立的版本向量（Version Vector），同步时只交换增量：

```rust
struct ZoneSyncState {
    zone_id: ZoneId,
    version_vector: HashMap<DeviceId, u64>,  // 每个设备的版本号
    last_sync_timestamp: DateTime<Utc>,
}
```

同步请求：

```
客户端 → 服务端：
  "给我 enterprise zone 中 version > 我的版本向量的变更"

服务端 → 客户端：
  返回增量 Ops 列表
```

**冲突解决**：采用 LWW（Last Writer Wins）策略，与现有设计一致。对于 enterprise zone，可配置为 CRDT 合并策略。

**企业场景的额外支持**：

企业用户可以配置"企业知识库 Agent"，它拥有一个 `enterprise` Zone，sync_policy 为 `CloudSyncEncrypted`。其他 Agent 的 enterprise Zone 可以订阅这个知识库 Agent 的更新，通过 Intent 机制获取企业知识，而非直接访问共享数据库——这保持了"Agent 间不共享数据库"的设计原则。

**个人敏感信息的保障**：

1. `identity` 和 `preferences` Zone 的 sync_policy 强制为 `LocalOnly`，不可配置
2. 即使用户误将 sync_policy 改为 CloudSync，隐私级别为 Sensitive 的记忆节点仍会被同步过滤器拦截
3. 双重保障：Zone 级别 + 节点 PrivacyLevel 级别

### 与问题三的衔接

Zone-Based 分区方案与问题三的隐私保护方案天然衔接：

- **重新打包时**：`rollball-pack --privacy-filter=strip-sensitive` 只需移除 `identity` 和 `preferences` Zone 的全部数据，以及 `knowledge` Zone 中 PrivacyLevel 为 Sensitive 的节点
- **云端同步时**：Zone 的 sync_policy 决定是否上云，PrivacyLevel 作为二级过滤
- 两个场景共享同一套元数据标记，无需维护两套隐私定义

---

## 总结

| 问题 | 核心决策 | 关键设计要点 |
|------|---------|-------------|
| Manifest 格式 | TOML 替换 JSON | 统一配置格式，serde 无缝迁移，注释和类型系统是核心优势 |
| 跨平台兼容性 | required/optional + 子平台精确声明 + 版本约束 | platforms 列表支持子平台细粒度匹配，wasm 独立标记，version_constraint 支持 |
| 隐私保护 | 分层标记 + 打包过滤 | PrivacyLevel 三级分类，LLM 自动标记，打包工具过滤 |
| 选择性同步 | Zone-Based 分区 + Privacy Tag | Zone 控制同步策略，PrivacyLevel 控制节点级过滤，双重保障 |

四个方案共享一个设计哲学：**声明式优先，运行时自治，边界清晰**——与 Rollball 整体的"Agent as APP"架构保持一致。

## References

1. [TOML v1.1.0 Specification](https://toml.io/en/v1.1.0)
2. [TOML v1.0.0 Specification](https://toml.io/en/v1.0.0)
3. [JSON vs YAML vs TOML: Complete Comparison Guide](https://dev.to/_d7eb1c1703182e3ce1782/json-vs-yaml-vs-toml-complete-comparison-guide-2142)
4. [Android uses-feature Element](https://developer.android.com/guide/topics/manifest/uses-feature-element)
5. [npm package.json Documentation](https://docs.npmjs.com/cli/v11/configuring-npm/package-json)
6. [Flutter pubspec options](https://docs.flutter.dev/tools/pubspec)
7. [Homebrew Formula Cookbook](https://docs.brew.sh/Formula-Cookbook)
8. [Apple CloudKit Shared Records](https://developer.apple.com/documentation/cloudkit/shared-records)
9. [Local-First Apps in 2025: CRDTs, Replication Patterns](https://debugg.ai/resources/local-first-apps-2025-crdts-replication-edge-storage-offline-sync)
10. [Local-First Software: Principles, Patterns, and Technologies](https://wal.sh/research/local-first.html)
11. [RFC 9614: Partitioning as an Architecture for Privacy](https://www.ietf.org/rfc/rfc9614.pdf)
12. [Obsidian Sync Settings and Selective Syncing](https://deepwiki.com/victor-software-house/obsidian-help/5.4-sync-settings-and-selective-sync)
13. [TOML Mixed-type Arrays Issue #553](https://github.com/toml-lang/toml/issues/553)
