//! manifest.toml data structures for .agent packages
//!
//! The AgentManifest is the core declaration of every .agent package.
//! It defines the agent's identity, capabilities, permissions, and
//! configuration in a declarative TOML format.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::permission::Permission;

/// Skill injection mode for system prompt.
///
/// Controls how skill definitions are injected into the Agent's system prompt.
/// This is an internal runtime representation derived from `[skills]` config.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SkillMode {
    /// Do not inject any skill content into system prompt.
    /// Skills are loaded on-demand via command or trigger.
    #[default]
    Manual,
    /// Inject a compact summary list (name + description) of available skills.
    /// Full instructions are loaded on-demand when a skill is activated.
    Progressive,
}

/// Skill configuration section in manifest.toml
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SkillsConfig {
    /// Whether to use progressive skill injection mode.
    /// When `true`, a compact summary (name + description) of available skills
    /// is injected into the system prompt. When `false` (default), no skill
    /// content is injected.
    #[serde(default)]
    pub progressive: bool,
}

impl SkillsConfig {
    /// Convert the boolean flag to the internal `SkillMode` enum.
    pub fn mode(&self) -> SkillMode {
        if self.progressive {
            SkillMode::Progressive
        } else {
            SkillMode::Manual
        }
    }
}

/// Complete manifest.toml structure for .agent packages
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentManifest {
    /// Reverse-domain identifier (e.g., "com.example.weather")
    pub agent_id: String,
    /// Semantic version (e.g., "1.0.0")
    pub version: String,
    /// Human-readable name
    pub name: String,
    /// Short display name for chat UI (defaults to `name` if absent)
    #[serde(default)]
    pub display_name: Option<String>,
    /// Agent role / job title (e.g. "Project Manager")
    #[serde(default)]
    pub role: Option<String>,
    /// Path to avatar image within the .agent package (e.g. "assets/avatar.png")
    #[serde(default)]
    pub avatar: Option<String>,
    /// Short description
    pub description: String,
    /// Author identifier
    pub author: String,
    /// Minimum runtime version required
    pub runtime_version: String,
    /// Declared permissions
    #[serde(default)]
    pub permissions: Vec<Permission>,
    /// Activation triggers (cron, event, etc.)
    #[serde(default)]
    pub triggers: Vec<Trigger>,
    /// LLM provider configuration
    pub llm: LlmConfig,
    /// Memory system configuration
    #[serde(default)]
    pub memory: MemoryConfig,
    /// Dependencies on System Agent identity fields
    #[serde(default)]
    pub identity_deps: Vec<String>,
    /// Tool declarations
    #[serde(default)]
    pub tools: Vec<ToolDeclaration>,
    /// Capability advertisements for Intent routing
    #[serde(default)]
    pub capabilities: HashMap<String, CapabilityDef>,
    /// Resource limits
    #[serde(default)]
    pub resources: ResourceLimits,
    /// Sandbox configuration
    #[serde(default)]
    pub sandbox: SandboxConfig,
    /// Whether this is a system agent
    #[serde(default)]
    pub system: bool,
    /// Whether developer mode is enabled
    #[serde(default)]
    pub dev: bool,
    /// Skill configuration
    #[serde(default)]
    pub skills: SkillsConfig,
}

impl AgentManifest {
    /// Returns the effective skill mode for this agent.
    pub fn skill_mode(&self) -> SkillMode {
        self.skills.mode()
    }

    /// Parse manifest from TOML string
    pub fn from_toml(toml_str: &str) -> Result<Self, ManifestError> {
        let manifest: Self = toml::from_str(toml_str)?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Serialize manifest to TOML string
    pub fn to_toml(&self) -> Result<String, ManifestError> {
        let toml_str = toml::to_string_pretty(self)?;
        Ok(toml_str)
    }

    /// Validate manifest fields
    pub fn validate(&self) -> Result<(), ManifestError> {
        // agent_id must follow reverse-domain format
        if self.agent_id.is_empty() {
            return Err(ManifestError::Validation("agent_id cannot be empty".into()));
        }
        if !self.agent_id.contains('.') {
            return Err(ManifestError::Validation(
                "agent_id must follow reverse-domain format (e.g., com.example.agent)".into(),
            ));
        }

        // version must be valid semver-like
        if self.version.is_empty() {
            return Err(ManifestError::Validation("version cannot be empty".into()));
        }

        // name cannot be empty
        if self.name.is_empty() {
            return Err(ManifestError::Validation("name cannot be empty".into()));
        }

        // runtime_version cannot be empty
        if self.runtime_version.is_empty() {
            return Err(ManifestError::Validation(
                "runtime_version cannot be empty".into(),
            ));
        }

        // llm.suggested_provider must be specified
        if self.llm.suggested_provider.is_empty() {
            return Err(ManifestError::Validation(
                "llm.suggested_provider cannot be empty".into(),
            ));
        }

        // llm.suggested_model must be specified
        if self.llm.suggested_model.is_empty() {
            return Err(ManifestError::Validation(
                "llm.suggested_model cannot be empty".into(),
            ));
        }

        Ok(())
    }

    /// Check if a specific tool is declared in the manifest
    pub fn has_tool(&self, tool_name: &str) -> bool {
        self.tools.iter().any(|t| t.name == tool_name)
    }

    /// Get tool declaration by name
    pub fn get_tool(&self, tool_name: &str) -> Option<&ToolDeclaration> {
        self.tools.iter().find(|t| t.name == tool_name)
    }

    /// Get all cron triggers from the manifest
    pub fn cron_triggers(&self) -> Vec<&Trigger> {
        self.triggers
            .iter()
            .filter(|t| t.trigger_type == "cron")
            .collect()
    }

    /// Get the first RAG tool configuration, if any.
    ///
    /// Returns `Some((tool_name, &RagToolConfig))` if a `[[tools]]` entry
    /// with `type = "rag"` is declared, otherwise `None`.
    pub fn rag_config(&self) -> Option<(&str, &RagToolConfig)> {
        self.tools.iter().find(|t| t.is_rag()).and_then(|t| {
            t.rag.as_ref().map(|rag| (t.name.as_str(), rag))
        })
    }

    /// Check if this manifest declares any RAG tool
    pub fn has_rag(&self) -> bool {
        self.tools.iter().any(|t| t.is_rag())
    }
}

/// LLM provider configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    /// Suggested provider name (e.g., "openai", "ollama", "anthropic").
    /// This is a recommendation from the Agent developer; the actual provider
    /// used at runtime is determined by the Gateway's LLMConfigDelivery,
    /// which overrides this value when the user has configured a default provider.
    /// Backward-compatible: deserializes from both `suggested_provider` and `provider`.
    #[serde(default, alias = "provider")]
    pub suggested_provider: String,

    /// Suggested model identifier (e.g., "gpt-4", "llama3").
    /// Same override semantics as `suggested_provider`.
    /// Backward-compatible: deserializes from both `suggested_model` and `model`.
    #[serde(default, alias = "model")]
    pub suggested_model: String,

    /// Sampling temperature (0.0 - 2.0)
    #[serde(default)]
    pub temperature: Option<f64>,
    /// Maximum tokens in response
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Fallback providers in priority order
    #[serde(default)]
    pub fallback_providers: Vec<String>,
    /// Multiple provider configurations (key = provider name)
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    /// Routing configuration
    #[serde(default)]
    pub routing: Option<RoutingConfig>,
    /// Budget configuration for LLM usage
    #[serde(default)]
    pub budget: Option<LlmBudget>,
}

/// Configuration for a specific LLM provider
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Model identifier for this provider
    pub model: String,
    /// API key reference (e.g., "vault:openai_key")
    #[serde(default)]
    pub api_key_ref: Option<String>,
    /// Custom base URL
    #[serde(default)]
    pub base_url: Option<String>,
    /// Provider-specific parameters
    #[serde(default)]
    pub params: Option<HashMap<String, serde_json::Value>>,
}

/// Routing strategy configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingConfig {
    /// Routing strategy: cost_priority / quality_priority / latency_priority
    #[serde(default = "default_routing_strategy")]
    pub strategy: String,
    /// Fallback provider order (overrides fallback_providers)
    #[serde(default)]
    pub fallback_order: Vec<String>,
    /// Enable automatic provider switching on failure
    #[serde(default = "default_true")]
    pub auto_switch: bool,
}

fn default_routing_strategy() -> String {
    "quality_priority".to_string()
}

fn default_true() -> bool {
    true
}

/// LLM budget configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmBudget {
    /// Maximum tokens per request
    #[serde(default)]
    pub max_tokens_per_request: Option<u64>,
    /// Maximum cost per request in USD
    #[serde(default)]
    pub max_cost_per_request_usd: Option<f64>,
    /// Maximum output tokens
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    /// Action when budget exceeded: stop / fallback_to_local / warn
    #[serde(default = "default_budget_action")]
    pub exceeded_action: String,
}

fn default_budget_action() -> String {
    "stop".to_string()
}

/// Memory system configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    /// Whether memory is enabled for this agent
    #[serde(default = "default_memory_enabled")]
    pub enabled: bool,
    /// Retention period in days
    #[serde(default)]
    pub retention_days: Option<u32>,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: default_memory_enabled(),
            retention_days: None,
        }
    }
}

fn default_memory_enabled() -> bool {
    true
}

/// Tool declaration in manifest
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDeclaration {
    /// Tool type (default: "builtin"). Use "rag" for RAG tools.
    #[serde(rename = "type", default = "default_tool_type")]
    pub tool_type: String,
    /// Tool name (must match a registered tool)
    pub name: String,
    /// Optional tool-specific configuration
    #[serde(default)]
    pub config: Option<serde_json::Value>,
    /// RAG configuration (only when tool_type = "rag")
    #[serde(default)]
    pub rag: Option<RagToolConfig>,
}

fn default_tool_type() -> String {
    "builtin".to_string()
}

impl ToolDeclaration {
    /// Check if this is a RAG tool declaration
    pub fn is_rag(&self) -> bool {
        self.tool_type == "rag"
    }
}

/// RAG tool configuration (manifest `[[tools]]` with type = "rag")
///
/// Declares an enterprise RAG knowledge base endpoint for the agent.
/// RAG is an opt-in capability — agents without this declaration behave
/// exactly as in Phase 3, zero intrusion.
///
/// Example manifest:
/// ```toml
/// [[tools]]
/// type = "rag"
/// name = "enterprise_knowledge"
///
/// [tools.rag]
/// endpoint = "https://rag.corp.example.com/v1/query"
/// collection = "product_docs"
/// auth_ref = "vault:rag_enterprise_key"
/// auth_type = "bearer"
/// max_results = 5
/// score_threshold = 0.7
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RagToolConfig {
    /// RAG service endpoint URL (required)
    pub endpoint: String,
    /// Collection / index name in the RAG service (optional)
    #[serde(default)]
    pub collection: Option<String>,
    /// Vault reference for authentication credential (e.g., "vault:rag_enterprise_key")
    #[serde(default)]
    pub auth_ref: Option<String>,
    /// Authentication type: "bearer" or "api_key" (default: "bearer")
    #[serde(default = "default_auth_type")]
    pub auth_type: String,
    /// Maximum number of results per query (default: 5)
    #[serde(default = "default_max_results")]
    pub max_results: u32,
    /// Minimum score threshold for results (default: 0.7)
    #[serde(default = "default_score_threshold")]
    pub score_threshold: f32,
    /// Query timeout in seconds (default: 10)
    #[serde(default = "default_rag_timeout")]
    pub timeout_secs: u64,
}

fn default_auth_type() -> String {
    "bearer".to_string()
}

fn default_max_results() -> u32 {
    5
}

fn default_score_threshold() -> f32 {
    0.7
}

fn default_rag_timeout() -> u64 {
    10
}

/// Capability definition for Intent routing
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityDef {
    /// Human-readable description of this capability
    pub description: String,
    /// JSON Schema for capability input
    #[serde(default)]
    pub input_schema: Option<serde_json::Value>,
    /// JSON Schema for capability output
    #[serde(default)]
    pub output_schema: Option<serde_json::Value>,
}

/// Resource limits for Agent process
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ResourceLimits {
    /// Maximum memory usage in MB
    #[serde(default)]
    pub max_memory_mb: Option<u64>,
    /// Maximum CPU percentage (0.0 - 100.0)
    #[serde(default)]
    pub max_cpu_percent: Option<f64>,
    /// Idle timeout in seconds before auto-stop
    #[serde(default)]
    pub idle_timeout_secs: Option<u64>,
}

/// Sandbox configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SandboxConfig {
    /// Whether sandboxing is enabled
    #[serde(default)]
    pub enabled: bool,
    /// Working directory for the agent
    #[serde(default)]
    pub work_dir: Option<String>,
    /// Paths the agent is allowed to access
    #[serde(default)]
    pub allowed_paths: Vec<String>,
}

/// Trigger definition (cron, event-based, etc.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trigger {
    /// Trigger type (e.g., "cron", "event", "manual")
    #[serde(rename = "type")]
    pub trigger_type: String,
    /// Cron schedule expression (for cron triggers)
    #[serde(default)]
    pub schedule: Option<String>,
    /// Action to fire when the trigger activates (for cron/event triggers)
    #[serde(default)]
    pub action: Option<String>,
    /// Params to include when the trigger fires (JSON, for cron/event triggers)
    #[serde(default)]
    pub params: Option<serde_json::Value>,
    /// Event name (for event triggers)
    #[serde(default)]
    pub event: Option<String>,
}

/// Manifest parsing/validation errors
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("TOML parse error: {0}")]
    TomlParse(#[from] toml::de::Error),
    #[error("TOML serialize error: {0}")]
    TomlSerialize(#[from] toml::ser::Error),
    #[error("Validation error: {0}")]
    Validation(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest_toml() -> &'static str {
        r#"
            agent_id = "com.example.weather"
            version = "1.0.0"
            name = "Weather Agent"
            description = "A weather query agent"
            author = "rollball"
            runtime_version = "0.1.0"

            [llm]
            provider = "openai"
            model = "gpt-4"
            temperature = 0.7
            max_tokens = 4096

            [memory]
            enabled = true
            retention_days = 90

            [resources]
            max_memory_mb = 512
            idle_timeout_secs = 300

            [sandbox]
            enabled = true
            work_dir = "/tmp/agent-workdir"

            [[permissions]]
            type = "Network"
            value = "wttr.in"

            [[permissions]]
            type = "MemoryRead"

            [[tools]]
            name = "weather"

            [[tools]]
            name = "memory_store"

            [[triggers]]
            type = "manual"
        "#
    }

    #[test]
    fn test_manifest_parse_minimal() {
        let toml_str = r#"
            agent_id = "com.example.weather"
            version = "1.0.0"
            name = "Weather Agent"
            description = "A weather query agent"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "openai"
            model = "gpt-4"
        "#;
        let manifest = AgentManifest::from_toml(toml_str).unwrap();
        assert_eq!(manifest.agent_id, "com.example.weather");
        assert_eq!(manifest.llm.suggested_provider, "openai");
        assert_eq!(manifest.llm.suggested_model, "gpt-4");
        assert!(manifest.permissions.is_empty());
        assert!(manifest.tools.is_empty());
        assert!(manifest.memory.enabled); // default true
    }

    #[test]
    fn test_manifest_parse_full() {
        let manifest = AgentManifest::from_toml(sample_manifest_toml()).unwrap();
        assert_eq!(manifest.agent_id, "com.example.weather");
        assert_eq!(manifest.version, "1.0.0");
        assert_eq!(manifest.llm.temperature, Some(0.7));
        assert_eq!(manifest.llm.max_tokens, Some(4096));
        assert!(manifest.memory.enabled);
        assert_eq!(manifest.memory.retention_days, Some(90));
        assert_eq!(manifest.resources.max_memory_mb, Some(512));
        assert!(manifest.sandbox.enabled);
    }

    #[test]
    fn test_manifest_roundtrip() {
        let original = AgentManifest::from_toml(sample_manifest_toml()).unwrap();
        let toml_str = original.to_toml().unwrap();
        let parsed = AgentManifest::from_toml(&toml_str).unwrap();
        assert_eq!(original.agent_id, parsed.agent_id);
        assert_eq!(original.version, parsed.version);
        assert_eq!(original.llm.suggested_provider, parsed.llm.suggested_provider);
        assert_eq!(original.llm.suggested_model, parsed.llm.suggested_model);
    }

    #[test]
    fn test_manifest_validation_empty_agent_id() {
        let toml_str = r#"
            agent_id = ""
            version = "1.0.0"
            name = "Test"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "openai"
            model = "gpt-4"
        "#;
        let result = AgentManifest::from_toml(toml_str);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("agent_id"), "Expected agent_id error, got: {err}");
    }

    #[test]
    fn test_manifest_validation_no_dots_in_id() {
        let toml_str = r#"
            agent_id = "weather"
            version = "1.0.0"
            name = "Test"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "openai"
            model = "gpt-4"
        "#;
        let result = AgentManifest::from_toml(toml_str);
        assert!(result.is_err());
    }

    #[test]
    fn test_manifest_has_tool() {
        let manifest = AgentManifest::from_toml(sample_manifest_toml()).unwrap();
        assert!(manifest.has_tool("weather"));
        assert!(manifest.has_tool("memory_store"));
        assert!(!manifest.has_tool("shell"));
    }

    #[test]
    fn test_manifest_get_tool() {
        let manifest = AgentManifest::from_toml(sample_manifest_toml()).unwrap();
        let tool = manifest.get_tool("weather").unwrap();
        assert_eq!(tool.name, "weather");
        assert!(manifest.get_tool("nonexistent").is_none());
    }

    #[test]
    fn test_manifest_rag_tool_declaration() {
        let toml_str = r#"
            agent_id = "com.example.sales"
            version = "1.0.0"
            name = "Sales Assistant"
            description = "Enterprise sales agent with RAG"
            author = "corp"
            runtime_version = "0.1.0"

            [llm]
            provider = "openai"
            model = "gpt-4"

            [[permissions]]
            type = "Network"
            value = "https://rag.corp.example.com"

            [[tools]]
            type = "rag"
            name = "enterprise_knowledge"

            [tools.rag]
            endpoint = "https://rag.corp.example.com/v1/query"
            collection = "product_docs"
            auth_ref = "vault:rag_enterprise_key"
            auth_type = "bearer"
            max_results = 5
            score_threshold = 0.7
        "#;
        let manifest = AgentManifest::from_toml(toml_str).unwrap();
        assert!(manifest.has_rag());
        let (tool_name, rag_config) = manifest.rag_config().unwrap();
        assert_eq!(tool_name, "enterprise_knowledge");
        assert_eq!(rag_config.endpoint, "https://rag.corp.example.com/v1/query");
        assert_eq!(rag_config.collection.as_deref(), Some("product_docs"));
        assert_eq!(rag_config.auth_ref.as_deref(), Some("vault:rag_enterprise_key"));
        assert_eq!(rag_config.auth_type, "bearer");
        assert_eq!(rag_config.max_results, 5);
        assert!((rag_config.score_threshold - 0.7).abs() < 0.001);
    }

    #[test]
    fn test_manifest_no_rag_by_default() {
        let manifest = AgentManifest::from_toml(sample_manifest_toml()).unwrap();
        assert!(!manifest.has_rag());
        assert!(manifest.rag_config().is_none());
    }

    #[test]
    fn test_manifest_tool_declaration_default_type() {
        let manifest = AgentManifest::from_toml(sample_manifest_toml()).unwrap();
        let tool = manifest.get_tool("weather").unwrap();
        assert_eq!(tool.tool_type, "builtin");
        assert!(!tool.is_rag());
    }
}
