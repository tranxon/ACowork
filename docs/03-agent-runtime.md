# Agent Runtime（统一执行引擎）

> 版本：v3.0 | 更新日期：2026-04-09

---

Agent Runtime 是平台提供的唯一二进制可执行文件，类似 Android 的 ART 虚拟机。Gateway 为每个 Agent 启动一个 Agent Runtime 进程，将 .agent 包路径作为启动参数传入。

## 1. 启动方式

```bash
agent-runtime \
    /path/to/agent-package \
    --socket /tmp/agent-gateway.sock \
    --agent-id com.example.weather \
    --workspace /home/user/.local/share/agent-gateway/agents/com.example.weather/workspace \
    --config-dir /home/user/.local/share/agent-gateway/agents/com.example.weather/config \
    --identity '{"name":"张三","city":"Shanghai","language":"zh-CN","timezone":"Asia/Shanghai"}'
```

## 2. 内部结构

```
Agent Runtime 二进制
├── Package Loader      # 解析 .agent ZIP，加载 manifest + prompts + config
├── Prompt Builder      # 组装 system prompt（identity + tools + skills + memory context）
├── History Manager     # 对话历史管理（token 预算、trim、压缩）
├── LLM Client          # 直连 LLM Provider API（OpenAI/Claude/Ollama 等）
├── Tool Dispatcher     # 解析 LLM 输出的 tool_calls，路由到工具实现
│   ├── Built-in Tools  # 内置工具（memory_recall, memory_store, http_get, shell...）
│   ├── WASM Tools      # .agent 包中声明的 WASM 工具（沙箱内执行）
│   └── Gateway Tools   # 需要 Gateway 协调的工具（Intent 收发）
├── Permission Checker  # 根据 manifest 权限表校验工具调用权限
├── Memory Client       # 读写私有 Grafeo
├── Grafeo (嵌入式)     # 私有 Memory（情景记忆 + 语义记忆）
├── Skill Loader        # 加载 .agent 包中的 Skills
├── Budget Manager      # 本地预算预检 + 用量上报
└── Loop Controller     # 主循环控制（迭代次数、超时、循环检测）
```

## 3. 主循环

Agent Runtime 的核心是 LLM 交互循环（参考 ZeroClaw 的 `run_tool_call_loop`）：

```
用户消息 / Intent / 定时触发
       │
       ▼
┌─────────────────────────────────────────┐
│  Agent Runtime 主循环 [iteration: 0..N]  │
│                                         │
│  ① 预算预检                             │
│     └─ 本地预算缓存不足 → fallback 或报错 │
│                                         │
│  ② 构建上下文                            │
│     ├─ System Prompt (from prompts/)    │
│     ├─ Memory RAG (from 私有 Grafeo)    │
│     ├─ Identity Context (from 启动注入) │
│     ├─ Skills (from skills/)            │
│     └─ 对话历史                          │
│                                         │
│  ③ 调用 LLM (直连 API)                  │
│     ├─ RateAcquire速率协调              │
│     └─ streaming 或 blocking            │
│                                         │
│  ④ 解析响应                              │
│     ├─ text → 返回结果/回复用户          │
│     └─ tool_calls → ⑤                  │
│                                         │
│  ⑤ 工具调度与执行                        │
│     ├─ Permission Check (manifest)      │
│     ├─ Built-in Tool → 直接执行         │
│     ├─ WASM Tool → Wasmtime 沙箱执行    │
│     └─ Gateway Tool → Unix Socket 调用  │
│                                         │
│  ⑥ 结果追加到历史                        │
│                                         │
│  ⑦ 用量上报（异步，不阻塞）              │
│                                         │
│  ⑧ 循环检测（防止重复工具调用）          │
│                                         │
│  └─→ 回到 ①（下一轮迭代）               │
└─────────────────────────────────────────┘
```
