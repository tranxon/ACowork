/** Gateway deployment mode */
export type GatewayMode = "local" | "remote";

/** Local Gateway process state */
export type LocalGatewayState = "idle" | "starting" | "running" | "stopped" | "error";

/** Gateway health check response */
export interface HealthResponse {
  status: string;
  version: string;
  checks?: Record<string, { status: string; detail?: string }>;
}

/** Agent list entry — matches Gateway HTTP API GET /api/agents */
export interface AgentListResponse {
  agent_id: string;
  name: string;
  display_name: string | null;
  role: string | null;
  avatar: string | null;
  builtin_avatar?: string;
  version: string;
  running: boolean;
  connected: boolean;
  dev_mode: boolean;
  /** Whether DevMode is live right now (ADR-048 follow-up; can be enabled at runtime). */
  debug_state?: "enabled" | "disabled";
  debug_port: number | null;
}

/** Agent model info — matches Gateway HTTP API GET /api/agents/{id}/model */
export interface AgentModelResponse {
  provider: string;
  model: string;
  available_models: string[];
}

/** System status response */
export interface SystemStatusResponse {
  version: string;
  agents_installed: number;
  agents_running: number;
  uptime_secs: number;
  /** ADR-055 D3: MQTT broker port for dynamic discovery (L3-6). */
  mqtt_port: number;
}

/** Node Agent entry — matches Gateway HTTP API GET /api/nodes (ADR-055 §6.13.3). */
export interface NodeInfo {
  node_id: string;
  online: boolean;
  online_since?: string;
  machine_uid?: string;
  hostname?: string;
  os?: string;
  arch?: string;
  node_version?: string;
  protocol_version?: number;
  capabilities: string[];
  max_agents?: number;
  agent_count?: number;
  http_endpoint?: string;
}

/** Agent list entry — matches Gateway API */
export interface AgentInfo {
  agent_id: string;
  name: string;
  display_name?: string;
  role?: string;
  avatar?: string;
  /**
   * Builtin avatar index declared in the agent's manifest.toml
   * (e.g. "icon-05"). Used as the default avatar on first install when
   * `avatar` (a packaged image path) is not set. The client normalises
   * and validates this against its bundled builtin icon set.
   */
  builtin_avatar?: string;
  version: string;
  running: boolean;
  connected: boolean;
  ready: boolean;
  dev_mode: boolean;
  /**
   * Whether DevMode is actually live for the running agent right now
   * (ADR-048 follow-up). Distinct from `dev_mode` (startup intent):
   * DevMode can be flipped on at runtime via
   * `POST /api/agents/{id}/debug/enable` without restarting the agent,
   * which flips this to `"enabled"` while `dev_mode` stays `false`.
   * Absent/undefined means the Gateway predates the field — treat as
   * `"disabled"`.
   */
  debug_state?: "enabled" | "disabled";
  debug_port?: number;
  /**
   * RFC3339 timestamp of the last user-driven interaction with this agent
   * (send_message / approval / question_answer / compact_context). Undefined
   * for agents the user has never interacted with. The Gateway already
   * returns agents in the canonical sidebar order; this field is exposed
   * for tooltips and future "last active" UI affordances.
   */
  last_interaction_at?: string | null;
  /**
   * RFC3339 timestamp the Runtime published the `sleeping` retained
   * status — i.e. when the auto-sleep watcher exited the process.
   * `null`/`undefined` for agents that are not currently sleeping.
   * Lets the UI render an "auto-slept at HH:MM" badge distinct from
   * "manually stopped" / "crashed" (both of which surface as
   * running=false, connected=false).
   */
  sleeping_at?: string | null;
}

/** Agent detail response */
export interface AgentDetail {
  agent_id: string;
  name: string;
  display_name?: string;
  role?: string;
  avatar?: string;
  /**
   * Builtin avatar index declared in the agent's manifest.toml
   * (e.g. "icon-05"). See `AgentInfo.builtin_avatar` for details.
   */
  builtin_avatar?: string;
  version: string;
  description: string;
  author: string;
  install_path: string;
  running: boolean;
  connected: boolean;
  ready: boolean;
  pid: number | null;
  started_at: string | null;
  /** Whether the agent was started with the `--dev-mode` flag (startup intent). */
  dev_mode?: boolean;
  /** Whether DevMode is live right now (can be enabled at runtime without restart). */
  debug_state?: "enabled" | "disabled";
}

/** Cost information for a model (per million tokens) */
export interface ModelCostInfo {
  /** Input cost per million tokens (USD) */
  input_per_million?: number;
  /** Output cost per million tokens (USD) */
  output_per_million?: number;
}

/** Modality information for a model */
export interface ModelModalities {
  /** Input modalities (e.g. "text", "image", "audio", "video") */
  input?: string[];
  /** Output modalities (e.g. "text", "image") */
  output?: string[];
}

/** Model capabilities info (from models.dev or user input) */
export interface ModelCapabilitiesInfo {
  /** Context window size (total tokens: input + output) */
  context_window: number;
  /** Maximum output tokens the model can generate */
  max_output_tokens: number;
  /** Whether the model supports tool/function calling */
  supports_tool_calling?: boolean;
  /** Whether the model supports reasoning/thinking */
  supports_reasoning?: boolean;
  /** Default reasoning effort level from model capabilities (auto/off/low/medium/high) */
  default_reasoning_effort?: string;
  /** Whether the model supports file attachments */
  supports_attachment?: boolean;
  /** Whether the model supports temperature parameter */
  supports_temperature?: boolean;
  /** Pricing information (USD per 1M tokens) */
  cost?: ModelCostInfo;
  /** Supported modalities */
  modalities?: ModelModalities;
  /** Model display name */
  name?: string;
  /** Model family */
  family?: string;
  /** Knowledge cutoff date */
  knowledge_cutoff?: string;
}

/** Per-model capabilities map (model ID → capabilities), matching vault structure */
export type ModelCapabilitiesMap = Record<string, ModelCapabilitiesInfo>;

/** Vault key entry (masked) */
export interface VaultKeyEntry {
  provider: string;
  key_preview: string;
  /** Optional base URL override for this provider */
  base_url?: string;
  /** Optional default model for this provider */
  default_model?: string;
  /** Selected models list (may be empty) */
  models?: string[];
  /** Per-model capabilities map (model ID → capabilities) */
  model_capabilities?: ModelCapabilitiesMap;
  /** Compact model for LLM summarization (ADR-010). null = use current model. */
  compact_model?: string;
  /** Whether this is a local (self-hosted) provider (no API key required) */
  local?: boolean;
  /** Whether this is a user-defined custom provider (not listed in models.dev) */
  custom?: boolean;
}

/** Gateway config response */
export interface GatewayConfig {
  socket_path: string;
  packages_dir: string;
  data_dir: string;
  log_level: string;
  idle_timeout_secs: number;
  dev_mode: boolean;
  http: {
    enabled: boolean;
    host: string;
    port: number;
    auth_enabled: boolean;
  };
  /** Default LLM provider (if configured) */
  default_provider?: string;
  /** Default LLM model (if configured) */
  default_model?: string;
  /// Global max output tokens limit (default 32768)
  max_output_tokens_limit: number;
  /// Log file max size in MB before auto-split (0 = disabled)
  log_file_size_mb: number;
  /** Maximum number of log files to keep (0 = unlimited, default 20) */
  log_file_count: number;
}

/** Generic message response */
export interface MessageResponse {
  message: string;
}

// ── Clone types ───────────────────────────────────────────────────────

/** Clone mode */
export type CloneMode = "skeleton" | "full";

/** Clone response from Gateway */
export interface CloneResponse {
  agent_id: string;
  install_path: string;
}

// ── Publish types ─────────────────────────────────────────────────────

/**
 * A single check item from `POST /api/agents/{id}/publish/prepare`.
 *
 * Wire shape mirrors `acowork_node::package::publish::CheckItem` (Node
 * is the source of truth — Gateway forwards the Node's `PrepareResult`
 * JSON verbatim). Field names MUST stay in sync with the Node side:
 * `name` and `detail` — not `field` / `message`. The earlier `field` /
 * `message` shape was stale from a pre-ADR-055 revision where the
 * Gateway itself implemented the checks; PublishWizard then read
 * `item.field` / `item.message` and silently got `undefined`, painting
 * every check item with the "error" red `XCircle` regardless of real
 * status.
 */
export interface CheckItem {
  name: string;
  status: string;
  detail?: string;
}

/**
 * Response from `POST /api/agents/{id}/publish/prepare`. Mirrors the
 * Node's `PrepareResult` shape forwarded by the Gateway.
 */
export interface PreparePublishResponse {
  checks: CheckItem[];
  warnings: string[];
  errors: string[];
  cleaned: boolean;
}

/** Publish build response */
export interface BuildPublishResponse {
  output_path: string;
  signed: boolean;
  file_size: number;
}

/** Export package response */
export interface ExportPackageResponse {
  status: string;
  output_path: string;
}

/** Send message response */
export interface SendMessageResponse {
  message_id: string;
  status: string;
}

/**
 * Gateway connection status.
 *
 * State machine:
 *   `connecting` → `connected` (success)
 *   `connecting` → `connected` → `error` (steady-state drop)
 *   `connecting` → `connecting` (startup probe failure; transient)
 *
 * Note: `disconnected` is preserved for external callers that explicitly
 * tear the gateway down (e.g. `stopLocalGateway`). During normal startup
 * we never sit in `disconnected` — the desktop immediately starts probing
 * `/health`, so the first observed state is `connecting`.
 */
export type GatewayStatus =
  | "connected"
  | "connecting"
  | "disconnected"
  | "error";

/** Todo item status from backend */
export type TodoStatus = "pending" | "in_progress" | "completed";

/** Todo list item (from todo_write built-in tool) */
export interface TodoItem {
  id: string;
  content: string;
  status: TodoStatus;
}

/** Chat message types */
export type MessageType = "user" | "assistant" | "system" | "tool_call" | "tool_result" | "thought" | "error" | "compaction";

// ──────────────────────────────────────────────────────────────────────
// ADR-046: Unified attachment entry types
//
// Mirrors backend `core/acowork-runtime/src/conversation.rs::AttachmentMeta`
// 1-to-1. The discriminant field is `type` (snake_case). Two of the five
// variants (`file_upload`, `image_upload`) carry a `documentId` and have a
// blob already on disk; the other three (`attached_file`, `attached_selection`,
// `attached_folder`) only carry a workspace `absPath` reference and expect the
// LLM to read via its own tools.
//
// Frontend-only fields (kept on PendingAttachedItem, NOT on AttachedItem):
//   - `tempId` / `status` / `errorMessage` / `base64Url` / `localUrl`
//   - only the *successfully persisted* item is promoted from the pending
//     form to an AttachedItem before being sent.
// ──────────────────────────────────────────────────────────────────────

/** User-uploaded document (PDF/DOCX/PPTX/XLSX). Blob at
 * `<work_dir>/files/<sanitizedStem>_<documentId>.<safeExt>` — see
 * `core/acowork-runtime/src/usecases/attachment.rs::on_disk_name`. */
export interface FileUploadItem {
  type: "file_upload";
  documentId: string;
  filename: string;
  /** Lowercase extension without the dot (e.g. "pdf"). */
  format: string;
  sizeBytes: number;
  /** Frontend-generated client ID for optimistic insertion.
   *  When set, the Runtime writes the JSONL entry with this exact ID
   *  so the optimistic overlay can be cleared via ID deduplication. */
  clientId?: string;
}

/** User-uploaded image (PNG/JPG). Blob at `<work_dir>/files/<documentId>`.
 *  `width`/`height` are best-effort hints from `new Image()` onLoad in the
 *  desktop frontend; absent for non-desktop clients — the renderer falls
 *  back to `<img onLoad>` natural sizing. */
export interface ImageUploadItem {
  type: "image_upload";
  documentId: string;
  filename: string;
  format: string;
  sizeBytes: number;
  width?: number;
  height?: number;
  clientId?: string;
}

/** Workspace file attached via "Add to Chat" (read-only reference, not copied). */
export interface AttachedFileItem {
  type: "attached_file";
  absPath: string;
  name: string;
  clientId?: string;
}

/** Workspace selection with explicit line range. */
export interface AttachedSelectionItem {
  type: "attached_selection";
  absPath: string;
  name: string;
  /** 1-based start line (inclusive). */
  startLine: number;
  /** 1-based end line (inclusive). */
  endLine: number;
  clientId?: string;
}

/** Workspace folder (contents NOT copied — LLM walks path via its own tools). */
export interface AttachedFolderItem {
  type: "attached_folder";
  absPath: string;
  name: string;
  clientId?: string;
}

/**
 * Discriminated union for the 5 metadata variants on a user message entry.
 *
 * Two shapes are at play, and they look similar but are **NOT identical**:
 *
 * | shape | file | discriminator tag | inner fields |
 * |---|---|---|---|
 * | **wire** (MQTT payload) | `acowork_core::protocol::AttachedItem` | snake_case `file_upload` | **camelCase** `documentId`/`sizeBytes`/`absPath`/`startLine`/`endLine` |
 * | **JSONL persistence** | `acowork_runtime::conversation::AttachmentMeta` | snake_case `file_upload` | **snake_case** `document_id`/`size_bytes`/`abs_path`/`start_line`/`end_line` |
 *
 * The wire shape is what the chatStore sends via `toWireAttachedItems`
 * (see below); the runtime maps it into the JSONL persistence shape in
 * `agent/loop_memory.rs::write_attached_items`. Mixing the two is the
 * exact bug `core/acowork-core/tests/attached_items_wire.rs` was
 * written to lock down.
 */
export type AttachedItem =
  | FileUploadItem
  | ImageUploadItem
  | AttachedFileItem
  | AttachedSelectionItem
  | AttachedFolderItem;

/**
 * Convert an `AttachedItem[]` to the JSON wire shape the MQTT boundary
 * sends to the Runtime.
 *
 * The wire contract is locked by the Rust deserializer
 * `acowork_core::protocol::AttachedItem` (ADR-046, see
 * `core/acowork-core/src/protocol.rs`). The discriminant tag `type` is
 * snake_case (`file_upload`, `attached_selection`, …) per the enum's
 * `#[serde(rename_all = "snake_case")]`; the **inner field names are
 * camelCase** per each variant's `#[serde(rename_all = "camelCase")]`
 * (e.g. `documentId`, `sizeBytes`, `absPath`, `startLine`, `endLine`).
 * Mixing the two styles would let the runtime silently drop every
 * item — the MQTT inbound at `gateway_loop.rs` parses each item via
 * `serde_json::from_value::<AttachedItem>(...).ok()` and unparseable
 * entries are discarded without diagnostic, which is why a wrong field
 * style surfaced as "all attachments vanish after send" rather than a
 * visible error.
 *
 * The shape is cross-language regression-locked by
 * `core/acowork-core/tests/attached_items_wire.rs`, which reads
 * `core/acowork-core/tests/fixtures/desktop_attached_items.json`
 * (regenerated by `apps/acowork-desktop/scripts/dump-attached-wire.mts`)
 * and asserts every entry deserializes back into `AttachedItem`.
 *
 * Used by `chatStore.sendMessage` at the MQTT boundary before publishing
 * the `params_json.attached_items` field. The runtime stores this same
 * shape verbatim in the JSONL `attached_items` field of the user entry.
 *
 * @see {@link https://github.com/.../docs/adr/zh/ADR-046-unified-attachment-entries.md}
 */
export function toWireAttachedItems(items: readonly AttachedItem[]): unknown[] {
  return items.map((item) => {
    switch (item.type) {
      case "file_upload": {
        const out: Record<string, unknown> = {
          type: "file_upload",
          documentId: item.documentId,
          filename: item.filename,
          format: item.format,
          sizeBytes: item.sizeBytes,
        };
        if (item.clientId !== undefined) out.clientId = item.clientId;
        return out;
      }
      case "image_upload": {
        const out: Record<string, unknown> = {
          type: "image_upload",
          documentId: item.documentId,
          filename: item.filename,
          format: item.format,
          sizeBytes: item.sizeBytes,
        };
        if (item.width !== undefined) out.width = item.width;
        if (item.height !== undefined) out.height = item.height;
        if (item.clientId !== undefined) out.clientId = item.clientId;
        return out;
      }
      case "attached_file": {
        const out: Record<string, unknown> = {
          type: "attached_file",
          absPath: item.absPath,
          name: item.name,
        };
        if (item.clientId !== undefined) out.clientId = item.clientId;
        return out;
      }
      case "attached_selection": {
        const out: Record<string, unknown> = {
          type: "attached_selection",
          absPath: item.absPath,
          name: item.name,
          startLine: item.startLine,
          endLine: item.endLine,
        };
        if (item.clientId !== undefined) out.clientId = item.clientId;
        return out;
      }
      case "attached_folder": {
        const out: Record<string, unknown> = {
          type: "attached_folder",
          absPath: item.absPath,
          name: item.name,
        };
        if (item.clientId !== undefined) out.clientId = item.clientId;
        return out;
      }
    }
  });
}

/** Narrow an `AttachedItem` to its upload variant.
 *  Returns the upload item iff it's a `file_upload` or `image_upload`;
 *  otherwise `null`. Convenience for the renderer: image/file uploads
 *  share the same display path (chip + optional thumbnail) while the three
 *  `attached_*` variants are pure workspace references. */
export function isUploadItem(item: AttachedItem):
  item is FileUploadItem | ImageUploadItem {
  return item.type === "file_upload" || item.type === "image_upload";
}

/** Status of a pending attachment before the blob (if needed) lands on disk.
 *  Frontend-only. Promoted to an `AttachedItem` with concrete `documentId` /
 *  persisted shape once `status === "success"`. */
export interface PendingAttachedItem {
  /** Local-only identifier used by React for keying; never crosses the wire. */
  tempId: string;
  status: "uploading" | "success" | "error";
  errorMessage?: string;
  /** Populated only when `status === "success"`: the resolved AttachedItem. */
  item?: AttachedItem;
  /** For `image_upload` items: a local `data:` URL used to render the
   *  pending thumbnail while the upload is in flight, and discarded after
   *  the bubble clears. Backend never sees this — `image_upload`'s wire
   *  payload is `documentId` + dimensions only. */
  localUrl?: string;
}

/** Chat message in the UI */
export interface ChatMessage {
  id: string;
  type: MessageType;
  content: string;
  timestamp: number;
  /** Per-session monotonic seq assigned by the Runtime's `next_seq()`
   *  counter. Set on entries received live via `stream_delta` /
   *  `record_complete`; absent on entries loaded from JSONL history. The
   *  Desktop uses it to place live frames at the correct position in
   *  `messages[]` (`insertBySeq`) even if MQTT delivers them out of
   *  order. See `acowork_data.proto::StreamDeltaPayload.seq` and
   *  `RecordCompletePayload.seq`. */
  seq?: number;
  /** Sender display name for chat bubble (e.g. "PM", "我") */
  senderDisplayName?: string;
  /** Sender avatar URL or data URI */
  senderAvatar?: string;
  /** Sender role label (e.g. "Project Manager") */
  senderRole?: string;
  /** For tool_call: tool name */
  toolName?: string;
  /** LLM-generated tool_call.id — used to match approval events to specific tool calls */
  toolCallId?: string;
  /** For tool_call/tool_result: parameters or result JSON */
  toolData?: Record<string, unknown>;
  /** For tool_call: duration in ms */
  duration?: number;
  /** For tool_call/tool_result: success/failure */
  toolStatus?: "success" | "error";
  /** Token usage from done event */
  usage?: TokenUsage;
  /** Turn/iteration ID — groups thinking + tools + reply in one LLM call cycle */
  turnId?: string;
  /** Timestamp when this message started (for duration calculation) */
  startTime?: number;
  /** Timestamp when this message ended (set by done event, fixes perpetual timer) */
  endTime?: number;
  /** For type=compaction: structured metadata parsed from CompactionEventMeta */
  compactionMeta?: CompactionEventMeta;
  /** For type=error: raw error detail (shown in expandable "Details" section) */
  errorDetail?: string;
  /** For type=error: error type string for conditional rendering */
  errorType?: string;
  /** ADR-027: `true` when this message is an in-progress streaming line projected
   *  into messages[] by the Gateway. Renders with a pulse cursor and its content
   *  is replaced by id on each poll (no placeholder machinery needed). */
  isStreaming?: boolean;
  /** Raw metadata from the JSONL entry. Used by system entries to carry
   *  attachment metadata (ADR-046 §2.5: 5 metadata.type branches). */
  metadata?: Record<string, unknown>;
  /** Internal flag: this entry is an optimistic (unconfirmed) insert from
   *  the frontend. Set by `sendMessage` on attachment system entries and
   *  cleared when the HTTP window lands (same id → server copy wins).
   *  Used by `AttachmentChipRow` to render the pending visual state. */
  _isOptimistic?: true;

}

/** Compaction event metadata (mirrors backend `CompactionEventMeta`).
 *  Carried inside `ConversationEntry.metadata` when `kind === "compaction"`. */
export interface CompactionEventMeta {
  /** First entry id covered by the summary (inclusive) */
  compacted_from_id?: string;
  /** Last entry id covered by the summary (inclusive) */
  compacted_to_id?: string;
  /** ADR-061: 8-level compression strategy level selected (1-8, 0 = none) */
  level: number;
  /** Compaction model used (diagnostic only) */
  model?: string;
  /** History token estimate before compaction (diagnostic only) */
  before_tokens: number;
  /** History token estimate after compaction (diagnostic only) */
  after_tokens: number;
}

/** Token usage stats */
export interface TokenUsage {
  prompt_tokens: number;
  completion_tokens: number;
  total_tokens: number;
}

/** Context usage info reported by Runtime, forwarded via Gateway WebSocket
 *
 *  Per-turn fields (`input_tokens`, `output_tokens`, `total_tokens`) reflect the
 *  most recent LLM call only. Cumulative session fields
 *  (`total_input_tokens`, `total_output_tokens`) accumulate across all LLM
 *  calls in the session and are sourced from the runtime's SessionTokens.
 *  They are `undefined` until the first LLM call has been recorded (e.g.
 *  before any tool call has been made in a fresh session).
 *
 *  Cumulative agent fields (`agent_total_input_tokens`,
 *  `agent_total_output_tokens`) aggregate across every LLM call made by the
 *  Runtime process for this agent. They are the **live** data source — see
 *  the `agentTokenTotals` field on `AgentStorage` for the fallback copy that
 *  rides along in the `GET /api/agents/:id/sessions` response.
 */
export interface ContextUsageInfo {
  /** Context window limit (from model capabilities) */
  context_window: number;
  /** Current input tokens used (prompt_tokens from API response, last turn) */
  input_tokens: number;
  /** Current output tokens generated (completion_tokens, last turn) */
  output_tokens: number;
  /** Total tokens of the last turn (input + output) */
  total_tokens: number;
  /** Max input tokens (from models.dev limit.input, if available) */
  max_input_tokens?: number;
  /** Usable context space */
  usable_context: number;
  /** Usage percentage (0-100) */
  usage_percent: number;
  /** Cumulative input tokens across all LLM calls in this session.
   *  Undefined until the first LLM call has been recorded. */
  total_input_tokens?: number;
  /** Cumulative output tokens across all LLM calls in this session.
   *  Undefined until the first LLM call has been recorded. */
  total_output_tokens?: number;
  /** ADR-028: cumulative input tokens across every LLM call made by this
   *  Runtime process for this agent (live data source). Always `Some` from
   *  ADR-028-aware Runtimes — `undefined` only on legacy clients. */
  agent_total_input_tokens?: number;
  /** ADR-028: cumulative output tokens across every LLM call made by this
   *  Runtime process for this agent. See `agent_total_input_tokens`. */
  agent_total_output_tokens?: number;
  /** ADR-066: cache-hit tokens reported by the Provider on the last turn
   *  (Anthropic `cache_read_input_tokens`, OpenAI `prompt_tokens_details.cached_tokens`).
   *  Undefined on providers that do not return cache accounting. */
  cache_read_tokens?: number;
  /** ADR-066: cache-write tokens reported by the Provider on the last turn
   *  (Anthropic `cache_creation_input_tokens`). Provider-billed as upfront
   *  cost when seeding the cache. */
  cache_write_tokens?: number;
  /** ADR-066: cumulative cache-hit tokens across all turns in this session.
   *  Populated from `SessionTokens.total_cache_read`. */
  total_cache_read_tokens?: number;
  /** ADR-066: cumulative cache-write tokens across all turns in this session.
   *  Populated from `SessionTokens.total_cache_write`. */
  total_cache_write_tokens?: number;
  /** ADR-066: cumulative cache-hit tokens across every LLM call made by
   *  this Runtime process for this agent. */
  agent_total_cache_read_tokens?: number;
  /** ADR-066: cumulative cache-write tokens across every LLM call made by
   *  this Runtime process for this agent. */
  agent_total_cache_write_tokens?: number;
}

/** Navigation view type */
export type NavView = "chat" | "harness" | "docs" | "projects" | "settings";

/** Theme type */
export type Theme = "light" | "dark" | "system";

/** Model info from models.dev via Gateway API */
export interface ModelInfo {
  id: string;
  name: string;
  family?: string;
  reasoning?: boolean;
  tool_call?: boolean;
  attachment?: boolean;
  temperature?: boolean;
  release_date?: string;
  /** Context window size (total tokens: input + output) */
  context_window?: number;
  /** Maximum output tokens */
  max_tokens?: number;
  /** Knowledge cutoff date */
  knowledge?: string;
  /** Input cost per million tokens (USD) */
  input_cost?: number;
  /** Output cost per million tokens (USD) */
  output_cost?: number;
  /** Input modalities */
  input_modalities?: string[];
  /** Output modalities */
  output_modalities?: string[];
}

/** Provider models response from Gateway API */
export interface ProviderModelsResponse {
  id: string;
  name: string;
  models: ModelInfo[];
}

/** Model entry with optional capability info for display */
export interface ModelEntry {
  name: string;
  provider: string;
  /** Whether the model supports tool/function calling */
  tool_call?: boolean;
  /** Whether the model supports reasoning/thinking */
  reasoning?: boolean;
  /** Default reasoning effort level from model capabilities (auto/off/low/medium/high) */
  default_reasoning_effort?: string;
  /** Input modalities (e.g. "text", "image") */
  input_modalities?: string[];
}

/** Provider list entry from Gateway API */
export interface ProviderListEntry {
  id: string;
  name: string;
  model_count: number;
  /** Provider's base API URL (from models.dev or offline data) */
  api?: string;
  /** Whether this is a local (self-hosted) provider (no API key required) */
  local?: boolean;
  /** Whether this is a user-defined custom provider (not listed in models.dev) */
  custom?: boolean;
}

// ── ADR-056: Global default compact model (cross-provider pick) ──────

/** Cross-provider reference to a (provider_id, model_id) pair.
 *  Lives at the top level of `provider_list.json` — independent of
 *  any single provider's `compact_model` field. */
export interface CompactModelRef {
  provider_id: string;
  model_id: string;
}

/** Response from `GET /api/settings/default-compact-model`. */
export interface DefaultCompactModelResponse {
  default_compact_model: CompactModelRef | null;
}

// ── Memory types ──────────────────────────────────────────────────────

/** Single memory node in the list response */
export interface MemoryNodeResponse {
  node_id: number;
  node_type: string;
  /**
   * Secondary classification inside the storage layer. For `Knowledge`
   * nodes: `Fact` | `Preference` | `Relation` | `Procedure`. For
   * `Autobiographical` nodes: `Identity` | `Capability` | `Limitation`
   * | `Preference` | `History` | `Relationship`. `undefined` for
   * `Episodic` / `Procedural` nodes. Drives the panel's per-row
   * badge and the secondary sub-filter dropdown.
   */
  sub_type?: string;
  content: string;
  /**
   * Raw `confidence` property on the node. 0 when the node type has no
   * such property (e.g. Episodic). Passed through verbatim — never derived.
   */
  confidence: number;
  /**
   * Raw `importance` property on the node. 0 when the node type has no
   * such property (e.g. Procedural / Autobiographical). Passed through
   * verbatim — never derived. For Episodic nodes this is the meaningful
   * score (compaction writes importance=0.7).
   */
  importance: number;
  decay_score: number;
  created_at: number;
  last_accessed_at: number;
  access_count: number;
  status: string;
}

/** Paginated list of memory nodes */
export interface MemoryNodesListResponse {
  total: number;
  page: number;
  size: number;
  nodes: MemoryNodeResponse[];
}

/** Memory statistics summary */
export interface MemoryStatsResponse {
  total_nodes: number;
  storage_bytes: number;
  by_type: Record<string, number>;
  by_status: Record<string, number>;
  avg_decay_score: number;
  index_health: string;
  /**
   * Embedding dimension of the Grafeo HNSW vector index actually persisted
   * on disk. 0 if the store has not yet built a vector index.
   */
  stored_dim: number;
  /**
   * Number of memory nodes (across all labels) that currently have a non-NULL
   * `embedding` field and therefore participate in vector search. Compare
   * against `total_nodes` to detect missing embeddings.
   */
  nodes_with_embedding: number;
  /**
   * Embedding dimension of the active embedding provider (model output).
   * 0 if no embedding provider is currently configured. Used together with
   * `stored_dim` to detect a dimension mismatch.
   */
  model_dim: number;
}

/** Response for deleting a memory node */
export interface DeleteNodeResponse {
  node_id: number;
  deleted: boolean;
  message: string;
}

/** Response for memory consolidation trigger */
export interface ConsolidateResponse {
  started: boolean;
  duration_ms: number;
  episodes_consolidated: number;
  knowledge_nodes_generated: number;
  message: string;
}

// ── Skill types ───────────────────────────────────────────────────────

/** A single skill entry in the list response */
export interface SkillListEntry {
  name: string;
  description: string;
  version: string | null;
  author: string | null;
  triggers: string[];
  tool_deps: string[];
}

/** Paginated list of skills */
export interface SkillListResponse {
  total: number;
  page: number;
  size: number;
  skills: SkillListEntry[];
}

/** Detailed skill information */
export interface SkillDetailResponse {
  name: string;
  description: string;
  version: string | null;
  author: string | null;
  triggers: string[];
  tool_deps: string[];
  instructions: string;
}

/** Skill execution history */
export interface SkillExecutionHistoryResponse {
  skill_name: string;
  total_executions: number;
  page: number;
  size: number;
  executions: unknown[];
}

// ── Tool approval types ───────────────────────────────────────────────

/** Tool approval needed event from WebSocket */
export interface ToolApprovalNeededEvent {
  type: "tool_approval_needed";
  request_id: string;
  agent_id: string;
  tool_name: string;
  risk_level: "Low" | "Medium" | "High";
  /** Session ID that originated this approval (used for multi-session routing) */
  session_id?: string;
  /** LLM-generated tool_call.id for precise UI matching to tool call items */
  tool_call_id?: string;
  /** Approval timeout in seconds — frontend shows countdown, Runtime auto-rejects after this */
  approval_timeout_secs?: number;
  shell_command?: {
    command: string;
    preview: string;
    risk_assessment: string;
  };
  params: Record<string, unknown>;
  params_summary: string;
}

/** Tool approval request payload */
export interface ToolApprovalResponse {
  request_id: string;
  action: "allow" | "deny" | "allow_all_session";
}

// ── Ask question types ────────────────────────────────────────────────

/** A single option in an ask_user_question prompt */
export interface QuestionOption {
  label: string;
  description?: string;
}

/** Ask question event from WebSocket (ask_user_question tool) */
export interface AskQuestionEvent {
  type: "ask_question";
  request_id: string;
  agent_id: string;
  question: string;
  options: QuestionOption[];
  title?: string;
  /**
   * Effective wait timeout in seconds, computed by the runtime from the
   * agent's `approval_timeout_secs` config (user preference, default 300s).
   *
   * Frontend shows a countdown based on this value. The Runtime auto-cancels
   * (returns "[Timeout: user did not respond]") once it elapses.
   */
  timeout_seconds?: number;
  /** Session ID that originated this question (used for multi-session routing) */
  session_id?: string;
}

/** Question answer request payload */
export interface QuestionAnswerRequest {
  request_id: string;
  answer: string;
  session_id?: string;
}

/** Question answer API response */
export interface QuestionAnswerResponse {
  request_id: string;
  status: string;
}

/** Approval API response */
export interface ApprovalApiResponse {
  request_id: string;
  action: string;
  status: string;
}

// ── Session types ─────────────────────────────────────────────────────

/** Session summary from Gateway */
export interface SessionInfo {
  session_id: string;
  created_at: string;
  last_active_at?: string;
  message_count: number;
  title: string | null;
  /** ADR-014: Session lifecycle status from backend (source of truth) */
  status?: SessionStatus;
  /** Per-session workspace selection ("__agent_home__" = agent home) */
  workspace_id?: string;
}

/**
 * ADR-014 + ADR-049: Session lifecycle status — read-only from backend.
 *
 * ADR-049 splits the old `streaming` variant into 3 sub-states so the
 * frontend can derive the processing phase directly from this status
 * without composing from data parameters (e.g. stream_delta line counts).
 * This type MUST stay 1:1 in sync with `SessionStatusDto` in
 * core/acowork-core/src/protocol.rs and `SessionStatus` in
 * core/acowork-runtime/src/agent/session_state.rs.
 */
export type SessionStatus =
  | { status: "idle" }
  /** ADR-049: TTFT wait phase — HTTP request sent, awaiting first chunk. */
  | { status: "llm_awaiting_first_chunk" }
  | { status: "thinking" }
  | { status: "llm_streaming"; detail?: { message_id: string | null } }
  /** ADR-049: Tool calls dispatched, awaiting tool results. */
  | { status: "tool_executing" }
  | { status: "waiting_approval"; detail: { request_id: string } }
  | {
    status: "paused";
    detail?: {
      iteration: number | null;
      max_iterations: number | null;
      /** 429 retry wait info — present when the provider is rate-limited */
      retry_info?: {
        wait_ms: number;
        attempt: number;
        max_attempts: number;
        provider: string;
      };
      /**
       * Why the session paused (mirrors runtime `PauseReason`).
       * `undefined` for 429 retry waits — `retry_info` disambiguates those.
       */
      reason?: "iteration_limit" | "loop_detected" | "debug";
      /** Human-readable pause message (e.g. iteration limit hint / loop detection detail) */
      message?: string;
    };
  };

/**
 * ADR-049: UI-facing coarse processing phase.
 *
 * Frontend code should derive `ProcessingPhase` from `SessionStatus` via
 * the pure `getProcessingPhase()` helper and drive UI behavior from there,
 * never from the raw status. The phase enum is closed (4 values); adding a
 * new phase requires a code-level decision, not a silent data drift.
 */
export type ProcessingPhase =
  | "idle"
  /** Waiting for the model — TTFT wait or inter-step processing */
  | "waiting"
  /** LLM is producing reasoning/thinking content */
  | "thinking"
  /** LLM is actively streaming visible reply content */
  | "streaming"
  /** Tool calls dispatched, results pending */
  | "tool_executing"
  /** Tool is asking the user for approval or answer */
  | "waiting_approval"
  /** Iteration limit reached, debug pause, or 429 retry wait */
  | "paused";

/**
 * ADR-049: Pure mapping from `SessionStatus` to `ProcessingPhase`.
 *
 * Single source of truth — every UI decision (`<Indicator />`, `<StreamingPreview />`,
 * `<SendButton disabled>`) MUST route through this function. The exhaustive
 * `switch` is checked by the TypeScript compiler: adding a new `SessionStatus`
 * variant will fail compilation until the mapping is updated.
 */
export function getProcessingPhase(s: SessionStatus | undefined | null): ProcessingPhase {
  if (!s) return "idle";
  switch (s.status) {
    case "idle":
      return "idle";
    case "llm_awaiting_first_chunk":
      return "waiting";
    case "thinking":
      return "thinking";
    case "llm_streaming":
      return "streaming";
    case "tool_executing":
      return "tool_executing";
    case "waiting_approval":
      return "waiting_approval";
    case "paused":
      return "paused";
  }
}

/**
 * ADR-049: True when the session is actively processing (non-idle).
 *
 * Equivalent to `getProcessingPhase(s) !== "idle"`. This replaces the old
 * `isProcessing()` which covered only 3 of the 6 variants.
 */
export function isProcessing(s: SessionStatus | undefined | null): boolean {
  return getProcessingPhase(s) !== "idle";
}

/**
 * Helper: get message_id from LlmStreaming status, or null.
 *
 * ADR-049: the old `streaming` variant is now `llm_streaming`. This helper
 * returns the message_id when the session is in the active streaming phase
 * (after the first content chunk has arrived, before tools take over).
 */
export function getStreamingMessageId(s: SessionStatus | undefined | null): string | null {
  if (!s || s.status !== "llm_streaming") return null;
  return s.detail?.message_id ?? null;
}

// ── ADR-038: lifecycle event payload types ──────────────────────────
//
// These are the *flat JSON* shapes the Tauri bridge emits on the
// `agent-event` Tauri channel after decoding the corresponding
// `SessionOpened` / `SessionNotOpened` MQTT envelopes
// (`apps/acowork-desktop/src-tauri/src/commands/chat_mqtt.rs`).
//
// The fields are camelCase_titles_to_underscore except where the Runtime
// carries an existing schema — `status`, `reason` etc. are status strings
// that the Desktop renders / branches on verbatim (see chatStore
// `case "session_opened"` / `case "session_not_opened"`).
//
// The `type` discriminator field is always `"session_opened"` or
// `"session_not_opened"`. `agent_id` is not on the wire (the topic path
// carries it) but the bridge forwards it for parity with sibling events.
/**
 * Flat JSON payload for `SessionOpened` (ADR-038, proto field 35).
 *
 * Published by the Runtime on the agent-scoped
 * `acowork/agents/{id}/sessions/{sid}/opened` retained topic after the
 * Runtime transitions a session into the **Active** state (idempotent
 * no-op when already Active, lazy-resume from JSONL when Closed, hard
 * load when NotFound).
 *
 * The Desktop uses this event to (a) flip `isSessionReady = true` so
 * the input area / send button unlock, (b) seed the session header
 * metadata (model, provider, last_active_at) so the user can see which
 * model was used before they type a word.
 */
export interface SessionOpenedEvent {
  type: "session_opened";
  agent_id: string;
  session_id: string;
  /**
   * Outcome discriminator.
   *   - `"already_active"`     — idempotent no-op (already in memory)
   *   - `"resumed_from_disk"`  — lazy-loaded from JSONL+meta into memory
   * Legacy Runtimes pre-ADR-038 may publish `"created"` (no
   * distinguishing semantics for the Desktop).
   */
  status: "already_active" | "resumed_from_disk" | "created" | string;
  model?: string;
  provider?: string;
  /** ISO-8601 timestamp from the session meta file, when available. */
  last_active_at?: string;
}

/**
 * Flat JSON payload for `SessionNotOpened` (ADR-038, proto field 36).
 *
 * Published by the Runtime whenever a session-level control command
 * (e.g. `chat_message`, `model_switch`, `stop`) is rejected because the
 * target session is **Closed** or **NotFound**. The Desktop listens for
 * this event to (a) flip `isSessionReady = false`, (b) surface a toast
 * with a one-click reopen affordance so the contract violation is
 * observable to the user instead of silently dropping the message.
 */
export interface SessionNotOpenedEvent {
  type: "session_not_opened";
  agent_id: string;
  session_id: string;
  /**
   * The control command that was rejected (e.g. `"chat_message"`,
   * `"model_switch"`, `"stop"`). Used only for diagnostic / logging —
   * the Desktop doesn't branch on this when deciding to surface a toast
   * (the rejection itself is the signal).
   */
  attempted_command: string;
  /**
   * Why the session is not Active. The Desktop treats both values as
   * the same surface-level affordance (a reopen button). Reviewers can
   * spot a `"session_not_found"` event as a sign the user is typing
   * into a session that was deleted while the tab was open.
   */
  reason: "session_not_found" | "session_closed" | string;
}

/** A single conversation entry as stored in JSONL */
export interface ConversationEntry {
  id: string;
  ts: string;
  role: "user" | "assistant" | "think" | "thought" | "tool_call" | "tool_result" | "system";
  content: string;
  metadata?: Record<string, unknown>;
  /** Entry kind. `undefined`/`"message"` denotes a regular role-based message.
   *  `"compaction"` denotes an LLM-driven compaction summary event whose
   *  `content` is the summary text and `metadata` is `CompactionEventMeta`. */
  kind?: string;
  /** ADR-027: `true` when this entry is an in-progress streaming line
   *  projected into messages[] by the Gateway. Content is the FULL
   *  accumulated text (not a delta) — the frontend replaces by id each poll. */
  is_streaming?: boolean;
}

/**
 * Paginated messages response from Gateway.
 *
 * Pagination uses an `offset` model — direction is NOT a parameter. Both
 * `offset` and `limit` are measured in **raw entries** (one JSONL line
 * each: a single user / assistant / thought / tool_call / tool_result
 * row). A displayed "explore group" (think + tool_call + tool_result
 * folded into one chip) is a **frontend UI abstraction only** and is
 * never exposed on the wire.
 *
 * ADR-050: Forward (oldest-end) offset model.
 * - `offset=0` returns the **oldest** `limit` raw entries (entries [0, limit)).
 * - `offset=K` returns entries [K, K+limit) clamped to total.
 * - To scroll older:  `offset = max(0, offset - limit)`.
 * - To scroll newer:  `offset = offset + limit`.
 * - At oldest (head):  `offset == 0`.
 * - At newest (tail):  `offset + limit >= total`.
 */
export interface PaginatedMessages {
  messages: ConversationEntry[];
  /** Echo of the requested offset, in raw entries (0 = oldest). */
  offset: number;
  /** Number of raw entries actually returned (≤ requested limit). */
  limit: number;
  /** Total raw-entry line count in the session JSONL. */
  total: number;
}

/** A single streaming line from MQTT stream_delta (thought only). */
export interface StreamLine {
  role: "thought" | "assistant";
  lineNo: number;
  content: string;
}

/** Per-session active stream tracker.
 *  - assistant: lineCount for isAssistantReplying threshold
 *  - thought: lines (cap 5) + startTime for ThinkBlock display */
export interface ActiveStream {
  messageId: string;
  role: "thought" | "assistant";
  /** Assistant: line count for threshold. Thought: not used. */
  lineCount: number;
  /** Thought only: last 5 lines for ThinkBlock preview. */
  lines: StreamLine[];
  /** Thought only: timestamp when thinking started. */
  startTime: number;
}

/**
 * ADR-035 D3: per-session data store. Each opened session owns one.
 *
 * - `messages[]` is the current cache window covering the offset range
 *   `[loadedOffset, loadedOffset + loadedCount)` out of `loadedTotal`.
 *   It is NOT full history.
 * - The window slides by `offset ± limit` requests to the paginated
 *   messages HTTP endpoint (no `direction` parameter; see PaginatedMessages).
 *   MQTT `record_complete` and `tool_call`/`tool_result` events append
 *   to `messages[]` and bump `loadedTotal`; the user sees new content by
 *   scrolling down or paging down (no separate "forward load" needed).
 * - HTTP scroll-back trims the newest end of the window; HTTP scroll-forward
 *   trims the oldest end — both keep the window size bounded by
 *   MESSAGE_CACHE_WINDOW.
 * - `activeStream` is the single active streaming buffer (null when idle).
 * - Receiving/storing is decoupled from rendering (foreground flag only
 *   controls rendering, not data receipt).
 *
 * Note: the actual runtime storage is split across Zustand `SessionChatState`
 * (messages, offset/total, meta) and the module-level `activeStreams` Map
 * in chatStore.ts. This interface documents the logical grouping per ADR D3.
 */
export interface SessionDataStore {
  sessionId: string;
  messages: ConversationEntry[];
  activeStream: ActiveStream | null;
  /** Window's starting offset (forward semantics: 0 = oldest entry). */
  loadedOffset: number;
  /** Number of message entries in the cache window. */
  loadedCount: number;
  /** Total message count in the session (returned by HTTP /messages). */
  loadedTotal: number;
  meta: unknown | null;
  subscribed: boolean;
  foreground: boolean;
}

// ── User profile (persisted in localStorage) ──────────────────────────

/** Avatar generation style from boring-avatars */
export type BoringAvatarVariant = "beam" | "marble" | "pixel" | "sunset" | "ring" | "bauhaus";

/** How the user's avatar is generated */
export type AvatarType = "boring" | "icon" | "letter";

/** Color palette preset ID */
export type ColorPalette = "rainbow" | "ocean" | "forest" | "sunset" | "neon";

/** Color palette definitions for boring-avatars */
export const COLOR_PALETTES: Record<ColorPalette, string[]> = {
  rainbow: ["#FF6900", "#FCB900", "#7BDCB5", "#00D084", "#8ED1FC", "#0693E3", "#ABB8C3", "#EB144C", "#F78DA7", "#9900EF"],
  ocean: ["#0066CC", "#0088FF", "#00AAFF", "#44CCFF", "#88DDEE", "#6699CC", "#336699", "#003366"],
  forest: ["#2D6A4F", "#40916C", "#52B788", "#74C69D", "#95D5B2", "#1B4332", "#081C15"],
  sunset: ["#FF6B35", "#F7C59F", "#EFE9E7", "#2D82B7", "#1E5F74", "#FF4500", "#FFD700"],
  neon: ["#FF006E", "#8338EC", "#3A86FF", "#06D6A0", "#FFBE0B", "#FB5607"],
};

/** User profile stored in localStorage */
export interface UserProfile {
  /** User's display name shown in chat */
  displayName: string;
  /** How the avatar is generated */
  avatarType: AvatarType;
  /** Boring Avatars variant (when avatarType = "boring") */
  avatarVariant: BoringAvatarVariant;
  /** Seed string for deterministic avatar (default = "user") */
  avatarSeed: string;
  /** Built-in icon ID (when avatarType = "icon") */
  avatarIcon: string | null;
  /** Color palette ID */
  colorPalette: ColorPalette;
  /** Custom colors override (when non-empty) */
  avatarColors: string[];
  /** Backend custom avatar file path (e.g. "assets/avatar-01.png"), synced from Gateway. */
  backendAvatarUrl?: string | null;
  /** Backend builtin avatar icon ID (e.g. "icon-05"), synced from Gateway. */
  backendBuiltinAvatarId?: string | null;
}

// ── Backend User Profile (Gateway /api/users) ─────────────────────────

/** Backend UserProfile — matches acowork_core::protocol::UserProfile */
export interface BackendUserProfile {
  user_id: string;
  display_name: string;
  language: string;
  timezone: string;
  city?: string;
  country?: string;
  occupation?: string;
  communication_style?: string;
  custom?: Record<string, string>;
  created_at: string;
  updated_at: string;
  is_active: boolean;
  /** Custom avatar path (relative to Gateway data_dir, e.g. "assets/avatar-01.png") */
  avatar?: string | null;
  /** Builtin avatar icon ID (e.g. "icon-05") */
  builtin_avatar?: string | null;
}

/** Response from GET /api/users */
export interface UserProfileListResponse {
  version: number;
  users: BackendUserProfile[];
}

/** Response from POST/PUT /api/users — matches UserResponse in acowork-gateway */
export interface UserProfileMutationResponse {
  user: BackendUserProfile;
  version: number;
}

/** Response from POST /api/users/{user_id}/activate — matches ActivateResponse */
export interface ActivateUserResponse {
  active_user_id: string;
  version: number;
}

/** Request body for POST /api/users */
export interface CreateUserRequest {
  display_name: string;
  language: string;
  timezone: string;
  city?: string;
  country?: string;
  occupation?: string;
  communication_style?: string;
  custom?: Record<string, string>;
}

/** Request body for PUT /api/users/{user_id} */
export interface UpdateUserRequest {
  display_name?: string;
  language?: string;
  timezone?: string;
  city?: string;
  country?: string;
  occupation?: string;
  communication_style?: string;
  custom?: Record<string, string>;
}

// ── MCP types ────────────────────────────────────────────────────────

/** MCP transport type — matches McpTransportDef in acowork_core::protocol */
export type McpTransportDef = "stdio" | "http" | "sse";

/** MCP server config — matches McpServerConfigDef in acowork_core::protocol */
export interface McpServerConfigDef {
  name: string;
  transport: McpTransportDef;
  url?: string;
  command: string;
  args: string[];
  env: Record<string, string>;
  headers?: Record<string, string>;
  tool_timeout_secs?: number;
}

/** MCP catalog entry response (env values with sensitive fields masked) */
export interface McpCatalogEntryResponse extends McpServerConfigDef {
  /** Whether this entry has sensitive env vars that are masked */
  has_secrets: boolean;
}

/** Full MCP catalog response */
export interface McpCatalogResponse {
  servers: McpCatalogEntryResponse[];
}

/** Per-agent MCP server activation response */
export interface AgentMcpServersResponse {
  agent_id: string;
  active_servers: string[];
}

/** MCP probe response — result of a health check against an MCP server */
export interface McpProbeResponse {
  success: boolean;
  tool_count: number;
  tools: string[];
  error: string | null;
  duration_ms: number;
}

/** MCP server health status (frontend-only, derived from probe results) */
export type McpHealthStatus = "unknown" | "probing" | "healthy" | "unhealthy";

/** Request body for PUT /api/agents/{id}/mcp-servers */
export interface UpdateMcpServersRequest {
  servers: string[];
}

/** MCP preset category */
export type McpPresetCategory =
  | "file"
  | "search"
  | "database"
  | "vcs"
  | "cloud"
  | "communication"
  | "knowledge"
  | "browser"
  | "design"
  | "document"
  | "utility";

// ── Web Search Provider types ────────────────────────────────────────

/** Search key entry — matches Gateway HTTP API Vault search key response */
export interface SearchKeyEntry {
  provider: string;
  key_preview: string;
  /** Optional base URL override (required for SearXNG) */
  base_url?: string;
}

/** Search provider definition — static catalog entry for frontend */
export interface SearchProviderDef {
  /** Provider identifier (e.g. "tavily") */
  id: string;
  /** Display name */
  name: string;
  /** Short description */
  description: string;
  /** Whether this provider requires an API key */
  requires_api_key: boolean;
  /** Free quota string for display */
  free_quota: string;
  /** Default API endpoint */
  base_url: string;
}

/** Search provider list item — matched from Gateway search_list */
export interface SearchProviderListItem {
  id: string;
  name: string;
  description: string;
  requires_api_key: boolean;
  base_url: string;
}

/** Per-agent search provider activation config */
export interface AgentSearchProvider {
  provider: string;
  /** Priority: 1 = highest (tried first) */
  priority: number;
}

/** Per-agent search config stored in agent_search.json */
export interface AgentSearchConfig {
  providers: AgentSearchProvider[];
}

// ── LSP types ────────────────────────────────────────────────────────────

/**
 * Response from `GET /api/agents/{id}/lsp-endpoint` (Gateway).
 *
 * ADR-055 §6.7 (Phase 4): the LSP Relay is a node-local sidecar, so the
 * endpoint is resolved per agent — the Gateway looks up the node hosting
 * the agent and returns that node's advertised relay base URL.
 *
 * Desktop App queries this to discover the LSP Relay's address, then
 * connects directly to the relay's WebSocket and HTTP API.
 */
export interface AgentLspEndpointResponse {
  /** The agent id the lookup was performed for */
  agent_id: string;
  /** Node hosting the agent ("local" for Gateway-spawned agents) */
  node_id: string;
  /** Relay base URL (e.g. "http://127.0.0.1:19878"), null when not ready */
  endpoint: string | null;
  /** Whether the node's LSP relay is ready */
  ready: boolean;
}

/** LSP server entry — matches acowork_lsp_relay::config::LspServerEntry */
export interface LspServerEntry {
  /** Candidate command names (tried in order) */
  candidates: string[];
  /** Extra arguments for stdio-mode LSP communication */
  args: string[];
  /** Per-candidate arg overrides */
  candidate_args?: Record<string, string[]>;
  /** One-line install hint shown to the user */
  install_hint: string;
  /** Name of the install script file (e.g. "rust" → assets/lsp_install/rust.sh) */
  install_script?: string;
  /** Human-readable description */
  description: string;
}

/** LSP servers config — matches acowork_lsp_relay::config::LspServersConfig */
export interface LspServersConfig {
  /** Schema version */
  version: number;
  /** Language-keyed server entries (canonical language names only) */
  servers: Record<string, LspServerEntry>;
}

/**
 * Combined response from `GET /api/lsp/servers-with-status`.
 *
 * Returns the configured LSP server list together with per-language
 * install status in a single round-trip. Used on initial load and on
 * Refresh so the server list and install badges arrive atomically —
 * avoiding the 1–2s window where the list was visible but badges
 * had not yet been resolved.
 *
 * The keys of `servers` and `status` are guaranteed to match 1:1 by
 * the backend. A language present in `servers` but missing from
 * `status` should be treated as "unknown" by the UI.
 */
export interface LspServersWithStatus {
  /** Configured LSP servers */
  servers: LspServersConfig;
  /** Per-language install status, keyed by canonical language */
  status: Record<string, LspServerStatusEntry>;
}

/** LSP install script response from GET /api/lsp/install/{language} */
export interface LspInstallScriptResponse {
  language: string;
  filename: string;
  script: string;
  platform: string;
}

/** LSP install run response from POST /api/lsp/install/{language} */
export interface LspInstallRunResponse {
  language: string;
  success: boolean;
  exit_code: number | null;
  stdout: string;
  stderr: string;
}

/**
 * Per-language LSP server installation status returned by
 * `GET /api/lsp/status`. The backend probes `PATH` for each candidate
 * command at request time using the same logic the LSP WebSocket handler
 * uses, so the value here matches the editor's actual ability to launch
 * the server.
 */
export interface LspServerStatusEntry {
  /** Canonical language name (e.g. "rust", "python") */
  language: string;
  /** Whether any candidate command was found on PATH */
  installed: boolean;
  /** Resolved command path or name; only present when `installed` is true */
  command?: string;
}

/** LSP server health status (frontend-only) */
export type LspHealthStatus = "unknown" | "checking" | "installed" | "not_installed" | "error";

/** LSP server with resolved status for UI display */
export interface LspServerWithStatus {
  /** Canonical language name (e.g. "rust", "python") */
  language: string;
  /** Server entry from config */
  entry: LspServerEntry;
  /** Whether the command is found on PATH */
  installed: boolean;
  /** Resolved command path (if installed) */
  command?: string;
  /** Health status */
  status: LspHealthStatus;
  /** Error message if status is "error" */
  error?: string;
}

/** MCP preset definition (frontend-only, not persisted) */
export interface McpPresetDef {
  id: string;
  name: string;
  description: string;
  category: McpPresetCategory;
  transport: McpTransportDef;
  /** For stdio: executable command */
  command?: string;
  /** For stdio: command arguments */
  args?: string[];
  /** For http/sse: server URL */
  url?: string;
  /** Required env vars (user must provide, e.g. API keys) */
  requiredEnv: string[];
  /** Optional env vars with defaults */
  optionalEnv: Record<string, string>;
  /** Install hint / instructions */
  installHint?: string;
  /** Icon name from lucide-react */
  icon?: string;
}

// ── Embedding Model types ─────────────────────────────────────────────────

/** Embedding model entry with status — matches GET /api/embedding-models */
export interface EmbeddingModelWithStatus {
  id: string;
  name: string;
  description?: string;
  dimension: number;
  max_tokens: number;
  size_mb: number;
  languages: string[];
  pooling_strategy: string;
  recommended: boolean;
  loaded: boolean;
  status: string;
  /** Available ONNX variants (e.g., {"fp32": "onnx/model.onnx", "fp16": "onnx/model_fp16.onnx"}) */
  onnx_variants?: Record<string, string>;
}

/** Response for GET /api/embedding-models */
export interface EmbeddingModelsResponse {
  models: EmbeddingModelWithStatus[];
  active_model_id: string | null;
  service_running: boolean;
}

/** Response for download/select actions */
export interface EmbeddingModelActionResponse {
  model_id: string;
  status: string;
  message: string;
}

/** Response from GET /api/embedding-models/{id}/status */
export interface EmbeddingModelStatusResponse {
  model_id: string;
  /** Always a string: "not_downloaded", "downloading", "downloaded", "loaded", "failed" */
  status: string;
  /** Download progress percentage (0-100). Only present when status is "downloading" */
  progress?: number;
  /** Error message. Only present when status is "failed" */
  error?: string;
  info?: { id: string; dimension: number; pooling: string } | null;
}

/** Response from POST /api/embedding-models/test */
export interface EmbeddingTestResponse {
  success: boolean;
  model_id?: string | null;
  dimension?: number | null;
  latency_ms?: number | null;
  error?: string | null;
}

/** Response from POST /api/embedding-providers/{id}/test (cloud providers).
 *
 *  Distinct from `EmbeddingTestResponse` because the cloud endpoint uses
 *  `{ ok, message }` rather than `{ success, error }` — kept the cloud
 *  field names verbatim so the gateway response and the frontend
 *  interface never drift out of sync via type aliasing.
 *
 *  Callers that need the unified `EmbeddingTestResponse` shape (e.g. the
 *  per-card UI) should map `ok → success`, `message → error` explicitly. */
export interface CloudEmbeddingTestResponse {
  provider_id: string;
  model_id: string;
  ok: boolean;
  dimension?: number | null;
  /** Human-readable status — "OK — returned N dims as expected." on success,
   *  or the upstream error message on failure (e.g. "HTTP 401: …"). */
  message?: string | null;
}

// ── Cloud Embedding Providers (S1-7) ────────────────────────────────────

/** A single cloud embedding model within a provider (e.g. volcengine.doubao-embedding). */
export interface CloudEmbeddingModel {
  id: string;
  name: string;
  dimensions: number;
  context_length?: number | null;
  embedding_modalities?: string[];
}

/** A cloud embedding provider (volcengine / dashscope / siliconflow / …). */
export interface CloudEmbeddingProvider {
  id: string;
  name: string;
  api: string;
  protocol: string;
  env: string[];
  doc?: string | null;
  models: Record<string, CloudEmbeddingModel>;
  /** Whether an API key is currently stored in Vault (mirrors backend
   * `EmbeddingProviderView.has_api_key`). Use this — not the active
   * selection's `has_api_key` — to drive per-card UI, because the
   * active selection only tracks the *currently selected* provider's
   * key state, while every card needs to know its own. */
  has_api_key?: boolean;
  /** Masked preview of the stored API key, when one is configured.
   * Backend returns this only when `has_api_key === true`. */
  key_preview?: string | null;
  /** True for user-added providers (persisted in
   * `user_embedding_providers.json`). Bundled providers omit this or
   * send `false`. */
  custom?: boolean;
}

/** Active cloud embedding selection — mirrors data_dir/active_embedding_provider.json. */
export interface ActiveCloudEmbeddingProvider {
  provider_id: string;
  model_id: string;
  dimension: number;
  base_url: string;
  has_api_key: boolean;
  selected_at: string;
}

/** Response for GET /api/embedding-providers */
export interface CloudEmbeddingProvidersResponse {
  providers: CloudEmbeddingProvider[];
  active: ActiveCloudEmbeddingProvider | null;
}

/** Response for POST /api/embedding-providers/{id}/select */
export interface SelectCloudEmbeddingResponse {
  provider_id: string;
  model_id: string;
  dimension: number;
  base_url: string;
  has_api_key: boolean;
  status: string;
  message: string;
}

/** Request body for POST /api/embedding-providers — add a user-defined provider. */
export interface AddCloudEmbeddingProviderRequest {
  id: string;
  name: string;
  api: string;
  models: Record<string, CloudEmbeddingModel>;
  /** Optional API key stored in Vault alongside the provider entry. */
  api_key?: string;
}

/** Request body for PUT /api/embedding-providers/{id} — update a user-defined provider. */
export interface UpdateCloudEmbeddingProviderRequest {
  name?: string;
  api?: string;
  /** When provided, REPLACES the entire model set. */
  models?: Record<string, CloudEmbeddingModel>;
}

/** Response for add / update / delete embedding provider. */
export interface CloudEmbeddingProviderResponse {
  id: string;
  name: string;
  api: string;
  custom: boolean;
  models: string[];
  message: string;
}

// ── Migration types ───────────────────────────────────────────────────────

/** Migration progress for a single agent — matches GET /api/embedding-models/migration-progress */
export interface AgentMigrationProgress {
  agent_id: string;
  request_id: string;
  target_model_id: string;
  target_dimension: number;
  progress?: {
    rebuilt: number;
    total_scanned: number;
    errors: number;
    phase: string;
    label: string;
  } | null;
  done: boolean;
  error?: string | null;
}

/** Response from GET /api/embedding-models/migration-progress */
export interface MigrationProgressResponse {
  agents: AgentMigrationProgress[];
}

/** Agent entry returned in migration-required response */
export interface MigrationAgentEntry {
  agent_id: string;
  name: string;
  is_running: boolean;
  has_active_sessions: boolean;
  migration_status?: string | null;
}

/** Response from select_model when migration is required */
export interface SelectModelMigrationResponse {
  model_id: string;
  status: string;
  message: string;
  new_dimension: number;
  old_dimension?: number | null;
  agents: MigrationAgentEntry[];
}

// ── Avatar config types (ADR-017) ─────────────────────────────────────

/** Effective avatar configuration from GET /api/agents/:id/avatar-config */
export interface AvatarConfigResponse {
  agent_id: string;
  /** Effective custom avatar path (relative to install dir). Null when none. */
  avatar: string | null;
  /** Effective builtin avatar icon ID (e.g. "icon-05"). Null when none. */
  builtin_avatar: string | null;
  /** Source of the effective value: "runtime" | "config" | "manifest" | "fallback" */
  source: "runtime" | "config" | "manifest" | "fallback";
}

/** PUT request body for PUT /api/agents/:id/avatar-config */
export interface UpdateAvatarConfigRequest {
  /** Set to a path to select, "" to clear, omit to leave unchanged */
  avatar?: string;
  /** Set to an icon ID to select, "" to clear, omit to leave unchanged */
  builtin_avatar?: string;
}

/** A single avatar asset file in the install directory */
export interface AvatarAssetEntry {
  relative_path: string;
}

/** Response from GET /api/agents/:id/manifest/avatar-assets */
export interface AvatarAssetsResponse {
  agent_id: string;
  assets: AvatarAssetEntry[];
}

/**
 * ADR-059 §5.1 — Gateway bootstrap snapshot (`GET /api/bootstrap` and
 * the retained `acowork/global/bootstrap` MQTT topic).
 *
 * `phase` is SCREAMING_SNAKE_CASE (`BOOTING` / `READY` / `DEGRADED` /
 * `FAILED` / `SHUTTING_DOWN`). `version` is the resource version
 * clients echo back via `expected_version` on mutation APIs.
 */
export interface BootstrapStateView {
  protocol_version: number;
  /** Fresh per Gateway process — changes on every restart. */
  instance_id: string;
  /** Resource version, increments on every readiness transition. */
  version: number;
  phase: string;
  /** Human-readable subsystem-level diagnostic (e.g. "3/5 required ready"). */
  phase_detail: string;
  issued_at_ms: number;
}

/** ADR-059 §6 — install operation ack (`POST /api/agents/install`, HTTP 202). */
export interface OperationAck {
  operation_id: string;
  /** snake_case: accepted / committed / running / completed / failed */
  state: string;
  resource_version?: number;
  terminal_error?: {
    code: string;
    [key: string]: unknown;
  };
}

