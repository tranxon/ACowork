//! Model capability configuration
//!
//! IMPORTANT: No LLM provider (OpenAI, Anthropic, MiniMax, etc.) offers a standard API
//! to query model capabilities like context window and max_tokens.
//! 
//! This module implements a hybrid approach:
//! 1. Remote: Can fetch from models.dev API (community-maintained, updated hourly)
//! 2. Local: Fallback to built-in configuration for offline scenarios  
//! 3. Override: manifest.toml settings have highest priority
//!
//! Models.dev API: https://github.com/anomalyco/models.dev
//! API Endpoint: https://models.dev/api.json
//! 
//! Note: Remote fetching is disabled by default to avoid blocking startup.
//! Enable by calling ModelRegistry::with_remote_fetch() if needed.

use std::collections::HashMap;

/// Known model capabilities
#[derive(Debug, Clone)]
pub struct ModelCapabilities {
    /// Context window size in tokens
    pub context_window: u64,
    /// Recommended max_tokens for generation (typically 10-25% of context window)
    pub recommended_max_tokens: u32,
    /// Whether this model supports function calling / tool use
    pub supports_tool_calling: bool,
}

/// Model capability registry
pub struct ModelRegistry {
    /// Local capabilities (built-in)
    capabilities: HashMap<String, ModelCapabilities>,
    /// Whether to attempt remote fetch
    enable_remote: bool,
}

impl ModelRegistry {
    /// Create a new model registry with local configuration only (fast, offline-safe)
    pub fn new() -> Self {
        Self {
            capabilities: Self::build_capabilities(),
            enable_remote: false,
        }
    }

    /// Create a registry that will attempt remote fetch on first use
    /// WARNING: This may block for up to 3 seconds if network is slow
    pub fn with_remote_fetch() -> Self {
        let mut registry = Self::new();
        registry.enable_remote = true;
        
        // Attempt remote fetch (non-critical, failures are silently ignored)
        if let Ok(remote_caps) = Self::fetch_remote_models() {
            tracing::info!(
                model_count = remote_caps.len(),
                "Loaded {} models from models.dev API",
                remote_caps.len()
            );
            // Merge remote capabilities (remote takes precedence)
            for (key, value) in remote_caps {
                registry.capabilities.insert(key, value);
            }
        } else {
            tracing::debug!("Using local model capabilities fallback (models.dev API unavailable)");
        }
        
        registry
    }

    /// Fetch model capabilities from models.dev API (synchronous, blocks for ~1-3s)
    fn fetch_remote_models() -> Result<HashMap<String, ModelCapabilities>, Box<dyn std::error::Error>> {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()?;
        
        let response = client.get("https://models.dev/api.json")
            .header("User-Agent", "RollballAI/0.1.0")
            .send()?;
        
        if !response.status().is_success() {
            return Err(format!("models.dev API returned {}", response.status()).into());
        }
        
        let json: serde_json::Value = response.json()?;
        let mut capabilities = HashMap::new();
        
        // Parse: { provider_name: { models: { model_id: { limit: { context, output }, ... } } } }
        if let serde_json::Value::Object(providers) = json {
            for (_provider_name, provider_data) in providers {
                if let Some(models) = provider_data.get("models") {
                    if let serde_json::Value::Object(model_map) = models {
                        for (model_id, model_data) in model_map {
                            if let Some(limit) = model_data.get("limit") {
                                let context_window = limit.get("context")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(128_000);
                                
                                let max_output = limit.get("output")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(4_096);
                                
                                let tool_call = model_data.get("tool_call")
                                    .and_then(|v| v.as_bool())
                                    .unwrap_or(false);
                                
                                // Recommended max_tokens = min(25% of context, max_output, 16K cap)
                                let recommended_max_tokens = ((context_window / 4) as u32)
                                    .min(max_output as u32)
                                    .min(16_384);
                                
                                capabilities.insert(
                                    model_id.to_lowercase(),
                                    ModelCapabilities {
                                        context_window,
                                        recommended_max_tokens,
                                        supports_tool_calling: tool_call,
                                    },
                                );
                            }
                        }
                    }
                }
            }
        }
        
        Ok(capabilities)
    }

    /// Build local fallback capabilities
    fn build_capabilities() -> HashMap<String, ModelCapabilities> {
        let mut capabilities = HashMap::new();

        // OpenAI models
        capabilities.insert("gpt-4o".to_string(), ModelCapabilities {
            context_window: 128_000,
            recommended_max_tokens: 16_384,
            supports_tool_calling: true,
        });
        capabilities.insert("gpt-4o-mini".to_string(), ModelCapabilities {
            context_window: 128_000,
            recommended_max_tokens: 16_384,
            supports_tool_calling: true,
        });
        capabilities.insert("gpt-4-turbo".to_string(), ModelCapabilities {
            context_window: 128_000,
            recommended_max_tokens: 4_096,
            supports_tool_calling: true,
        });
        capabilities.insert("gpt-4".to_string(), ModelCapabilities {
            context_window: 8_192,
            recommended_max_tokens: 2_048,
            supports_tool_calling: true,
        });
        capabilities.insert("gpt-3.5-turbo".to_string(), ModelCapabilities {
            context_window: 16_385,
            recommended_max_tokens: 4_096,
            supports_tool_calling: true,
        });

        // Anthropic models
        capabilities.insert("claude-sonnet-4".to_string(), ModelCapabilities {
            context_window: 200_000,
            recommended_max_tokens: 8_192,
            supports_tool_calling: true,
        });
        capabilities.insert("claude-3-5-sonnet".to_string(), ModelCapabilities {
            context_window: 200_000,
            recommended_max_tokens: 8_192,
            supports_tool_calling: true,
        });
        capabilities.insert("claude-3-opus".to_string(), ModelCapabilities {
            context_window: 200_000,
            recommended_max_tokens: 4_096,
            supports_tool_calling: true,
        });

        // MiniMax models (based on models.dev data and official docs)
        capabilities.insert("minimax-m2.7".to_string(), ModelCapabilities {
            context_window: 204_800,  // Updated based on models.dev
            recommended_max_tokens: 8_192,
            supports_tool_calling: true,
        });
        capabilities.insert("minimax-m2.6".to_string(), ModelCapabilities {
            context_window: 204_800,
            recommended_max_tokens: 8_192,
            supports_tool_calling: true,
        });
        capabilities.insert("minimax-m2.5".to_string(), ModelCapabilities {
            context_window: 204_800,
            recommended_max_tokens: 8_192,
            supports_tool_calling: true,
        });
        capabilities.insert("minimax-m2".to_string(), ModelCapabilities {
            context_window: 1_000_000,  // MiniMax-M2 has 1M context
            recommended_max_tokens: 128_000,
            supports_tool_calling: true,
        });

        // Qwen models
        capabilities.insert("qwen-max".to_string(), ModelCapabilities {
            context_window: 32_768,
            recommended_max_tokens: 8_192,
            supports_tool_calling: true,
        });
        capabilities.insert("qwen-plus".to_string(), ModelCapabilities {
            context_window: 1_000_000,
            recommended_max_tokens: 32_768,
            supports_tool_calling: true,
        });
        capabilities.insert("qwen-turbo".to_string(), ModelCapabilities {
            context_window: 8_192,
            recommended_max_tokens: 2_048,
            supports_tool_calling: true,
        });

        // DeepSeek models
        capabilities.insert("deepseek-chat".to_string(), ModelCapabilities {
            context_window: 128_000,
            recommended_max_tokens: 8_192,
            supports_tool_calling: true,
        });
        capabilities.insert("deepseek-v3".to_string(), ModelCapabilities {
            context_window: 128_000,
            recommended_max_tokens: 8_192,
            supports_tool_calling: true,
        });

        capabilities
    }

    /// Get model capabilities by name (case-insensitive)
    pub fn get_capabilities(&self, model_name: &str) -> Option<&ModelCapabilities> {
        let key = model_name.to_lowercase();
        self.capabilities.get(&key)
    }

    /// Get recommended max_tokens for a model
    pub fn get_recommended_max_tokens(&self, model_name: &str) -> Option<u32> {
        self.get_capabilities(model_name)
            .map(|caps| caps.recommended_max_tokens)
    }

    /// Get context window size for a model
    pub fn get_context_window(&self, model_name: &str) -> Option<u64> {
        self.get_capabilities(model_name)
            .map(|caps| caps.context_window)
    }

    /// Check if a model supports tool calling
    pub fn supports_tool_calling(&self, model_name: &str) -> bool {
        self.get_capabilities(model_name)
            .map(|caps| caps.supports_tool_calling)
            .unwrap_or(false)
    }

    /// Check if a model is known to the registry
    pub fn is_known_model(&self, model_name: &str) -> bool {
        self.get_capabilities(model_name).is_some()
    }
}

impl Default for ModelRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_known_model_capabilities() {
        let registry = ModelRegistry::new();

        // Test GPT-4o
        let caps = registry.get_capabilities("gpt-4o").unwrap();
        assert_eq!(caps.context_window, 128_000);
        assert_eq!(caps.recommended_max_tokens, 16_384);
        assert!(caps.supports_tool_calling);

        // Test case-insensitive matching
        let caps = registry.get_capabilities("GPT-4O").unwrap();
        assert_eq!(caps.context_window, 128_000);

        // Test MiniMax
        let caps = registry.get_capabilities("MiniMax-M2.7").unwrap();
        assert_eq!(caps.context_window, 204_800);
    }

    #[test]
    fn test_unknown_model() {
        let registry = ModelRegistry::new();
        assert!(registry.get_capabilities("unknown-model").is_none());
        assert!(!registry.is_known_model("unknown-model"));
    }

    #[test]
    fn test_recommended_max_tokens() {
        let registry = ModelRegistry::new();
        assert_eq!(registry.get_recommended_max_tokens("gpt-4o"), Some(16_384));
        assert_eq!(registry.get_recommended_max_tokens("unknown"), None);
    }
}
