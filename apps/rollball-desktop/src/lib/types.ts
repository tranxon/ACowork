/** Gateway health check response */
export interface HealthResponse {
  status: string;
  version: string;
  checks?: Record<string, { status: string; detail?: string }>;
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
  version: string;
  running: boolean;
}

/** Agent detail response */
export interface AgentDetail {
  agent_id: string;
  name: string;
  version: string;
  description: string;
  author: string;
  install_path: string;
  running: boolean;
  pid: number | null;
  started_at: string | null;
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
}

/** Generic message response */
export interface MessageResponse {
  message: string;
}

/** Send message response */
export interface SendMessageResponse {
  message_id: string;
  status: string;
}

/** Gateway connection status */
export type GatewayStatus = "connected" | "disconnected" | "error";

/** Chat message types */
export type MessageType = "user" | "assistant" | "system" | "tool_call" | "tool_result";

/** Chat message in the UI */
export interface ChatMessage {
  id: string;
  type: MessageType;
  content: string;
  timestamp: number;
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
}

/** Token usage stats */
export interface TokenUsage {
  prompt_tokens: number;
  completion_tokens: number;
  total_tokens: number;
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
  release_date?: string;
  /** Context window size (total tokens: input + output) */
  context_window?: number;
  /** Maximum output tokens */
  max_tokens?: number;
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
  shell_command?: {
    command: string;
    preview: string;
    risk_assessment: string;
  };
  params: Record<string, unknown>;
  params_summary: string;
  required_permission: string;
  timeout_ms: number;
}

/** Tool approval request payload */
export interface ToolApprovalResponse {
  request_id: string;
  action: "allow" | "deny" | "allow_all_session";
}

/** Approval API response */
export interface ApprovalApiResponse {
  request_id: string;
  action: string;
  status: string;
}
