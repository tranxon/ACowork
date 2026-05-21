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
  version: string;
  running: boolean;
  connected: boolean;
  dev_mode: boolean;
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
}

/** Agent list entry — matches Gateway API */
export interface AgentInfo {
  agent_id: string;
  name: string;
  display_name?: string;
  role?: string;
  avatar?: string;
  version: string;
  running: boolean;
  connected: boolean;
  ready: boolean;
  dev_mode: boolean;
  debug_port?: number;
}

/** Agent detail response */
export interface AgentDetail {
  agent_id: string;
  name: string;
  display_name?: string;
  role?: string;
  avatar?: string;
  version: string;
  description: string;
  author: string;
  install_path: string;
  running: boolean;
  connected: boolean;
  ready: boolean;
  pid: number | null;
  started_at: string | null;
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
  /** Model capabilities (from models.dev or user input) */
  model_capabilities?: ModelCapabilitiesInfo;
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

/** A single check item from publish prepare */
export interface CheckItem {
  field: string;
  status: string;
  message?: string;
}

/** Publish prepare response */
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

/** Gateway connection status */
export type GatewayStatus = "connected" | "disconnected" | "error";

/** Chat message types */
export type MessageType = "user" | "assistant" | "system" | "tool_call" | "tool_result" | "thought" | "document_upload";

/** Chat message in the UI */
export interface ChatMessage {
  id: string;
  type: MessageType;
  content: string;
  timestamp: number;
  /** Sender display name for chat bubble (e.g. "PM", "我") */
  senderDisplayName?: string;
  /** Sender avatar URL or data URI */
  senderAvatar?: string;
  /** Sender role label (e.g. "Project Manager") */
  senderRole?: string;
  /** For tool_call: tool name */
  toolName?: string;
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
  /** For document_upload: document ID */
  documentId?: string;
  /** For document_upload: document format (pdf, docx, pptx, xlsx) */
  documentFormat?: string;
  /** For document_upload: file size in bytes */
  documentSize?: number;
  /** For document_upload: absolute path */
  documentPath?: string;
}

/** Token usage stats */
export interface TokenUsage {
  prompt_tokens: number;
  completion_tokens: number;
  total_tokens: number;
}

/** Context usage info reported by Runtime, forwarded via Gateway WebSocket */
export interface ContextUsageInfo {
  /** Context window limit (from model capabilities) */
  context_window: number;
  /** Current input tokens used (prompt_tokens from API response) */
  input_tokens: number;
  /** Current output tokens generated (completion_tokens) */
  output_tokens: number;
  /** Total tokens (input + output) */
  total_tokens: number;
  /** Max input tokens (from models.dev limit.input, if available) */
  max_input_tokens?: number;
  /** Usable context space */
  usable_context: number;
  /** Usage percentage (0-100) */
  usage_percent: number;
}

/** Navigation view type */
export type NavView = "chat" | "settings";

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

/** Provider list entry from Gateway API */
export interface ProviderListEntry {
  id: string;
  name: string;
  model_count: number;
  /** Provider's base API URL (from models.dev or offline data) */
  api?: string;
}

// ── Memory types ──────────────────────────────────────────────────────

/** Single memory node in the list response */
export interface MemoryNodeResponse {
  node_id: number;
  node_type: string;
  content: string;
  confidence: number;
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
}

/** ADR-014: Session lifecycle status — read-only from backend */
export type SessionStatus =
  | { status: "idle" }
  | { status: "streaming"; detail?: { message_id: string | null } }
  | { status: "waiting_approval"; detail: { request_id: string } }
  | { status: "paused"; detail?: { iteration: number | null; max_iterations: number | null } };

/** Helper: check if a SessionStatus means the session is actively processing (includes paused) */
export function isSessionActive(s: SessionStatus | undefined | null): boolean {
  if (!s) return false;
  return s.status === "streaming" || s.status === "waiting_approval" || s.status === "paused";
}

/** Helper: get message_id from Streaming status, or null */
export function getStreamingMessageId(s: SessionStatus | undefined | null): string | null {
  if (!s || s.status !== "streaming") return null;
  return s.detail?.message_id ?? null;
}

/** A single conversation entry as stored in JSONL */
export interface ConversationEntry {
  id: string;
  ts: string;
  role: "user" | "assistant" | "think" | "thought" | "tool_call" | "tool_result" | "system";
  content: string;
  metadata?: Record<string, unknown>;
}

/** Metadata for document upload entries in conversation history */
export interface DocumentUploadMeta {
  type: "document_upload";
  document_id: string;
  filename: string;
  format: "pdf" | "docx" | "pptx" | "xlsx";
  size_bytes: number;
  path: string;
}

/** Paginated messages response from Gateway */
export interface PaginatedMessages {
  session_id: string;
  messages: ConversationEntry[];
  cursor: string | null;
  has_more: boolean;
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
}
