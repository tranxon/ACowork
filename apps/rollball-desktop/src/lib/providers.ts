//! Provider registry — single source of truth for all supported LLM providers.
//!
//! Every UI component that displays or manages providers should import from here.
//! Adding a new provider only requires updating this file.

/** Authentication style for the provider */
export type AuthStyle = "bearer" | "x-api-key" | "zhipu-jwt";

/** Provider definition */
export interface ProviderDef {
  /** Canonical ID used as Vault key (e.g. "openai", "deepseek") */
  id: string;
  /** Display name */
  name: string;
  /** Provider category */
  category: "international" | "china" | "local" | "cloud";
  /** Default base URL (empty = user must provide) */
  baseUrl: string;
  /** Whether base URL is editable by user */
  editableBaseUrl: boolean;
  /** Auth style */
  authStyle: AuthStyle;
  /** Placeholder text for API key input */
  keyPlaceholder: string;
  /** Example models this provider offers.
   * Used as fallback when dynamic models.dev fetch fails.
   * NOT actively maintained — always prefer dynamic list from Gateway API. */
  exampleModels: string[];
  /** Whether this provider needs an API key (Ollama doesn't) */
  needsApiKey: boolean;
  /** Aliases that map to this provider */
  aliases: string[];
  /** Optional description shown in UI */
  description?: string;
}

// ── International providers ──────────────────────────────────────────

const OPENAI: ProviderDef = {
  id: "openai",
  name: "OpenAI",
  category: "international",
  baseUrl: "https://api.openai.com/v1",
  editableBaseUrl: true,
  authStyle: "bearer",
  keyPlaceholder: "sk-...",
  exampleModels: ["gpt-4o", "gpt-4o-mini", "o3-mini", "o4-mini"],
  needsApiKey: true,
  aliases: [],
};

const ANTHROPIC: ProviderDef = {
  id: "anthropic",
  name: "Anthropic",
  category: "international",
  baseUrl: "https://api.anthropic.com",
  editableBaseUrl: false,
  authStyle: "x-api-key",
  keyPlaceholder: "sk-ant-...",
  exampleModels: ["claude-sonnet-4-20250514", "claude-3-5-sonnet-20241022", "claude-3-5-haiku-20241022"],
  needsApiKey: true,
  aliases: [],
};

const GOOGLE: ProviderDef = {
  id: "google",
  name: "Google Gemini",
  category: "international",
  baseUrl: "https://generativelanguage.googleapis.com",
  editableBaseUrl: false,
  authStyle: "bearer",
  keyPlaceholder: "AIza...",
  exampleModels: ["gemini-2.5-pro", "gemini-2.5-flash", "gemini-2.0-flash"],
  needsApiKey: true,
  aliases: ["gemini"],
};

const GROQ: ProviderDef = {
  id: "groq",
  name: "Groq",
  category: "international",
  baseUrl: "https://api.groq.com/openai/v1",
  editableBaseUrl: false,
  authStyle: "bearer",
  keyPlaceholder: "gsk_...",
  exampleModels: ["llama-4-scout-17b-16e-instruct", "mixtral-8x7b-32768"],
  needsApiKey: true,
  aliases: [],
  description: "Ultra-fast inference",
};

const MISTRAL: ProviderDef = {
  id: "mistral",
  name: "Mistral",
  category: "international",
  baseUrl: "https://api.mistral.ai/v1",
  editableBaseUrl: false,
  authStyle: "bearer",
  keyPlaceholder: "API key...",
  exampleModels: ["mistral-large-latest", "codestral-latest"],
  needsApiKey: true,
  aliases: [],
};

const XAI: ProviderDef = {
  id: "xai",
  name: "xAI (Grok)",
  category: "international",
  baseUrl: "https://api.x.ai",
  editableBaseUrl: false,
  authStyle: "bearer",
  keyPlaceholder: "xai-...",
  exampleModels: ["grok-3", "grok-3-mini"],
  needsApiKey: true,
  aliases: ["grok"],
};

const OPENROUTER: ProviderDef = {
  id: "openrouter",
  name: "OpenRouter",
  category: "cloud",
  baseUrl: "https://openrouter.ai/api/v1",
  editableBaseUrl: false,
  authStyle: "bearer",
  keyPlaceholder: "sk-or-...",
  exampleModels: ["Multi-provider gateway"],
  needsApiKey: true,
  aliases: [],
  description: "Access 200+ models via one API",
};

const AZURE: ProviderDef = {
  id: "azure",
  name: "Azure OpenAI",
  category: "cloud",
  baseUrl: "",
  editableBaseUrl: true,
  authStyle: "bearer",
  keyPlaceholder: "Azure API key...",
  exampleModels: ["gpt-4o", "gpt-4o-mini"],
  needsApiKey: true,
  aliases: ["azure_openai"],
  description: "Microsoft Azure hosted OpenAI",
};

// ── China domestic providers ─────────────────────────────────────────

const DEEPSEEK: ProviderDef = {
  id: "deepseek",
  name: "DeepSeek",
  category: "china",
  baseUrl: "https://api.deepseek.com",
  editableBaseUrl: true,
  authStyle: "bearer",
  keyPlaceholder: "sk-...",
  exampleModels: ["deepseek-chat", "deepseek-reasoner", "deepseek-v3"],
  needsApiKey: true,
  aliases: [],
  description: "DeepSeek V3 / R1",
};

const ZHIPUAI: ProviderDef = {
  id: "zhipuai",
  name: "智谱AI (GLM)",
  category: "china",
  baseUrl: "https://open.bigmodel.cn/api/paas/v4",
  editableBaseUrl: true,
  authStyle: "zhipu-jwt",
  keyPlaceholder: "id.secret",
  exampleModels: ["glm-4-plus", "glm-4-flash", "glm-4v-plus"],
  needsApiKey: true,
  aliases: ["glm", "zhipu", "zhipuai"],
  description: "Key format: id.secret (JWT auto-generated)",
};

const MOONSHOTAI: ProviderDef = {
  id: "moonshotai",
  name: "Moonshot (Kimi)",
  category: "china",
  baseUrl: "https://api.moonshot.cn/v1",
  editableBaseUrl: true,
  authStyle: "bearer",
  keyPlaceholder: "sk-...",
  exampleModels: ["moonshot-v1-128k", "moonshot-v1-32k", "kimi-k2"],
  needsApiKey: true,
  aliases: ["moonshot", "kimi", "moonshotai"],
};

const ALIBABA: ProviderDef = {
  id: "alibaba",
  name: "阿里云 (Qwen)",
  category: "china",
  baseUrl: "https://dashscope.aliyuncs.com/compatible-mode/v1",
  editableBaseUrl: true,
  authStyle: "bearer",
  keyPlaceholder: "sk-...",
  exampleModels: ["qwen-max", "qwen-plus", "qwen-turbo", "qwen3-235b-a22b"],
  needsApiKey: true,
  aliases: ["qwen", "dashscope", "alibaba"],
  description: "Alibaba Cloud LLM",
};

const MINIMAX: ProviderDef = {
  id: "minimax",
  name: "MiniMax",
  category: "china",
  baseUrl: "https://api.minimax.chat/v1",
  editableBaseUrl: true,
  authStyle: "bearer",
  keyPlaceholder: "API key...",
  exampleModels: ["MiniMax-M2.5", "MiniMax-M2.1", "MiniMax-M2.7"],
  needsApiKey: true,
  aliases: [],
};

const DOUBAO: ProviderDef = {
  id: "doubao",
  name: "Doubao (豆包)",
  category: "china",
  baseUrl: "https://ark.cn-beijing.volces.com/api/v3",
  editableBaseUrl: true,
  authStyle: "bearer",
  keyPlaceholder: "API key...",
  exampleModels: ["doubao-1.5-pro", "doubao-1.5-lite"],
  needsApiKey: true,
  aliases: [],
  description: "ByteDance LLM",
};

// ── Local providers ──────────────────────────────────────────────────

const OLLAMA: ProviderDef = {
  id: "ollama",
  name: "Ollama (Local)",
  category: "local",
  baseUrl: "http://localhost:11434",
  editableBaseUrl: true,
  authStyle: "bearer",
  keyPlaceholder: "(no key needed)",
  exampleModels: ["qwen3:8b", "llama3:8b", "deepseek-r1:7b", "gemma3:4b"],
  needsApiKey: false,
  aliases: [],
  description: "Run models locally, no API key required",
};

const LMSTUDIO: ProviderDef = {
  id: "lmstudio",
  name: "LM Studio (Local)",
  category: "local",
  baseUrl: "http://localhost:1234/v1",
  editableBaseUrl: true,
  authStyle: "bearer",
  keyPlaceholder: "(no key needed)",
  exampleModels: ["Local models"],
  needsApiKey: false,
  aliases: ["lm-studio"],
};

// ── Registry ─────────────────────────────────────────────────────────

/** All supported providers, ordered by category */
export const ALL_PROVIDERS: ProviderDef[] = [
  // International
  OPENAI,
  ANTHROPIC,
  GOOGLE,
  GROQ,
  MISTRAL,
  XAI,
  // Cloud gateway
  OPENROUTER,
  AZURE,
  // China domestic
  DEEPSEEK,
  ZHIPUAI,
  MOONSHOTAI,
  ALIBABA,
  MINIMAX,
  DOUBAO,
  // Local
  OLLAMA,
  LMSTUDIO,
];

/** Provider categories for grouped display */
export const PROVIDER_CATEGORIES = [
  { id: "international", label: "International" },
  { id: "cloud", label: "Cloud Gateway" },
  { id: "china", label: "China Domestic" },
  { id: "local", label: "Local" },
] as const;

/** Lookup provider by ID or alias */
export function getProviderDef(id: string): ProviderDef | undefined {
  return ALL_PROVIDERS.find(
    (p) => p.id === id || p.aliases.includes(id),
  );
}

/** Resolve an alias to canonical provider ID */
export function canonicalProviderId(name: string): string {
  const def = getProviderDef(name);
  return def?.id ?? name;
}

/** Providers that require an API key */
export const KEYED_PROVIDERS = ALL_PROVIDERS.filter((p) => p.needsApiKey);

/** Providers that don't require an API key (local) */
export const LOCAL_PROVIDERS = ALL_PROVIDERS.filter((p) => !p.needsApiKey);
