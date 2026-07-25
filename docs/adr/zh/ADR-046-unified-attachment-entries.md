# ADR-046：附件统一条目化（File Upload / Image Upload / Add to Chat）

**状态**：草案
**日期**：2026-07-25
**决策者**：大鱼

**前置**：
- ADR-021（Session 数据加载统一路径）
- ADR-024（per-session meta 文件 + JSONL 去 header）
- ADR-034（MQTT / HTTP 边界 — HTTP 仍是文档上传通道）

**触发动因**：
用户上传一张图片让 Agent 分析，分析过程正确，但 conversation JSONL 里完全没有这条上传的痕迹；下次重开 session 找不到文件，聊天记录也没有文件图标。三类附件（文件上传、图片上传、Add to Chat）各自走不同的、没有持久化的代码路径，user 消息被污染。

---

## 1. 问题描述

### 1.1 现状（事实）

三类附件今天的代码路径：

| 附件类型 | 入口 | 持久化结果 |
|---|---|---|
| 文件上传（PDF/DOCX/PPTX/XLSX） | `upload_document` HTTP → `<work_dir>/sessions/{sid}/documents/<doc_id>` + `documents.json` 旁路索引 | JSONL 写一条 `metadata.type="document_upload"` 的 system 条目，但 `filename/format/size_bytes/path` 全为 null（gateway_loop 只转发 `document_ids` 字符串数组，元数据被丢弃） |
| 图片上传（PNG/JPG） | 前端 base64 → `content_parts.image_url` → 后端 `ChatMessage::user_multimodal`（**只在内存**） | JSONL **完全不记录**；base64 走完 MQTT 即丢 |
| Add to Chat（文件/选区/目录） | 前端 `addAttachedContext` → 发送时拼 `params.attached_context` → 后端 `build_attached_context_blocks` 拼进 `enriched_content` | JSONL **完全不记录**；只活在当次 LLM 请求的 prompt 里 |

### 1.2 用户可见后果

1. 上传 PDF 后重启 session：JSONL 里有 `document_upload` 条目但所有元数据为 null
2. 上传图片后重启 session：图片彻底消失
3. 选区/文件 Add to Chat 后重启 session：上下文完全丢失
4. 聊天记录从不显示附件图标（无独立条目类型 → 无渲染分支）
5. 重启后 user 消息被 `mergeDocumentUploads` 折回去复杂化，与原始输入不一致

---

## 2. 决策

### 2.1 统一条目化

**每条附件都是 JSONL 里独立的 system 条目**，`user` 消息只保留用户原话。附件类型用 `metadata.type` 区分：

```jsonl
{"id":"...","ts":"...","role":"system","content":"Uploaded file: report.pdf","metadata":{"type":"file_upload","document_id":"0123456789ab-3","filename":"report.pdf","format":"pdf","size_bytes":12345}}
{"id":"...","ts":"...","role":"system","content":"Uploaded image: screen.png","metadata":{"type":"image_upload","document_id":"0123456789ac-7","filename":"screen.png","format":"png","size_bytes":987654,"width":1920,"height":1080}}
{"id":"...","ts":"...","role":"system","content":"Attached: src/main.rs","metadata":{"type":"attached_file","abs_path":"/abs/path/src/main.rs","name":"main.rs"}}
{"id":"...","ts":"...","role":"system","content":"Attached: src/main.rs (L10-L25)","metadata":{"type":"attached_selection","abs_path":"/abs/path/src/main.rs","name":"main.rs","start_line":10,"end_line":25}}
{"id":"...","ts":"...","role":"system","content":"Attached folder: src/","metadata":{"type":"attached_folder","abs_path":"/abs/path/src","name":"src"}}
```

`user` 消息永远是用户原话（含图片时除外，base64 在当次请求通过 `content_parts` 注入到 LLM，但不入 JSONL；图片文件本身已落盘）。

### 2.2 文件存储统一

**所有上传文件（PDF + 图片）落到 `<work_dir>/files/<doc_id>`**，与 `conversations/` 同级：

```
<work_dir>/
├── conversations/
│   ├── <sid>.jsonl
│   └── meta/<sid>.json
└── files/
    └── <doc_id>          # 无扩展名 — format 在 JSONL 元数据里
```

**删除**：`sessions/<sid>/documents/` 目录、`sessions/<sid>/documents.json` 旁路索引。`load_documents_index` / `save_documents_index` / `documents_dir` / `compute_doc_id` 全部移除。文件元数据不再有 sidecar，**JSONL 是唯一真相**。

**Add to Chat（attached_file/selection/folder）不落盘**，仅在 JSONL 里记地址。理由：这些是工作区文件，本来就在磁盘上，多复制一份没意义；唯一持久化开销是路径字符串。

### 2.3 写入时机

| 类型 | 触发点 | 写入 |
|---|---|---|
| `file_upload` | `upload_document` HTTP 返回后，前端把 `{document_id, filename, format, size_bytes}` 放入 `params.document_ids` | 后端 `session_task.rs` 收到 `documents` 参数后调用 `conversation.append_message_with_id("system", "Uploaded file: ...", metadata, doc_id_as_msg_id)` |
| `image_upload` | **新增** `upload_file` HTTP 端点（接受任意文件，`content_type` 默认从 ext 推断）。前端可选回传 `width/height`（desktop 必传，CLI 可能不传）；后端 schema 容错存储，缺失即 JSONL 不写 | 同上 |
| `attached_*` | 前端发送时按当前顺序逐个补 system 条目到 JSONL（在 user 消息之前） | 复用 `append_message_with_id("system", "Attached: ...", metadata, msg_id)` |

### 2.4 LLM 输入改造

**删除** `session_task.rs:807-1100` 里的 enriched_content 拼装逻辑：
- 删除 `<attached_document filename="...">...</attached_document>` 块的 doc_reader 预提取
- 删除 `[Attached context:] - file: \`path\`` 块拼接
- 删除 `The following workspace files were attached by the user...` 块

**保留**：
- 图片 → 在 `agent_loop.run()` 时把 `content_parts` 注入当前 user 消息（已经是 multimodal 流程，不变）
- 文件/选区 → 在 `context_builder` 里以**结构化引用**形式给出（每个 attached_* 转一条 `- file: \`path\` (L10-L25)` 文本，让 LLM 知道用 `read_file` 自取内容）

### 2.5 渲染层

`MessageBubble.tsx` 新增分支（替换当前 `document_upload` 单一分支）：

```tsx
if (message.type === "system" && message.metadata?.type === "file_upload")
  return <AttachmentChipRow kind="file"     filename={...} format={...} size={...} />
if (message.type === "system" && message.metadata?.type === "image_upload")
  return <AttachmentChipRow kind="image"    filename={...} format={...} size={...} documentId={...} width={metadata.width} height={metadata.height} />  // 缩略图由 GET /files/<doc_id> 拉取
if (message.type === "system" && message.metadata?.type === "attached_file")
  return <AttachmentChipRow kind="file"     name={...} absPath={...} onClick={openInEditor} />
if (message.type === "system" && message.metadata?.type === "attached_selection")
  return <AttachmentChipRow kind="selection" name={...} startLine={...} endLine={...} onClick={openInEditor} />
if (message.type === "system" && message.metadata?.type === "attached_folder")
  return <AttachmentChipRow kind="folder"   name={...} absPath={...} onClick={revealInTree} />
```

`AttachmentChipRow` 统一组件：
- 图标：`FileText`（file）、`Image`（image，缩略图从 `GET /files/<doc_id>` 拿 blob → `URL.createObjectURL` 渲染）、`Hash`（selection）、`Folder`（folder）
- 单击行为：file/selection → 调 `useFileEditorStore.openFile(agentId, workspaceId, relPath)`；folder → 调 `requestLocate` + `requestShowWorkspacePanel`
- **image_upload 宽高**：JSONL 中若存在（desktop 当前必传）则作为 CSS 宽高提示；若缺失（未来轻量 CLI 场景）则 `<img>` 由 `onLoad` 自然读取真实尺寸作为 fallback；**渲染层两种路径都容错**

### 2.6 删除项

| 删除位置 | 删除内容 |
|---|---|
| `core/acowork-runtime/src/http/server.rs` | `UploadDocumentBody`, `upload_document`, `read_document`, `delete_document`, `DocumentEntry`, `DocumentsIndex`, `documents_dir`, `documents_index_path`, `load_documents_index`, `save_documents_index`, `compute_doc_id` |
| `core/acowork-gateway/src/http/proxy.rs` | `proxy_upload_document`, `proxy_list_documents`, `proxy_read_document`, `proxy_delete_document` + 路由 |
| `core/acowork-runtime/src/agent/loop_memory.rs` | `write_document_entries` 整个函数 |
| `core/acowork-runtime/src/agent/session/session_task.rs` | `build_attached_context_blocks` 整个函数；`session_task.rs:807-1100` 的 enriched_content 拼装；doc_reader 预提取逻辑；image 上传时手工拼 file_summary |
| `core/acowork-runtime/src/startup/gateway_loop.rs` | 解析 `documents: Vec<serde_json::Value>` → 改为解析 `document_ids: Vec<{document_id, filename, format, size_bytes, [width, height]}>` 富对象数组（`width`/`height` 为 `Option<u32>`，缺失即 None；与 type 字段对应，分别写 file_upload / image_upload） |
| `apps/acowork-desktop/src/stores/chatStore.ts` | `mergeDocumentUploads` 函数、`convertConversationEntry` 里 document_upload 分支、optimisticDocs 合并、`documents` 字段被剥到 user 消息的逻辑 |
| `apps/acowork-desktop/src/components/chat/ChatPanel.tsx` | `handleFileUpload` 的 `upload_document` Tauri command → 改为通用 `upload_file`（自动判 format 走 file 或 image）；`pendingFiles` 状态保留为上传中临时态，但发送后立刻从 JSONL 持久化的 system 条目回填；`optimisticDocs` 取消 |
| `apps/acowork-desktop/src/components/chat/MessageBubble.tsx` | 现有的 document_upload 单一分支 → 替换为 5 个 metadata.type 分支 |

### 2.7 HTTP 端点（保留+扩展）

| 端点 | 用途 |
|---|---|
| `POST /sessions/{sid}/files` | 上传任意文件（PDF/DOCX/PPTX/XLSX/PNG/JPG/...）。`multipart` form `{file, content_type?, width?, height?}`（`width`/`height` 为可选，desktop 当前必传，CLI 可能省略）。返回 `{document_id, filename, format, size_bytes, [width, height]}`（服务端把收到的 width/height 原样回传，未传则不出现）。**保存到 `<work_dir>/files/<document_id>`**。后端不做图片识别、不读 header、不依赖 `image` crate —— 完全信任前端传入的元数据 |
| `GET /files/{document_id}` | 下载文件（auth 通过 Gateway 代理；Tauri 端用此接口加载缩略图） |

---

## 3. 目录结构最终态

```
<work_dir>/
├── conversations/
│   ├── <sid>.jsonl       # 纯对话 + 附件条目（system role + metadata.type）
│   ├── <sid>.jsonl.lock  # 现有文件锁
│   └── meta/
│       └── <sid>.json    # ADR-024 per-session meta
└── files/
    └── <doc_id>          # 所有上传文件，无扩展名
```

---

## 4. 兼容性

**无**。项目还在开发中，不保留任何兼容代码，不写迁移脚本：
- 旧 `document_upload` 条目不识别（丢弃）
- 旧 `sessions/<sid>/documents/` 目录用户手动处理（若需要保留数据，由用户自行 cp/mv 到新位置）
- 旧 `documents.json` 旁路索引直接删除
- 旧 `mergeDocumentUploads` / `optimisticDocs` / `documents` inline 到 user 消息的逻辑全删

---

## 5. 风险与回滚

| 风险 | 缓解 |
|---|---|
| 大量并发上传的文件落盘命名冲突 | `<doc_id>` 是内容 hash 12-hex + 4-hex suffix（沿用现有算法），冲突概率 ~10⁻⁹ |
| `files/` 目录随时间无限增长 | **本轮不管**，由未来的文档管理功能统一处理 |
| `MessageBubble` 多分支回归 | 5 个 metadata.type 各一个 React 测试快照 |

---

## 6. 实施拆分

1. **后端 schema**（conversation.rs / loop_memory.rs / session_task.rs）
   - 新 metadata 字段类型（FileUploadMeta / ImageUploadMeta / AttachedFileMeta / AttachedSelectionMeta / AttachedFolderMeta）
   - 替换 `write_document_entries` → 接受富对象数组
   - 删除 enriched_content 拼装
2. **后端 storage**（http/server.rs + proxy.rs）
   - 新 `POST /sessions/{sid}/files`、`GET /files/{doc_id}`（**不实现 DELETE**，留给未来文档管理功能）
   - 删除 upload_document / read_document / delete_document / documents.json 相关
   - 落盘路径改为 `<work_dir>/files/<doc_id>`
3. **前端发送**（chatStore.ts / ChatPanel.tsx）
   - sendMessage 新增 `attachedItems` 参数（来自 sessionState.attachedContext），逐条先写 system JSONL（通过 HTTP 端点拿到 msg_id 再交给 backend）— 或者更简单：**前端只把 `document_ids + attached_items` 通过 params 推给后端**，由后端写 JSONL
   - pendingFiles / pendingImages 仍保留（上传中本地态），但发送后立刻清空
   - 统一 `upload_file` Tauri command（自动判 PDF/图片）
4. **前端渲染**（MessageBubble.tsx / 新 AttachmentChipRow.tsx）
   - 5 个 metadata.type 分支
   - image_upload 缩略图通过 `GET /files/<doc_id>` 拿 blob → `URL.createObjectURL`
   - desktop 前端：发送时**必传** width/height（用 `new Image()` 读真实尺寸）；JSONL 中带 width/height 时优先用作缩略图 CSS hint
   - 未来 CLI 客户端：可不传 width/height；JSONL 缺字段；渲染时 `<img onLoad>` 拿真实尺寸作为 fallback，**两种路径都容错**

---

## 7. 决策点摘要

- ✅ 复用 `metadata.type` 判别式（与 `compaction` 同机制）
- ✅ `<work_dir>/files/` 与 conversations 同级
- ✅ 文件落盘无扩展名（format 在 JSONL 元数据里）
- ✅ attached_folder 不落盘，只在 JSONL 记地址
- ✅ 不保留任何兼容代码，不写迁移脚本（旧文件由用户手动处理）
- ✅ 用户消息永远只含原话
- ✅ 图片宽高是**可选字段**（`Option<u32>`）：desktop 当前必传，CLI 未来可能省略；JSONL 用 `skip_serializing_if = "Option::is_none"` 容错存储，渲染层缺字段时由 `<img onLoad>` 自然读取 fallback
- ✅ `files/` 目录清理属于未来文档管理功能，本轮不实现
- ✅ 不实现 `DELETE /files/<doc_id>` 端点，留给未来文档管理功能