# ADR-055 Phase 3c 实施 Overview

**日期**：2026-08-26
**范围**：ADR-055 §7 Phase 3 的 3.4 — Runtime `GET /workspaces/raw/{path}` + Gateway 静态预览纯反代 + `fs_browse` `target`（L2-6/7、L7-1）
**状态**：✅ 完成并验证

## 本次完成

### Runtime raw 端点（L2-7）
- `WorkspaceQueryService` trait 加 `read_file_raw`，返回 `RawFileDto { bytes, mime_type, size }`（verbatim 字节，非 JSON envelope）；复用 `resolve_within` 路径穿越防护。
- 新端点 `GET /workspaces/raw/{*path}?workspace_id=…`，返回原始字节 + `Content-Type`。

### Gateway 静态预览改纯反代（L2-6/7）
- `workspaces.rs` 重写：`/workspace-files/{id}/{ws}/{*path}` 与 `/ws-files/{id}/{*path}` 改为反代到 Runtime raw 端点；删除本机读文件逻辑（`resolve_workspace_root` / `serve_workspace_file_from_root` 等）。
- `proxy_to_runtime_with_method` 提为 `pub(crate)`；`urlencoding` 逐段编码文件名（空格/非 ASCII）。

### fs_browse target（L7-1）
- proto `NodeInfo` 加 `http_endpoint = 11`（Node 反代 base URL）。
- Node `fs_browse.rs` 从 stub 实现本机目录列举 + `/fs/browse` 路由，与 `/agents/{id}/*` 合并同 `:19900` 监听。
- Gateway `/api/fs/browse?target={node_id}`：非 `local` 反代到该 node 的 `/fs/browse`；`AppState` 注入 `node_registry`。

## 关键决策
- **Gateway 对 workspace 文件系统零直接访问**：静态预览走「Gateway → Node → Runtime」反代，`workspace_id` 解析 + 穿越防护都在 Runtime 侧（ADR-034 规则 3 零例外达成）。
- **`http_endpoint` 复用 NodeInfo retained 下发**：零新协议消息，与 agent 反代的 `agent_id → http_endpoint` 对称。
- **fs_browse 反代「解析后重序列化」**：错误映射为明确 `ApiError`（node offline / 无 endpoint / 请求失败）。

## 验证
- `cargo check`（4 crate）✅ + `clippy -D warnings` 0 warning ✅
- golden 6 passed（`http_endpoint` 字节更新，NodeInfo 长度 103→127）✅
- node 67 passed + gateway 271 passed（各 1 既有 macOS `ps -p` flaky）+ runtime raw 单测 1 passed ✅

## 剩余（Phase 3d+）
- 3d node shutdown 关停 runtime + §6.19 re-adopt 孤儿收养
- 3e `--node` CLI 接线 + `logs -f` + 生产严格签名（dev_mode）+ 版本协商
- 3f rename/leave/service install
- 3g Desktop 节点管理 UI
- 5a Node 反代 token 鉴权

详见 `report-adr055-phase3c-workspaces-raw-fs-browse.md`。
