# 17-web-search-provider — Web Search 可配置 Provider 系统

**版本**: v1.0
**状态**: 设计阶段
**创建日期**: 2026-05-26

## 1. 问题概述

当前内置 `web_search` 工具硬编码依赖 DuckDuckGo HTML 爬虫，因 DDG 限流导致实际不可用。
需要重构为可配置的 Web Search Provider 系统，参照 LLM Provider 配置链路，
让用户能自由选择 Tavily / Brave / Firecrawl 等主流搜索供应商。

### 1.1 当前问题

- [web_search.rs](file:///d:/projects/rust/agent-study/core/rollball-runtime/src/tools/builtin/web_search.rs) 硬编码 DuckDuckGo HTML 爬虫
- DDG 频繁限流，搜索成功率极低
- 无备选方案，工具形同虚设
- API Key 管理缺失（web_search 无需 key 是因为 DDG 不需要，但主流供应商都需要）

### 1.2 设计目标

| 目标 | 说明 |
|------|------|
| **Provider 可配置** | 用户可选择 Tavily / Brave / Firecrawl 等供应商 |
| **多 Provider 优先级** | 支持配置多个搜索 Provider，按优先级 fallback |
| **API Key 通过 Vault 管理** | 复用现有 Vault 体系，类似 LLM Provider |
| **配置链路对齐 LLM** | 完全复制 LLM Provider 的 app→gateway→runtime 配置链路 |
| **与 MCP 共存** | 内置 `web_search` 和 MCP `web_search` 互不冲突 |

---

## 2. 架构总览

### 2.1 与 LLM Provider 配置链路的对应关系

```
┌──────────────────────────────────────────────────────────────────┐
│                    LLM Provider 链路 (已有)                        │
│                                                                   │
│  Desktop App          Gateway                 Runtime              │
│  ┌──────────┐    ┌──────────────┐    ┌──────────────────┐        │
│  │ Providers │───▶│ VaultFacade   │───▶│ AgentHelloResult │        │
│  │ Tab       │    │ (加密存储)    │    │ .provider_key_   │        │
│  │          │    │              │    │   vault[]        │        │
│  │ add_key  │    │ LLMConfig-   │    │                  │        │
│  │ remove   │    │ Delivery     │    │ SessionManager   │        │
│  │ update   │    │ (gRPC push)  │    │ .update_llm_     │        │
│  └──────────┘    └──────────────┘    │   config()       │        │
│                                      │                  │        │
│                                      │ Provider Router  │        │
│                                      │ .create_provider │        │
│                                      └──────────────────┘        │
└──────────────────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────────────────┐
│                 Web Search Provider 链路 (新增)                    │
│                                                                   │
│  Desktop App          Gateway                 Runtime              │
│  ┌──────────┐    ┌──────────────┐    ┌──────────────────┐        │
│  │ Search   │───▶│ VaultFacade   │───▶│ AgentHelloResult │        │
│  │ Tab      │    │ (加密存储)    │    │ .search_key_     │        │
│  │ (新增)   │    │              │    │   vault[]        │        │
│  │          │    │ SearchConfig │    │ (新增)           │        │
│  │ add_key  │    │ Delivery     │    │                  │        │
│  │ remove   │    │ (gRPC push)  │    │ SessionManager   │        │
│  │ update   │    │ (新增)       │    │ .update_search_  │        │
│  └──────────┘    └──────────────┘    │   config()       │        │
│                                      │ (新增)           │        │
│                                      │                  │        │
│                                      │ WebSearchTool    │        │
│                                      │ (重构)           │        │
│                                      └──────────────────┘        │
└──────────────────────────────────────────────────────────────────┘
```

### 2.2 核心原则

1. **每 Agent 独立配置**：web_search provider 优先级列表存储在 `agent_search.json`（per-agent workspace），gateway 只管理 Vault Key
2. **Vault Key 一次性分发**：API Key 通过 AgentHelloResult 分发，Runtime 仅内存持有，不落盘
3. **配置热更新**：用户修改后通过 gRPC push 通知 Runtime 热重载
4. **无 API Key = 降级不可用**：没有配置任何搜索 Provider 时，`web_search` 工具返回明确错误

---

## 3. Provider 设计

### 3.1 支持的 Provider

| Provider | API 端点 | 免费额度 | 特点 |
|----------|---------|----------|------|
| **Tavily** | `https://api.tavily.com/search` | 1000 次/月 | AI 优化搜索，最流行 |
| **Brave Search** | `https://api.search.brave.com/res/v1/web/search` | 2000 次/月 | 隐私优先，独立索引 |
| **Firecrawl** | `https://api.firecrawl.dev/v1/search` | 500 次/月 | 爬取能力强，Markdown 输出 |
| **SearXNG** | 自托管实例 URL | 无限 | 开源元搜索引擎，需自托管 |

### 3.2 数据结构

```rust
/// Web Search provider identifier
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SearchProvider {
    Tavily,
    Brave,
    Firecrawl,
    SearXNG,
}

/// Web Search provider configuration (per-Provider, per-Agent)
///
/// Stored in agent workspace as `agent_search.json`.
/// Only the provider name and priority are persisted; API keys come from Vault.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchProviderConfig {
    /// Provider identifier
    pub provider: String,               // "tavily", "brave", "firecrawl", "searxng"
    /// Priority: lower = higher priority (for fallback chain)
    pub priority: u32,
    /// Custom base URL (required for SearXNG, optional for others)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Enabled flag — allows disabling without removing
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool { true }

/// Search key entry — delivered by Gateway to Runtime via AgentHelloResult.
/// Mirrors ProviderKeyEntry pattern for LLM keys.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchKeyEntry {
    /// Search provider identifier
    pub provider_id: String,
    /// Decrypted API key
    pub api_key: String,
}
```

### 3.3 API 协议（Rust 侧）

每个 Provider 实现统一的 `SearchBackend` trait：

```rust
/// Search request
#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub query: String,
    pub max_results: u32,
}

/// Search result item
#[derive(Debug, Clone, Serialize)]
pub struct SearchResultItem {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Search backend trait — implemented by each provider
#[async_trait]
pub trait SearchBackend: Send + Sync {
    /// Provider identifier (e.g. "tavily")
    fn provider_name(&self) -> &str;
    
    /// Execute a web search
    async fn search(&self, request: &SearchRequest) -> Result<Vec<SearchResultItem>, SearchError>;
}
```

每个 Provider 实现各自的 HTTP 调用（纯 reqwest，无额外依赖）：
- **Tavily**: POST `https://api.tavily.com/search`, header `Authorization: Bearer <key>`
- **Brave**: GET `https://api.search.brave.com/res/v1/web/search?q=...`, header `X-Subscription-Token: <key>`
- **Firecrawl**: POST `https://api.firecrawl.dev/v1/search`, header `Authorization: Bearer <key>`
- **SearXNG**: GET `{base_url}/search?q=...&format=json`, 无需 API key

---

## 4. 配置链路详解

### 4.1 总体数据流

完全对标 LLM Provider 的两条数据流：

```
┌─────────────────────────────────────────────────────────────────────┐
│  类型        │  存储位置        │  版本化?  │  推送时机              │
├──────────────┼─────────────────┼──────────┼────────────────────────┤
│ Provider     │ provider_list    │  ✅ 是   │ AgentHello 差量同步     │
│ List         │ .json (Gateway)  │          │ + 热更新 push          │
│              │ (versioned)      │          │                        │
├──────────────┼─────────────────┼──────────┼────────────────────────┤
│ Key Vault    │ rollball-vault   │  ❌ 否   │ AgentHello 始终全量     │
│              │ (加密存储)       │          │ + 热更新 push          │
├──────────────┼─────────────────┼──────────┼────────────────────────┤
│ Per-Agent    │ agent workspace  │  ❌ 否   │ Runtime 自己管理        │
│ Config       │ .agent_search    │          │ (per-agent priority)    │
│              │ .json            │          │                        │
└──────────────┴─────────────────┴──────────┴────────────────────────┘
```

### 4.2 Resource Cache 版本号差量同步机制

这是 LLM Provider 和 MCP 已验证的成熟机制，Web Search 完全复用。

#### 4.2.1 现有机制回顾

Gateway 在 `{data_dir}/` 下维护三个版本化缓存文件：

```
{data_dir}/
├── provider_list.json     ← LLM Provider 列表 (已有)
├── mcp_list.json          ← MCP Server 列表 (已有)
└── search_list.json       ← Web Search Provider 列表 (新增)
```

每个文件格式：

```json
// search_list.json (新增)
{
  "version": 3,
  "providers": [
    {
      "id": "tavily",
      "name": "Tavily Search",
      "description": "AI-optimized search API",
      "requires_api_key": true,
      "base_url": "https://api.tavily.com"
    }
  ]
}
```

**关键规则：**
- **版本号单调递增**（`wrapping_add(1)`），从来不会回退
- **Gateway 启动时**总是从 Vault 重建缓存文件，确保 in-memory 状态和磁盘一致
- **用户修改资源时**，HTTP handler 重建对应缓存文件（version+1），更新 in-memory cache，然后 hot-push 到所有已连接的 Agent

#### 4.2.2 AgentHello 差量同步流程

这是 Runtime 启动时获取 Search Provider 资源的完整流程：

```
Runtime                              Gateway
  │                                     │
  │  ① 读取本地 resource_cache.json     │
  │     { search_list_version: 0 }      │  (首次启动 version=0)
  │     { search_list_version: 3 }      │  (后续启动 version=3)
  │                                     │
  │  ② AgentHello ──────────────────▶  │
  │     .search_list_version: 3         │   Runtime 发送本地缓存版本
  │                                     │
  │                           ③ Gateway 比较版本号:
  │                              if Runtime.version < Gateway.version:
  │                                → 返回 search_list (差量)
  │                              else:
  │                                → search_list = None (已最新)
  │                              search_key_vault 始终全量返回
  │                                     │
  │  ④ AgentHelloResult ◄────────────  │
  │     .search_list:                   │
  │       None (Runtime 已是最新)        │
  │       Some([...]) (差量数据)         │
  │     .search_list_version: 5         │  Gateway 当前版本号
  │     .search_key_vault: [            │  始终全量
  │       { provider_id: "tavily",      │
  │         api_key: "tvly-..." }       │
  │     ]                               │
  │                                     │
  │  ⑤ Runtime 更新本地缓存             │
  │     save_resource_cache({           │  (保存到 workspace)
  │       search_list_version: 5        │
  │     })                              │
  │     + 更新 AgentCore 的             │
  │       WebSearchTool backends        │
```

#### 4.2.3 热更新推送流程

用户通过 Desktop App 修改 Search 配置后：

```
Desktop App          Gateway                            Runtime
  │                    │                                    │
  │  add_search_key    │                                    │
  │ ──────────────────▶│                                    │
  │  (tavily, api_key) │                                    │
  │                    │ ① VaultFacade.store_search_key()   │
  │                    │    加密保存到 rollball-vault        │
  │                    │                                    │
  │                    │ ② resource_cache::                 │
  │                    │    rebuild_and_save_search_cache()  │
  │                    │    重建 search_list.json            │
  │                    │    version = old.wrapping_add(1)    │
  │                    │    更新 in-memory ResourceCache     │
  │                    │                                    │
  │                    │ ③ 热推送                            │
  │                    │    SearchConfigDelivery ──────────▶│
  │                    │    .search_key_vault: [全量]        │
  │                    │    .search_list: [全量]             │
  │                    │    .search_list_version: 6          │
  │                    │                                    │
  │                    │                   ④ SessionManager  │
  │                    │                     .update_search_ │
  │                    │                     config()        │
  │                    │                     → 更新缓存       │
  │                    │                     → 广播到 Session │
  │                    │                   ⑤ 保存 resource_  │
  │                    │                     cache.json      │
  │                    │                     version = 6     │
```

#### 4.2.4 为什么 Search Provider List 需要版本化？

| 特性 | LLM Provider List | Search Provider List |
|------|------------------|---------------------|
| 数据量 | 大（每个 provider 含模型列表 + capabilities） | 小（4 个固定 provider） |
| 变更频率 | 低（用户配置后基本不动） | 低（用户配置后基本不动） |
| 传输成本 | 高，需要差量 | 实际上很低，但**架构一致性**更重要 |
| 版本化理由 | ✅ 避免每次传输完整模型列表 | ✅ 与 Provider/MCP 机制统一，**减少 Runtime 歧义** |

虽然 Search Provider List 数据量很小，但**版本化机制的价值不在于节省带宽，而在于：**

1. **确定性**：Runtime 明确知道自己持有的资源版本，不会出现新旧数据混合
2. **幂等性**：同一版本号重复推送不会导致重复重建
3. **可观测性**：日志中可跟踪 `search_list_version` 变化，方便排查问题
4. **架构一致性**：三种 resource 使用完全相同的模式，降低维护心智负担

#### 4.2.5 Runtime 侧 resource_cache.json（已有，扩展 search_list_version）

Runtime 在 agent workspace 中维护本地缓存，**该文件已存在**并存储全量 provider/mcp 列表：

```json
// workspace/config/resource_cache.json（已有的实际结构 + search_list_version）
{
  "provider_list_version": 20,
  "mcp_list_version": 21,
  "search_list_version": 0,
  "providers": [
    { "id": "alibaba-cn", "base_url": "...", "protocol_type": "openai", "models": [...] },
    { "id": "deepseek", "base_url": "...", "protocol_type": "openai", "models": [...] }
  ],
  "mcps": [
    { "id": "playwright", "name": "playwright", "transport": "stdio", ... }
  ]
}
```

- `providers` / `mcps`: 全量列表缓存（AgentHello 差量同步，版本号匹配则跳过传输）
- `search_list_version`: 新增字段。Search Provider 列表数据量极小，无需缓存全量
- 首次启动时 `search_list_version = 0`（未同步过），Gateway 返回全量 search_list
- 收到 AgentHelloResult 后更新版本号，下次启动传新版本
- 收到热推送 SearchConfigDelivery 后也更新版本号

### 4.3 Desktop App → Gateway 通信

与 LLM Provider 完全对齐：

| 操作 | LLM Provider | Web Search Provider |
|------|-------------|-------------------|
| 列出已配置 | `invoke("list_keys")` → `VaultKeyEntry[]` | `invoke("list_search_keys")` → `SearchKeyEntry[]` |
| 添加 | `invoke("add_key", { provider, key, ... })` | `invoke("add_search_key", { provider, key, ... })` |
| 删除 | `invoke("remove_key", { provider })` | `invoke("remove_search_key", { provider })` |
| 更新 | `invoke("update_key", { ... })` | `invoke("update_search_key", { ... })` |
| 测试 | `fetchProviderModels(provider)` → success/fail | 新增 `GET /api/search/test?provider=tavily` |

### 4.4 Gateway → Runtime 热推送协议

新增 gRPC message：

```protobuf
// gateway_ipc.proto 新增

message SearchConfigDelivery {
  // Search key vault entries (always full, not versioned)
  repeated SearchKeyEntry search_key_vault = 1;
  // Search provider list (always full on push, version-driven on AgentHello)
  repeated SearchProviderListItem search_list = 2;
  // Current search list version
  uint64 search_list_version = 3;
}

message SearchKeyEntry {
  string provider_id = 1;
  string api_key = 2;
}

message SearchProviderListItem {
  string id = 1;           // "tavily"
  string name = 2;         // "Tavily Search"
  string description = 3;  // "AI-optimized search API"
  bool requires_api_key = 4;
  string base_url = 5;     // default API endpoint
}
```

### 4.5 Runtime 侧接收与热更新

```rust
// SessionManager 新增方法
impl SessionManager {
    /// Update web search config for all sessions
    pub fn update_search_config(
        &mut self,
        search_key_vault: Vec<SearchKeyEntry>,
        search_list: Vec<SearchProviderListItem>,
        search_list_version: u64,
    ) {
        // 更新缓存 — mirrors CachedLLMConfig pattern
        self.cached_search_config = Some(CachedSearchConfig {
            key_vault: search_key_vault,
            provider_list: search_list,
        });
        
        // 持久化版本号到 resource_cache.json
        // (下次 AgentHello 时传递，避免重复传输)
        save_resource_cache_version(
            &self.work_dir,
            ResourceVersions {
                search_list_version: search_list_version,  // 新增
                // provider_list_version / mcp_list_version unchanged
                ..self.resource_versions.clone()
            }
        );
        
        // 广播到所有活跃 SessionTask
        let config = self.cached_search_config.clone().unwrap();
        for session in self.sessions.values() {
            let _ = session.search_config_tx.send(config.clone());
        }
    }
}

/// Cached search config — mirrors CachedLLMConfig pattern
#[derive(Clone)]
struct CachedSearchConfig {
    key_vault: Vec<SearchKeyEntry>,
    provider_list: Vec<SearchProviderListItem>,
}
```

### 4.6 文件命名规范与存储架构

本项目有两层存储，需要严格区分：

#### Gateway 全局资源（`{data_dir}/`）

这些是 Gateway 管理的全局资源，供所有 Agent 共用，版本化用于 AgentHello 差量同步：

| 文件 | 内容 | 版本字段 |
|------|------|---------|
| `provider_list.json` | 全量 LLM Provider 列表 + 模型 + capabilities | `provider_list_version` |
| `mcp_list.json` | 全量 MCP Server 列表 | `mcp_list_version` |
| `search_list.json` | **新增** — 全量 Search Provider 列表 | `search_list_version` |

#### Agent Workspace（`{work_dir}/config/`）

这些是 per-agent 配置，由 Runtime 管理，目前已有：

| 文件 | 内容 | 状态 |
|------|------|------|
| `agent_model.json` | Agent 当前使用的 LLM provider + model | ✅ 已有 |
| `agent_config.json` | max_output_tokens / max_iterations / temperature / active_tools / shell_approval_threshold | ✅ 已有 |
| `resource_cache.json` | Gateway 下发的全量 `providers` + `mcps` 列表 + 版本号 | ✅ 已有 |
| `mcp_servers.json` | Agent 从 Catalog 中选用的 MCP Server 子集 | ✅ 已有 → 🔄 重命名为 `agent_mcp.json` |
| `agent_search.json` | Agent 从全量 Search Provider 中选用的**子集** + 优先级 | 🆕 新增 |

**命名规则**：
- `agent_*.json` = per-agent 配置快照（Agent 自主选择的结果）
- `resource_cache.json` = Gateway 下发的全局资源 + 版本追踪（用于 AgentHello 差量同步）

#### 三层链路文件职责

```
Gateway ({data_dir}/)              Runtime ({work_dir}/config/)         Agent 决策
──────────────────────────────────────────────────────────────────────────────────
                                    
provider_list.json ───AgentHello──▶ resource_cache.json.providers      agent_model.json
(全量 Provider+模型)                 (缓存全量，版本号差量)                 (用户选的模型)
                                    
mcp_list.json ───────AgentHello──▶ resource_cache.json.mcps            mcp_servers.json → agent_mcp.json
(全量 Catalog)                      (缓存全量，版本号差量)                 (Agent 选的子集)
                                    
search_list.json ────AgentHello──▶ resource_cache.json                  agent_search.json
(全量 Search Provider)              (只存版本号)                          (Agent 选的子集+优先级)
```

**关键设计**：`agent_search.json` 不是全量列表的副本，而是该 Agent 从全局 `search_list.json` 中**选择的子集**。这避免了冗余存储，同时允许每个 Agent 独立配置不同的搜索供应商和优先级。

#### resource_cache.json 现状

已存在的 `resource_cache.json` 结构：

```json
{
  "provider_list_version": 20,
  "mcp_list_version": 21,
  "providers": [
    { "id": "alibaba-cn", "base_url": "...", "protocol_type": "openai", "models": [...] },
    { "id": "deepseek", "base_url": "...", "protocol_type": "openai", "models": [...] }
  ],
  "mcps": [
    { "id": "playwright", "name": "playwright", "transport": "stdio", "command": "npx", "args": ["-y", "@anthropic/mcp-playwright"] }
  ]
}
```

**Search 扩展后**增加：
```json
"search_list_version": 0,
```

不需要在 `resource_cache.json` 中缓存全量 Search Provider 列表（因为数据量极小，每次 AgentHello 直接传递即可），只需要版本号用于差量判断。

#### agent_search.json 格式

```json
// workspace/config/agent_search.json — Agent 从全量列表中选择的子集
{
  "providers": [
    { "provider": "tavily", "priority": 1 },
    { "provider": "brave",  "priority": 2 }
  ]
}
```

---

## 5. Vault 集成

### 5.1 Vault 存储方案

Web Search API Key 在 Vault 中的存储方式与 LLM Provider Key 相同，
但使用独立的 key namespace 以避免冲突：

| 存储项 | LLM Provider (已有) | Web Search Provider (新增) |
|--------|-------------------|--------------------------|
| Vault Key 命名 | `provider_id`（如 `"openai"`） | `"search:{provider_id}"`（如 `"search:tavily"`） |
| 存储内容 JSON | `{ api_key, base_url, models, ... }` | `{ api_key, base_url }` |
| VaultFacade 方法 | `store_provider()` / `get_provider()` | `store_search_provider()` / `get_search_provider()` |
| 列表方法 | `list_keys()` → `VaultKeyEntry[]` | `list_search_keys()` → `SearchKeyEntry[]` |

### 5.2 VaultFacade 新增接口

```rust
impl VaultFacade {
    // ── 新增方法 ──

    /// Store a web search API key
    pub fn store_search_key(&mut self, provider: &str, api_key: &str, base_url: Option<&str>)
        -> Result<(), GatewayError>;

    /// Get search provider entry (decrypted)
    pub fn get_search_provider(&self, provider: &str) -> Result<SearchProviderEntry, GatewayError>;

    /// List all configured search providers with masked previews
    pub fn list_search_keys(&self) -> Result<Vec<SearchKeyPreview>, GatewayError>;

    /// Remove a search provider key
    pub fn remove_search_key(&mut self, provider: &str) -> Result<(), GatewayError>;

    /// Get all search key entries for AgentHello delivery
    pub fn all_search_keys(&self) -> Result<Vec<SearchKeyEntry>, GatewayError>;
}
```

### 5.3 安全性

- API Key 通过 Argon2id + ChaCha20-Poly1305 加密存储（复用 rollball-vault）
- 分发后 Runtime 仅内存持有，使用后不落盘
- 传输通过 gRPC（本地连接，不出机器）
- 前端仅显示 masked preview（前3+后3字符）

---

## 6. 工具架构重构

### 6.1 当前实现

```rust
// 当前: hardcoded DuckDuckGo
pub struct WebSearchTool {
    client: reqwest::Client,  // 无状态，无配置
}
```

### 6.2 重构后

```rust
/// Web search tool — delegates to configured search backends
///
/// Backends are selected by priority from the agent's search_config.
/// API keys are resolved from the cached search config delivered by Gateway.
pub struct WebSearchTool {
    /// Search backends initialized from config (sorted by priority)
    backends: Vec<Box<dyn SearchBackend>>,
    /// Whether any backend is configured (controls error message quality)
    has_any_backend: bool,
}

impl WebSearchTool {
    /// Create from search config (called during tool registry build)
    pub fn from_config(
        provider_configs: &[SearchProviderConfig],
        key_vault: &[SearchKeyEntry],
    ) -> Self {
        let mut backends: Vec<Box<dyn SearchBackend>> = Vec::new();
        
        let mut sorted = provider_configs.to_vec();
        sorted.sort_by_key(|c| c.priority);
        
        for config in &sorted {
            if !config.enabled { continue; }
            
            let api_key = key_vault
                .iter()
                .find(|k| k.provider_id == config.provider)
                .map(|k| k.api_key.as_str());
            
            let backend: Option<Box<dyn SearchBackend>> = match config.provider.as_str() {
                "tavily" => Some(Box::new(TavilyBackend::new(api_key))),
                "brave" => Some(Box::new(BraveBackend::new(api_key))),
                "firecrawl" => Some(Box::new(FirecrawlBackend::new(api_key))),
                "searxng" => Some(Box::new(SearXNGBackend::new(
                    config.base_url.as_deref().unwrap_or("http://localhost:8888")
                ))),
                _ => None,
            };
            
            if let Some(b) = backend {
                backends.push(b);
            }
        }
        
        let has_any_backend = !backends.is_empty();
        Self { backends, has_any_backend }
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn spec(&self) -> ToolSpec { /* 不变 */ }

    async fn execute(&self, params: Value) -> Result<ToolResult> {
        let query = params["query"].as_str().unwrap_or("");
        if query.is_empty() {
            return Ok(ToolResult { ok: false, content: String::new(),
                error: Some("Missing 'query'".to_string()), token_usage: None });
        }
        
        let count = params["count"].as_u64().unwrap_or(5) as u32;
        
        if !self.has_any_backend {
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some("No web search provider configured. Please add a search provider in Harness > Search tab.".to_string()),
                token_usage: None,
            });
        }
        
        let request = SearchRequest { query: query.to_string(), max_results: count };
        
        // Try each backend in priority order (fallback chain)
        for (i, backend) in self.backends.iter().enumerate() {
            match backend.search(&request).await {
                Ok(results) => {
                    let joined = format_results(&results);
                    let (content, _) = output::truncate_output(&joined);
                    return Ok(ToolResult { ok: true, content, error: None, token_usage: None });
                }
                Err(e) => {
                    tracing::warn!(
                        provider = %backend.provider_name(),
                        error = %e,
                        "Search backend failed, trying next"
                    );
                    if i == self.backends.len() - 1 {
                        return Ok(ToolResult {
                            ok: false,
                            content: String::new(),
                            error: Some(format!("All search providers failed. Last error: {e}")),
                            token_usage: None,
                        });
                    }
                }
            }
        }
        
        unreachable!()
    }
}
```

### 6.3 移除 DuckDuckGo 爬虫

以下代码将被完全移除：
- `extract_ddg_results()` 函数及其 `regex` 依赖
- `strip_tags()` 辅助函数
- 内嵌的 `urlencoding` 模块（改用标准库 `url` crate 或 `percent_encoding`）

---

## 7. MCP vs 内置 web_search 共存策略

### 7.1 问题

当用户同时配置了内置 `web_search`（通过 Search Provider）和 MCP 提供的 `web_search`（如 Tavily MCP）时，
工具注册表中会出现两个同名工具。如何区分？

### 7.2 方案：LLM 自主选择

- **内置** `web_search`：使用当前 Search Provider 配置，通过 Vault 管理 key
- **MCP** `web_search`：通过 MCP Server 提供，工具名由 MCP Server 定义

因为 MCP 工具在注册时已经带了 server 前缀（如 `tavily-mcp__web_search`），而内置的是 `web_search`，
名称不会冲突。

```
Tool Registry:
  ├── web_search          ← 内置工具，使用 Search Provider 配置
  ├── web_fetch            ← 内置工具
  ├── tavily-mcp__search   ← MCP 工具（Tavily MCP Server 提供）
  └── brave-mcp__search    ← MCP 工具（Brave MCP Server 提供）
```

### 7.3 不同名的 MCP search 工具

如果用户通过 MCP 添加了一个工具名为 `web_search` 的 MCP server，
MCP 框架会加前缀 `{server_name}__web_search`，所以不会和内置工具冲突。

### 7.4 决策

- **内置 `web_search`**：平台基础设施，始终注册。没有配置 Provider 时返回错误。
- **MCP web_search**：用户自行管理，通过 MCP Tab 添加。
- **LLM 选择**：两个工具同时对 LLM 可见，LLM 根据工具描述自行选择，无需平台做特殊区分。

---

## 8. 前端 UI 设计

> **两个 UI 入口，不同职责**：
> - **HarnessPage → Search Tab**：管理 Gateway 全局的 Vault API Key + 全量 search_list.json
> - **Agent Setup 面板 → Search Block**：每个 Agent 独立选择激活的 search provider 子集 + 优先级，写入 `agent_search.json`

### 8.1 HarnessPage 新增 Search Tab（全局 Vault 管理）

```tsx
// HarnessPage.tsx 改动

// 当前 Tab 定义
type HarnessTab = "providers" | "mcp";

// 新增
type HarnessTab = "providers" | "search" | "mcp";

const tabs = [
  { id: "providers", label: "Providers" },
  { id: "search", label: "Search" },     // 新增
  { id: "mcp", label: "MCP" },
];
```

### 8.2 SearchTab 组件布局

**完全平行于 ProvidersTab 的布局结构**：

```
┌─────────────────────────────────────────────────┐
│  Provider Management                           │
│  ┌───────────────────────────────────────────┐ │
│  │  Configured Search Providers              │ │  ← 已配置列表（上方）
│  │  ┌─────────────────────────────────────┐ │ │
│  │  │ Tavily    ★ Default    Key: tvly-.. │ │ │
│  │  │ Active    [Edit] [Remove]           │ │ │
│  │  └─────────────────────────────────────┘ │ │
│  │  ┌─────────────────────────────────────┐ │ │
│  │  │ Brave                Key: BSA-...    │ │ │
│  │  │ Active    [Edit] [Remove]           │ │ │
│  │  └─────────────────────────────────────┘ │ │
│  └───────────────────────────────────────────┘ │
│                                                 │
│  ┌───────────────────────────────────────────┐ │
│  │  Available Search Providers              │ │  ← 可选列表（下方）
│  │  ┌─────────────────────────────────────┐ │ │
│  │  │ Tavily - AI-optimized search        │ │ │
│  │  │ 1000 free queries/month  [Add Key] │ │ │
│  │  └─────────────────────────────────────┘ │ │
│  │  ┌─────────────────────────────────────┐ │ │
│  │  │ Brave Search - Privacy-first        │ │ │
│  │  │ 2000 free queries/month  [Add Key] │ │ │
│  │  └─────────────────────────────────────┘ │ │
│  │  ┌─────────────────────────────────────┐ │ │
│  │  │ Firecrawl - Web scraping + search   │ │ │
│  │  │ 500 free credits/month   [Add Key] │ │ │
│  │  └─────────────────────────────────────┘ │ │
│  │  ┌─────────────────────────────────────┐ │ │
│  │  │ SearXNG - Self-hosted meta search   │ │ │
│  │  │ No API key required      [Add Key] │ │ │
│  │  └─────────────────────────────────────┘ │ │
│  └───────────────────────────────────────────┘ │
└─────────────────────────────────────────────────┘
```

### 8.3 与 ProvidersTab 的差异

| 项目 | ProvidersTab | SearchTab |
|------|-------------|-----------|
| 模型选择 | 有 | 无 |
| Base URL | 有（可编辑） | 有（SearXNG 必填，其他可选） |
| Model Capabilities | 有 | 无 |
| 优先级设置 | 无 | 有（上下箭头调整顺序） |
| Test 按钮 | 通过 fetch models 测试 | 通过搜索一个测试 query 验证 |
| Set Default | 有（Star 按钮） | 有（Star 按钮标记默认） |

### 8.4 编辑/添加对话框

```
┌─────────────────────────────────────┐
│  Add Search Provider                │
│                                     │
│  Provider: Tavily Search           │  (read-only)
│                                     │
│  API Key: [___________________]     │
│  Placeholder: "tvly-..."           │
│                                     │
│  Base URL: [___________________]    │  (optional)
│  Placeholder: https://api.tavily... │
│                                     │
│  Priority: [▼ 1st ──────────────] │  (dropdown: 1st, 2nd, 3rd...)
│                                     │
│  [Test Key]  [Cancel]  [Save]      │
└─────────────────────────────────────┘
```

### 8.5 前端数据流

```typescript
// 新增类型 (lib/types.ts)
interface SearchKeyEntry {
  provider: string;
  key_preview: string;      // "tvly-...abc"
  base_url?: string;
  priority: number;
}

interface SearchProviderDef {
  id: string;               // "tavily"
  name: string;             // "Tavily Search"
  description: string;      // "AI-optimized search API"
  requires_api_key: boolean;
  free_quota: string;       // "1000/mo"
  base_url: string;         // default endpoint
}

// 新增 Tauri Commands (src-tauri)
"list_search_keys"  → SearchKeyEntry[]
"add_search_key"    → { provider, key, base_url?, priority }
"remove_search_key" → { provider }
"update_search_key"  → { provider, key?, base_url?, priority }
```

### 8.6 搜索供应商静态列表

搜索供应商不需要 models.dev 那样的动态注册表（搜索供应商数量有限且稳定），
前端使用静态文件：

```typescript
// lib/search-providers.ts (新文件)
export const SEARCH_PROVIDERS: SearchProviderDef[] = [
  {
    id: "tavily",
    name: "Tavily Search",
    description: "AI-optimized real-time search API built for AI agents",
    requires_api_key: true,
    free_quota: "1,000 queries/month",
    base_url: "https://api.tavily.com",
  },
  {
    id: "brave",
    name: "Brave Search",
    description: "Privacy-first web search with independent index",
    requires_api_key: true,
    free_quota: "2,000 queries/month",
    base_url: "https://api.search.brave.com",
  },
  {
    id: "firecrawl",
    name: "Firecrawl",
    description: "Web scraping and search with markdown output",
    requires_api_key: true,
    free_quota: "500 credits/month",
    base_url: "https://api.firecrawl.dev",
  },
  {
    id: "searxng",
    name: "SearXNG",
    description: "Self-hosted privacy-respecting metasearch engine",
    requires_api_key: false,
    free_quota: "Unlimited (self-hosted)",
    base_url: "",
  },
];
```

### 8.7 Agent Setup 面板新增 Search 配置块

Agent Setup 面板（`AgentSetupTab.tsx`）需要新增一个 Search 功能块，
与已有的 MCP server 激活开关并列，用于**每个 Agent 独立选择搜索供应商和优先级**。

#### 职责区分

| 界面 | 操作对象 | 持久化位置 | 推送目标 |
|------|---------|-----------|---------|
| **Harness → Search Tab** | Gateway Vault Key + 全局 search_list.json | Gateway data_dir | 通过 gRPC 推送给 Runtime → resource_cache.json 版本号 |
| **Agent Setup → Search Block** | Agent 从 search_list 中选用子集 + 优先级 | `agent_search.json`（per-agent workspace） | 仅 Runtime 本地读取 |

#### 数据流

```
Harness/Search Tab                Gateway                    Runtime                       Agent Setup/Search Block
────────────────────────────────────────────────────────────────────────────────────────────────────────────────
① 用户添加 Tavily Key
   invoke("add_search_key")  ──▶  Vault.store_search_key()
                                  rebuild_and_save_search_cache()
                                  search_list.json version++
                                  
                                  SearchConfigDelivery ──────▶ update_search_config()
                                                               resource_cache.json
                                                               search_list_version = N

                                                               search_list 缓存在内存    ──▶ ② Agent Setup 读取全量 search_list
                                                                                            展示所有可选供应商
                                                                                            
                                                                                            ③ 用户勾选 Tavily + Brave
                                                                                               设置优先级 Brave > Tavily
                                                                                               
                                                                                            ④ Runtime 写入 agent_search.json:
                                                                                               { "providers": [
                                                                                                   { "provider": "brave", "priority": 1 },
                                                                                                   { "provider": "tavily", "priority": 2 }
                                                                                               ]}
```

#### UI 布局

Agent Setup 面板中，在 MCP section 下方新增 Search section：

```
┌─────────────────────────────────────────────────┐
│  Agent Setup — my-agent                         │
│                                                 │
│  ┌── Model ──────────────────────────────────┐ │
│  │  ... (已有)                                │ │
│  └────────────────────────────────────────────┘ │
│                                                 │
│  ┌── Tools ──────────────────────────────────┐ │
│  │  ☑ web_search   Built-in web search tool  │ │  ← 已有 tool toggle
│  │  ☑ ...                                    │ │
│  └────────────────────────────────────────────┘ │
│                                                 │
│  ┌── Web Search Providers ───────────────────┐ │  ← 🆕 新增 Search Block
│  │                                            │ │
│  │  Select and prioritize search providers   │ │
│  │  for this agent:                          │ │
│  │                                            │ │
│  │  ┌──────────────────────────────────────┐ │ │
│  │  │ ≡ Tavily Search              [✓ ON] │ │ │  ← toggle 激活
│  │  │   AI-optimized search       Prio: 2  │ │ │  ← 优先级
│  │  └──────────────────────────────────────┘ │ │
│  │  ┌──────────────────────────────────────┐ │ │
│  │  │ ≡ Brave Search               [✓ ON] │ │ │
│  │  │   Privacy-first web search  Prio: 1  │ │ │  ← 最高优先级
│  │  └──────────────────────────────────────┘ │ │
│  │  ┌──────────────────────────────────────┐ │ │
│  │  │ ≡ Firecrawl                  [  OFF]│ │ │  ← 未激活（灰色）
│  │  │   Web scraping + search               │ │ │
│  │  └──────────────────────────────────────┘ │ │
│  │  ┌──────────────────────────────────────┐ │ │
│  │  │ ≡ SearXNG                     [  OFF]│ │ │
│  │  │   Self-hosted meta search             │ │ │
│  │  └──────────────────────────────────────┘ │ │
│  │                                            │ │
│  │  ℹ️ No API key configured? Add in        │ │  ← 如果 Vault 中无 Key
│  │     Harness → Search tab first.           │ │
│  └────────────────────────────────────────────┘ │
│                                                 │
│  ┌── MCP Servers ───────────────────────────┐ │  ← 已有 MCP section
│  │  ... (已有)                               │ │
│  └────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────┘
```

#### 交互规则

1. **拖拽排序**：已激活的 provider 卡片支持拖拽上下移动，调整优先级。排在最前面的 = priority 1。
2. **Toggle 开关**：右侧 ON/OFF 开关控制该 provider 是否对该 Agent 可用。
   - OFF = 不写入 `agent_search.json`，Agent 无法使用该搜索供应商
   - ON = 加入 `agent_search.json.providers[]`，优先级由位置决定
3. **灰色提示**：如果某个 provider 在 Vault 中没有 API Key（如用户还没在 Harness/Search 中配置），显示为灰色 + tooltip 提示「No API key configured. Add in Harness → Search.」，但仍可切换 ON/OFF。
4. **自动保存**：每次 toggle 或排序结束即自动写入 `agent_search.json`，无需手动保存按钮。

#### 前端数据获取

```typescript
// AgentSetupTab.tsx 新增逻辑

// ① 从 Gateway API 获取全量 search_list（已配置 API Key 的供应商）
const [searchProviders, setSearchProviders] = useState<SearchProviderListItem[]>([]);

useEffect(() => {
  if (!selectedAgentId) return;
  // 查询这个 Agent 当前可用的 search provider 列表
  // 来源：Runtime 缓存的 search_list（通过 AgentHello 从 Gateway 获取）
  fetch(`${getGatewayUrl()}/api/agents/${selectedAgentId}/search-providers`)
    .then(res => res.json())
    .then(data => setSearchProviders(data.providers));
}, [selectedAgentId]);

// ② 从 Runtime 获取当前 agent 的激活 search 配置（agent_search.json 内容）
const [activeSearch, setActiveSearch] = useState<AgentSearchConfig>({ providers: [] });

useEffect(() => {
  if (!selectedAgentId) return;
  fetch(`${getGatewayUrl()}/api/agents/${selectedAgentId}/search-config`)
    .then(res => res.json())
    .then(data => setActiveSearch(data));
}, [selectedAgentId]);
```

#### 新增后端 API

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/agents/{id}/search-providers` | 查询该 Agent Runtime 当前缓存的 search_list（全量） |
| GET | `/api/agents/{id}/search-config` | 查询该 Agent 的 `agent_search.json`（激活子集 + 优先级） |
| PUT | `/api/agents/{id}/search-config` | Gateway 转发给 Runtime 写入 `agent_search.json` |

#### 类型定义

```typescript
// lib/types.ts 新增

interface SearchProviderListItem {
  id: string;               // "tavily"
  name: string;             // "Tavily Search"
  description: string;
  requires_api_key: boolean;
  base_url: string;
}

interface AgentSearchProvider {
  provider: string;         // "tavily"
  priority: number;         // 1 = highest
}

interface AgentSearchConfig {
  providers: AgentSearchProvider[];
}
```

#### Harness/Search Tab 与 Agent Setup/Search Block 的联动

```
Harness / Search Tab                    Agent Setup / Search Block
(全局管理)                                (per-agent 选择)

┌──────────────────────────┐            ┌──────────────────────────┐
│  Tavily    ★ Default     │            │  ☑ Tavily     Prio: 2    │
│  tvly-***  [Edit][Remove]│            │  ☑ Brave      Prio: 1    │
│                          │            │  ☐ Firecrawl             │
│  Brave                   │            │  ☐ SearXNG               │
│  BSA-***   [Edit][Remove]│            │                          │
│                          │            │  排序决定优先级           │
│  Firecrawl               │            │                          │
│  fc-***    [Edit][Remove]│            └──────────────────────────┘
│                          │
│  SearXNG                 │            当 Harness 中删除了某个 Key：
│  (no key)  [Edit][Remove]│            → Agent Setup 中该 provider 变灰
└──────────────────────────┘            → 自动从 agent_search.json 移除
                                         → 下次 AgentHello 拿到新 search_list
```

**关键联动**：当用户在 Harness/Search 中删除某个 provider 的 API Key 后，Gateway 通过 `SearchConfigDelivery` 推送新 search_list 给 Runtime。Agent Setup 面板读取 search_list 时发现该 provider 不再可用，自动将其从 agent_search.json 中移除并灰显。

---

## 9. 协议变更清单

### 9.1 rollball-core/protocol.rs

```rust
// ── 新增数据结构 ──

/// Search key entry — delivered by Gateway to Runtime via AgentHelloResult.
/// Key vaults are NOT versioned — always delivered in full.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchKeyEntry {
    pub provider_id: String,
    pub api_key: String,
}

/// Search provider list item — metadata for UI/Runtime resource delivery.
/// Versioned via search_list_version for AgentHello diff sync.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchProviderListItem {
    pub id: String,
    pub name: String,
    pub description: String,
    pub requires_api_key: bool,
    pub base_url: String,
}

// ── GatewayRequest::AgentHello 新增字段 ──

AgentHello {
    // ... 现有字段 ...
    agent_id: String,
    version: String,
    connection_role: String,
    provider_list_version: u64,    // 已有
    mcp_list_version: u64,         // 已有

    /// Runtime's cached search provider list version (0 = never synced)
    #[serde(default)]
    search_list_version: u64,      // 新增
}

// ── AgentHelloResult 新增字段 ──

AgentHelloResult {
    // ... 现有字段 ...
    success: bool,
    error: Option<String>,
    provider_list: Option<Vec<ProviderListItem>>,
    provider_list_version: u64,
    mcp_list: Option<Vec<McpListItem>>,
    mcp_list_version: u64,
    provider_key_vault: Vec<ProviderKeyEntry>,
    mcp_key_vault: Vec<McpKeyEntry>,
    identity_entries: Vec<IdentityEntry>,

    // ── Search provider 新增 ──
    /// Search provider list (None when Runtime version >= Gateway version)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    search_list: Option<Vec<SearchProviderListItem>>,     // 新增

    /// Gateway's current search list version
    #[serde(default)]
    search_list_version: u64,                              // 新增

    /// Search provider API keys — NEVER persisted to workspace disk
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    search_key_vault: Vec<SearchKeyEntry>,                  // 新增
}

// ── GatewayResponse 新增变体 ──

/// Hot-push delivery after user modifies search config
GatewayResponse::SearchConfigDelivery {
    search_list: Vec<SearchProviderListItem>,
    search_list_version: u64,
    search_key_vault: Vec<SearchKeyEntry>,
}
```

### 9.2 rollball-gateway/src/resource_cache.rs 新增

```rust
/// Versioned search provider list persisted to disk.
///
/// Mirrors ProviderListFile and McpListFile patterns.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SearchListFile {
    pub version: u64,
    pub providers: Vec<SearchProviderListItem>,
}

impl Default for SearchListFile {
    fn default() -> Self {
        Self { version: 0, providers: Vec::new() }
    }
}

/// ResourceCache 新增字段
pub struct ResourceCache {
    pub provider_list: ProviderListFile,  // 已有
    pub mcp_list: McpListFile,            // 已有
    pub search_list: SearchListFile,      // 新增
}

// ── 新增函数 ──

/// Rebuild search_list.json from Vault search provider entries.
/// Called by vault_api.rs handlers after add/update/delete search key.
pub fn rebuild_and_save_search_cache(
    gw: &mut GatewayState,
    data_dir: &Path,
) {
    let provider_names = gw.vault.list_search_providers();
    let mut providers = Vec::with_capacity(provider_names.len());

    for name in &provider_names {
        let entry = match gw.vault.get_search_provider(name) {
            Ok(e) => e,
            Err(_) => continue,
        };

        // Look up static metadata from built-in provider catalog
        let meta = SEARCH_PROVIDER_CATALOG.get(name.as_str())
            .cloned()
            .unwrap_or(SearchProviderListItem {
                id: name.clone(),
                name: name.clone(),
                description: String::new(),
                requires_api_key: true,
                base_url: entry.base_url.unwrap_or_default(),
            });

        providers.push(meta);
    }

    let new_version = gw.resource_cache.search_list.version.wrapping_add(1);
    let new_list = SearchListFile {
        version: new_version,
        providers,
    };

    if let Err(e) = save_search_list(data_dir, &new_list) {
        tracing::error!(error = %e, "Failed to save search_list.json after vault change");
    }
    gw.resource_cache.search_list = new_list;
}
```

### 9.3 gateway_ipc.proto

```protobuf
// ── AgentHelloRequest 新增字段 ──
message AgentHelloRequest {
  string agent_id = 1;
  string version = 2;
  string connection_role = 3;
  uint64 provider_list_version = 4;  // 已有
  uint64 mcp_list_version = 5;       // 已有
  uint64 search_list_version = 6;    // 新增
}

// ── AgentHelloResult 新增字段 ──
message AgentHelloResult {
  bool success = 1;
  string error = 2;

  string provider_list_json = 19;
  uint64 provider_list_version = 20;
  string mcp_list_json = 21;
  uint64 mcp_list_version = 22;

  // 新增
  string search_list_json = 27;      // JSON-serialized Vec<SearchProviderListItem>
  uint64 search_list_version = 28;

  string provider_key_vault_json = 23;
  string mcp_key_vault_json = 24;
  string search_key_vault_json = 29;  // 新增

  string identity_entries_json = 25;
}

// ── 新增 push message ──
message SearchConfigDelivery {
  string search_list_json = 1;
  uint64 search_list_version = 2;
  string search_key_vault_json = 3;
}

message SearchKeyEntry {
  string provider_id = 1;
  string api_key = 2;
}

message SearchProviderListItem {
  string id = 1;
  string name = 2;
  string description = 3;
  bool requires_api_key = 4;
  string base_url = 5;
}

// server_message 新增字段
message ServerMessage {
  // ... 现有字段 ...
  LLMConfigDelivery llm_config_delivery = 21;
  SearchConfigDelivery search_config_delivery = 33;  // 新增
}
```

### 9.4 Gateway handle_agent_hello 新增逻辑

```rust
// rollball-gateway/src/ipc/server.rs

pub async fn handle_agent_hello(
    agent_id: &str,
    version: &str,
    connection_role: &str,
    provider_list_version: u64,
    mcp_list_version: u64,
    search_list_version: u64,              // 新增参数
    conn_id: &str,
    state: &SharedState,
    session_mgr: &SharedSessionMgr,
) -> GatewayResponse {
    // ... 现有逻辑 ...

    let gw = state.read().await;

    // ... 现有 provider_list / mcp_list 差量逻辑 ...

    // ── 新增: search_list 差量同步 ──
    let (search_list, gw_search_version) = if search_list_version < gw.resource_cache.search_list.version {
        (
            Some(gw.resource_cache.search_list.providers.clone()),
            gw.resource_cache.search_list.version,
        )
    } else {
        (None, gw.resource_cache.search_list.version)
    };

    // ── 新增: search_key_vault 始终全量 ──
    let search_key_vault: Vec<SearchKeyEntry> = gw
        .vault
        .list_search_providers()
        .iter()
        .filter_map(|name| {
            gw.vault.get_search_provider(name).ok().map(|entry| {
                SearchKeyEntry {
                    provider_id: name.clone(),
                    api_key: entry.api_key,
                }
            })
        })
        .collect();

    drop(gw);

    GatewayResponse::AgentHelloResult {
        // ... 现有字段 ...
        search_list,                     // 新增
        search_list_version: gw_search_version,  // 新增
        search_key_vault,                // 新增
        // ... 其余不变 ...
    }
}
```

---

## 10. Manifest 变更（可选）

`manifest.toml` 中可选项新增 `web_search.providers` 声明：

```toml
# manifest.toml
[tools]
builtin = ["web_search", "web_fetch", ...]

[web_search]
# 建议的搜索 Provider（Agent 开发者推荐，用户可覆盖）
suggested_provider = "tavily"
# 备用 Provider 列表
fallback_providers = ["brave"]
```

与 LLM Provider 机制对齐：
- `suggested_provider` — 开发者推荐，Gateway 配置覆盖
- `fallback_providers` — 运行时 fallback 链

---

## 11. 实现阶段

### Phase 1: Provider 核心 + Vault 集成 + Resource Cache（后端）

| 任务 | 产出 | 预估 |
|------|------|------|
| P1.1 | `SearchProviderConfig` / `SearchKeyEntry` / `SearchProviderListItem` 数据结构定义 | `rollball-core/src/protocol.rs` |
| P1.2 | `SearchBackend` trait + Tavily / Brave / Firecrawl / SearXNG 实现 | `rollball-runtime/src/tools/builtin/web_search/` |
| P1.3 | VaultFacade 新增 `store_search_key` / `get_search_provider` / `list_search_providers` / `remove_search_key` | `rollball-gateway/src/vault/mod.rs` |
| P1.4 | ResourceCache 新增 `SearchListFile` + `rebuild_and_save_search_cache()` + load/save 函数 | `rollball-gateway/src/resource_cache.rs` |
| P1.5 | Gateway 启动时 `rebuild_and_save_search_cache()` 初始化 search_list.json | `rollball-gateway/src/gateway/mod.rs` |
| P1.6 | Gateway HTTP API 新增 `POST/DELETE /api/search/keys` 端点 (add → 触发 rebuild_cache) | `rollball-gateway/src/http/search_api.rs` |
| P1.7 | Tauri Command 新增 `list_search_keys` / `add_search_key` / `remove_search_key` / `update_search_key` | `apps/rollball-desktop/src-tauri/src/commands/` |
| P1.8 | GatewayRequest::AgentHello 新增 `search_list_version` 字段 | `rollball-core/src/protocol.rs` |
| P1.9 | GatewayResponse::AgentHelloResult 新增 `search_list` / `search_list_version` / `search_key_vault` | `rollball-core/src/protocol.rs` |
| P1.10 | `handle_agent_hello` 新增 search_list 差量同步逻辑 | `rollball-gateway/src/ipc/server.rs` |
| P1.11 | GatewayResponse 新增 `SearchConfigDelivery` 变体 | `rollball-core/src/protocol.rs` |
| P1.12 | gRPC proto 更新: AgentHello + AgentHelloResult + SearchConfigDelivery + ServerMessage | `rollball-core/proto/gateway_ipc.proto` |
| P1.13 | Proto bridge 更新: AgentHello / AgentHelloResult / SearchConfigDelivery 的序列化 | `rollball-core/src/proto_bridge.rs` |
| P1.14 | Gateway gRPC dispatch 新增 `SearchConfigDelivery` 推送 | `rollball-gateway/src/grpc/dispatch.rs` |
| P1.15 | Runtime `send_agent_hello` 新增 `cached_search_version` 参数 | `rollball-runtime/src/grpc/client.rs` |
| P1.16 | `WebSearchTool` 重构，移除 DuckDuckGo，接入 SearchBackend fallback 链 | `rollball-runtime/src/tools/builtin/web_search.rs` |
| P1.17 | Agent workspace `agent_search.json` 读写（per-agent search provider 优先级列表） | `rollball-runtime/src/agent_config.rs` (扩展) |
| P1.18 | Runtime 侧 `resource_cache.json` 读写 (search_list_version 版本号持久化) | `rollball-runtime/src/agent_config.rs` (扩展) |
| P1.19 | SessionManager 新增 `update_search_config()` + `CachedSearchConfig` | `rollball-runtime/src/agent/session/` |
| P1.20 | Gateway 热推送: vault_api 修改 Search Key 后调用 `rebuild_search_cache()` + `push_search_config()` | `rollball-gateway/src/http/search_api.rs` |

#### 附带：命名统一清理（独立 PR，可与 Search Provider 并行）

| 任务 | 内容 | 产出 |
|------|------|------|
| Cleanup.1 | 将 `mcp_servers.json` 重命名为 `agent_mcp.json`（文件已存在） | `rollball-runtime/src/agent_config.rs` |
| Cleanup.2 | 更新 Gateway HTTP API `/api/agents/{id}/mcp-servers` 读写路径 | `rollball-gateway/src/http/agents.rs` |

### Phase 2: 前端 UI

#### P2.A — HarnessPage Search Tab（Gateway 全局管理）

| 任务 | 产出 | 预估 |
|------|------|------|
| P2.1 | HarnessPage 新增 Search Tab | `HarnessPage.tsx` |
| P2.2 | SearchTab 组件实现（已配置列表 + 可选列表） | `components/harness/SearchTab.tsx` (新文件) |
| P2.3 | `search-providers.ts` 静态供应商列表 | `lib/search-providers.ts` (新文件) |
| P2.4 | 添加/编辑对话框 + Test Key | SearchTab 内嵌或独立组件 |
| P2.5 | 类型定义 `SearchKeyEntry` / `SearchProviderDef` | `lib/types.ts` |
| P2.6 | Tauri Commands: `add_search_key` / `remove_search_key` / `list_search_keys` | `src-tauri/` |

#### P2.B — Agent Setup 面板 Search Block（per-Agent 配置）

| 任务 | 产出 | 预估 |
|------|------|------|
| P2.7 | AgentSetupTab 新增 Search 配置块 UI | `AgentSetupTab.tsx` |
| P2.8 | 拖拽排序实现（调整 provider 优先级） | 复用或引入轻量 dnd 库 |
| P2.9 | Toggle 开关 + 自动保存到 `agent_search.json` | AgentSetupTab 逻辑扩展 |
| P2.10 | 新增 Gateway HTTP API: `/api/agents/{id}/search-providers` + `search-config` GET/PUT | `rollball-gateway/src/http/agents.rs` |
| P2.11 | 灰色降级展示（Vault 中无 Key 的 provider） | AgentSetupTab 联动 search_list |

### Phase 3: 集成测试

| 任务 | 产出 |
|------|------|
| P3.1 | Tavily API 集成测试 |
| P3.2 | Fallback 链测试（主 Provider 失败 → 备用） |
| P3.3 | 无 Provider 配置时的错误返回测试 |
| P3.4 | Vault 读写 + 分发链路端到端测试 |
| P3.5 | 前端 E2E：添加 → 保存 → Agent 启动 → web_search 工作 |

---

## 12. 向后兼容

- 已有 `web_search` 工具 spec 不变（`name: "web_search"`, 参数 `query` + `count`）
- 迁移路径：升级后首次启动无 Provider 配置时，`web_search` 返回友好错误提示而非 panic
- 不影响已有的 MCP web_search 工具注册

---

## 参考

- [12-tool-system.md](file:///d:/projects/rust/agent-study/docs/design/12-tool-system.md) — 工具系统设计
- [04-gateway.md](file:///d:/projects/rust/agent-study/docs/design/04-gateway.md) — Gateway 设计
- [08-security.md](file:///d:/projects/rust/agent-study/docs/design/08-security.md) — 安全设计（Vault）
- [16-ipc-grpc-migration.md](file:///d:/projects/rust/agent-study/docs/design/16-ipc-grpc-migration.md) — gRPC migration
- [现有 web_search.rs](file:///d:/projects/rust/agent-study/core/rollball-runtime/src/tools/builtin/web_search.rs)
- [现有 VaultFacade](file:///d:/projects/rust/agent-study/core/rollball-gateway/src/vault/mod.rs)
- [现有 HarnessPage ProvidersTab](file:///d:/projects/rust/agent-study/apps/rollball-desktop/src/components/harness/HarnessPage.tsx)
