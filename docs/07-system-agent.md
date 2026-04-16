# 系统 Agent（com.rollball.system）

> 版本：v3.4 | 更新日期：2026-04-16

---

系统 Agent 是 Rollball 的"系统应用"，类似 Android 的系统内置 App（SystemUI / Settings / Contacts）。它是一个特殊的 .agent 包，随 Gateway 一起分发，拥有特权，Gateway 启动时自动拉起。

**核心定位**：系统 Agent 是用户与 Rollball 平台之间的默认交互入口和系统级数据服务的提供者。当没有安装任何第三方 Agent 时，系统 Agent 就是用户和 Rollball 交互的唯一界面。所有需要"智能"的系统级服务——身份识别、偏好管理、帮助导航——都由系统 Agent 的 LLM 处理，Gateway 本身不承担任何业务逻辑推理。

## 1. 系统 Agent 的特殊性

```json
{
  "agent_id": "com.rollball.system",
  "system": true,
  "privileges": [
    "content_provider",       // 可注册 ContentProvider 服务
    "auto_start",             // Gateway 启动时自动拉起
    "uninstallable": false,   // 不可卸载
    "priority": "system"      // 最高启动优先级
  ]
}
```

**与普通 Agent 的区别：**

| 属性 | 普通 Agent | 系统 Agent |
|------|-----------|-----------|
| 安装方式 | 用户从仓库安装 | 随 Gateway 分发，不可卸载 |
| 启动时机 | 按需 / 定时 / 手动 | Gateway 启动时自动拉起 |
| 生命周期 | 空闲可被杀死 | 常驻（空闲超时后也可杀死，但下次 Gateway 检测到需求时立即拉起） |
| ContentProvider | 不可以 | 可以注册，其他 Agent 可通过 Intent 查询 |
| 身份提报 | 可以向系统 Agent 提报身份信息 | 接收提报，LLM 判断后存入私有 Grafeo |

## 2. ContentProvider 机制

系统 Agent 通过 Intent + Capability 机制对外提供数据服务，标记 `"provider": true` 的 Capability 表示这是 ContentProvider 语义——只读数据服务，不是一次性动作。

**系统 Agent 声明的 Capabilities：**

```json
{
  "capabilities": {
    "identity:query": {
      "input": { "fields": ["string"] },
      "output": { "values": "map<string, string>", "confidence": "map<string, float>" },
      "provider": true
    },
    "identity:observe": {
      "input": { "fields": ["string"], "callback_intent": "string" },
      "output": { "subscribed": "bool" },
      "provider": true
    }
  }
}
```

**其他 Agent 查询用户信息：**

```json
{
  "type": "intent",
  "target": "com.rollball.system",
  "action": "identity:query",
  "params": { "fields": ["name", "city", "language"] }
}
```

系统 Agent 从私有 Grafeo 查询并返回：

```json
{
  "values": {
    "name": "张三",
    "city": "Shanghai",
    "language": "zh-CN"
  },
  "confidence": {
    "name": 1.0,
    "city": 0.85,
    "language": 1.0
  }
}
```

## 3. 冷启动身份注入

新安装的 Agent 首次运行时，如果 manifest 中声明了 `identity_deps`，Gateway 通过握手协议的 `identity_delivery` 消息将身份信息注入 Agent Runtime：

```
Agent manifest 声明 identity_deps
        │
        ▼
Gateway 启动前，向系统 Agent 发送 identity:query Intent
        │
        ▼
系统 Agent 从私有 Grafeo 查询，返回字段值和 confidence
        │
        ▼
Gateway 启动 Agent Runtime，建立 Socket 连接
        │
        ▼
握手 step ④：identity_delivery 消息推送身份数据
        │（消息类型已在 06-communication.md §1.2 定义）
        ▼
Agent Runtime 将身份数据写入工作记忆，进入主循环
```

manifest 声明示例：

```json
{
  "identity_deps": ["display_name", "city", "language", "timezone"]
}
```

`identity_delivery` 消息格式（见 06-communication.md §1.2）：

```json
{
  "type": "identity_delivery",
  "identity": {
    "display_name": "张三",
    "city": "Shanghai",
    "language": "zh-CN",
    "timezone": "Asia/Shanghai"
  },
  "confidence": {
    "display_name": 1.0,
    "city": 1.0,
    "language": 1.0,
    "timezone": 1.0
  }
}
```

> 注意：用户身份信息**不通过命令行参数传入**（避免 `/proc/<pid>/cmdline` 泄露）。Runtime 启动后通过握手消息获取。

## 4. 身份信息的来源

系统 Agent 获取用户身份信息有三条路径，分层补充而非互斥：

### 渠道一：Onboarding 注册（主要，高确定性）

首次启动 Desktop App 或 CLI 时强制采集，采集完成后写入系统 Agent：

```
Desktop App Onboarding → Gateway HTTP API → 系统 Agent
                                              │
                                              ▼
                                    identity_store(
                                      field = "display_name",
                                      value = "张三",
                                      confidence = 1.0,
                                      source = "onboarding"
                                    )
```

**采集字段分级：**

| 级别 | 字段 | 说明 |
|------|------|------|
| 必填 | `display_name` | 称谓（用户希望怎么被称呼） |
| 必填 | `language` | 语言偏好（如 zh-CN、en-US），影响 LLM prompt 语言 |
| 必填 | `timezone` | 时区（如 Asia/Shanghai），影响时间显示 |
| 选填 | `city` | 所在城市 |
| 选填 | `occupation` | 职业/领域 |
| 选填 | `communication_style` | 沟通偏好（简洁/详细/正式） |
| 选填 | `custom` | 开放扩展字段（如编辑器偏好等） |

Onboarding 采集的数据 confidence = 1.0（用户主动声明，确定性最高）。

Desktop App 的 Onboarding 流程见 14-desktop-app.md §4.1。

### 渠道二：Agent 主动询问（补充，中确定性）

Agent 在运行时识别到缺失关键身份字段（如"你在哪个城市？"），向系统 Agent 发送 `identity:update` Intent：

```json
{
  "type": "intent",
  "target": "com.rollball.system",
  "action": "identity:update",
  "params": {
    "field": "city",
    "value": "Shanghai",
    "evidence": "用户说'我住在北京'",
    "confidence": 0.85,
    "source": "agent_question"
  }
}
```

### 渠道三：自然对话沉淀（辅助，低确定性）

Agent 在日常对话中自动提取用户透露的身份信息，通过 `identity:update` Intent 汇报：

```json
{
  "type": "intent",
  "target": "com.rollball.system",
  "action": "identity:update",
  "params": {
    "field": "occupation",
    "value": "软件工程师",
    "evidence": "用户提到'我是做后端开发的'",
    "confidence": 0.7,
    "source": "conversation"
  }
}
```

### 统一写入：identity_store 工具

三条路径最终都调用 `identity_store` 工具（系统 Agent 专用，第 14 个内置工具，见 12-tool-system.md §2.3），由系统 Agent 的 LLM 做二次质量判断：

```
来源提报 → 系统 Agent LLM 判断 → identity_store 写入
  ├─ 语义有效 → 写入 Grafeo
  └─ 语义模糊 → 拿不准就不更新
```

**LLM 二次判断示例：**

```
提报: city = "Shanghai", evidence = "我刚搬到上海", confidence = 0.9
          │
          ▼
系统 Agent LLM: "搬家" → 确实是居住地变更 → 更新 user.city

提报: city = "Shanghai", evidence = "我下周去上海出差", confidence = 0.7
          │
          ▼
系统 Agent LLM: "出差" → 临时行程，非居住地变更 → 不更新 user.city
```

## 5. 变更通知（observe 机制）

类似 Android ContentProvider 的 `registerContentObserver`，Agent 可以订阅特定身份字段的变更：

```json
{
  "type": "intent",
  "target": "com.rollball.system",
  "action": "identity:observe",
  "params": {
    "fields": ["city"],
    "callback_intent": "com.example.weather"
  }
}
```

当系统 Agent 更新了 city 字段，通过 Gateway 向订阅者广播：

```json
{
  "type": "notification",
  "from": "com.rollball.system",
  "action": "identity:changed",
  "params": {
    "field": "city",
    "old_value": "Beijing",
    "new_value": "Shanghai"
  }
}
```

## 6. 系统 Agent 的能力边界

| 能力 | 说明 | 类比 Android |
|------|------|-------------|
| 身份管理 | 用户姓名、语言、时区、城市等 | Contacts / Settings |
| 偏好管理 | 回复风格、默认模型等 | Settings |
| 帮助与导航 | "我该怎么用？"、"你能做什么？" | Settings 的帮助页 |
| Agent 推荐 | 根据用户需求推荐安装新 Agent | Play Store 的推荐 |
| 默认交互 | 无第三方 Agent 时的用户入口 | Launcher |

系统 Agent 只做"系统级"的事，每个具体领域的能力留给专门的 Agent。

## 7. 对架构的简化效果

去掉公共 Grafeo、引入系统 Agent 后，Gateway 彻底回归"纯基础设施"定位：

| 之前（公共 Grafeo） | 现在（系统 Agent） |
|---|---|
| Gateway 维护 Grafeo 实例 | Gateway 不维护任何数据库 |
| Gateway 提供 SharedMemory API | Gateway 只做 Intent 路由 |
| Gateway 管理只读视图、写入权限 | 权限交给系统 Agent 自治 |
| Agent 提报 → Gateway 仲裁 → 用户确认 | Agent 提报 → 系统 Agent LLM 判断 |
| 需要确认策略配置 | LLM 推理替代策略配置 |
| Gateway 承担业务逻辑 | Gateway 纯基础设施，零业务逻辑 |

连系统级服务本身也是一个 Agent——这才是 Agent as APP 模型最自洽的设计。
