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
export type NavView = "chat" | "models" | "skills" | "settings";

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
}
