//! Capability registry for Intent routing
//!
//! Tracks which Agent provides which capabilities (actions),
//! enabling the IntentRouter to discover target agents for
//! incoming Intent requests.

use std::collections::HashMap;
use serde::{Deserialize, Serialize};

/// Capability key: `agent_id:action`
pub type CapabilityKey = String;

/// Registered capability entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisteredCapability {
    /// Agent that provides this capability
    pub agent_id: String,
    /// Action name (e.g., "weather_query", "calendar_schedule")
    pub action: String,
    /// Capability definition
    pub definition: rollball_core::CapabilityDef,
}

/// Capability registry — maps `agent_id:action` to CapabilityDef
///
/// S4.2.1: Registry data structure (HashMap<agent:action, CapabilityDef>)
/// Capabilities are registered during package installation and
/// removed during uninstallation.
#[derive(Debug, Clone, Default)]
pub struct CapabilityRegistry {
    /// Map of "agent_id:action" → RegisteredCapability
    capabilities: HashMap<CapabilityKey, RegisteredCapability>,
}

impl CapabilityRegistry {
    /// Create a new empty registry
    pub fn new() -> Self {
        Self::default()
    }

    /// Build the capability key from agent_id and action
    pub fn make_key(agent_id: &str, action: &str) -> CapabilityKey {
        format!("{}:{}", agent_id, action)
    }

    /// Register a capability from an agent manifest
    ///
    /// S4.2.2: Called during package installation to register
    /// all capabilities declared in the manifest.
    pub fn register(&mut self, agent_id: &str, action: &str, definition: rollball_core::CapabilityDef) {
        let key = Self::make_key(agent_id, action);
        tracing::info!("Registering capability: {}", key);
        self.capabilities.insert(key, RegisteredCapability {
            agent_id: agent_id.to_string(),
            action: action.to_string(),
            definition,
        });
    }

    /// Register all capabilities from an agent manifest
    ///
    /// S4.2.2: Install-time registration
    pub fn register_from_manifest(&mut self, agent_id: &str, manifest: &rollball_core::AgentManifest) {
        for (action, def) in &manifest.capabilities {
            self.register(agent_id, action, def.clone());
        }
    }

    /// Unregister all capabilities for an agent
    ///
    /// S4.2.3: Called during package uninstallation
    pub fn unregister_agent(&mut self, agent_id: &str) {
        let prefix = format!("{}:", agent_id);
        let keys_to_remove: Vec<String> = self
            .capabilities
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .cloned()
            .collect();

        for key in keys_to_remove {
            tracing::info!("Unregistering capability: {}", key);
            self.capabilities.remove(&key);
        }
    }

    /// Look up a capability by agent_id and action
    ///
    /// S4.2.4: CapabilityQuery handler support
    pub fn get(&self, agent_id: &str, action: &str) -> Option<&RegisteredCapability> {
        let key = Self::make_key(agent_id, action);
        self.capabilities.get(&key)
    }

    /// Look up a capability by action only (find which agent provides it)
    ///
    /// Used by IntentRouter to discover which agent can handle an action.
    pub fn find_by_action(&self, action: &str) -> Vec<&RegisteredCapability> {
        self.capabilities
            .values()
            .filter(|c| c.action == action)
            .collect()
    }

    /// Get all capabilities for a specific agent
    ///
    /// S4.2.5: capability_overview push support
    pub fn capabilities_for_agent(&self, agent_id: &str) -> Vec<&RegisteredCapability> {
        self.capabilities
            .values()
            .filter(|c| c.agent_id == agent_id)
            .collect()
    }

    /// Get all registered capabilities
    pub fn all_capabilities(&self) -> Vec<&RegisteredCapability> {
        self.capabilities.values().collect()
    }

    /// Get count of registered capabilities
    pub fn len(&self) -> usize {
        self.capabilities.len()
    }

    /// Check if the registry is empty
    pub fn is_empty(&self) -> bool {
        self.capabilities.is_empty()
    }

    /// Get capability overview for handshake step ⑤
    ///
    /// S4.2.5: Returns a summary of all capabilities for the
    /// capability_overview push during IPC handshake.
    pub fn overview(&self) -> CapabilityOverview {
        let mut by_agent: HashMap<String, Vec<String>> = HashMap::new();
        for cap in self.capabilities.values() {
            by_agent
                .entry(cap.agent_id.clone())
                .or_default()
                .push(cap.action.clone());
        }
        CapabilityOverview { by_agent }
    }
}

/// Capability overview for handshake push
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityOverview {
    /// Map of agent_id → list of available actions
    pub by_agent: HashMap<String, Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_capability_def(desc: &str) -> rollball_core::CapabilityDef {
        rollball_core::CapabilityDef {
            description: desc.to_string(),
            input_schema: None,
            output_schema: None,
        }
    }

    #[test]
    fn test_registry_new() {
        let registry = CapabilityRegistry::new();
        assert!(registry.is_empty());
    }

    #[test]
    fn test_register_and_get() {
        let mut registry = CapabilityRegistry::new();
        registry.register("com.example.weather", "query", test_capability_def("Query weather"));
        
        let cap = registry.get("com.example.weather", "query").unwrap();
        assert_eq!(cap.agent_id, "com.example.weather");
        assert_eq!(cap.action, "query");
        assert_eq!(cap.definition.description, "Query weather");
    }

    #[test]
    fn test_register_from_manifest() {
        let mut registry = CapabilityRegistry::new();
        let toml_str = r#"
            agent_id = "com.example.weather"
            version = "1.0.0"
            name = "Weather"
            description = "test"
            author = "test"
            runtime_version = "0.1.0"
            [llm]
            provider = "openai"
            model = "gpt-4"
            [capabilities.query]
            description = "Query weather"
            [capabilities.forecast]
            description = "Weather forecast"
        "#;
        let manifest = rollball_core::AgentManifest::from_toml(toml_str).unwrap();
        registry.register_from_manifest("com.example.weather", &manifest);
        
        assert_eq!(registry.len(), 2);
        assert!(registry.get("com.example.weather", "query").is_some());
        assert!(registry.get("com.example.weather", "forecast").is_some());
    }

    #[test]
    fn test_unregister_agent() {
        let mut registry = CapabilityRegistry::new();
        registry.register("com.example.weather", "query", test_capability_def("Query"));
        registry.register("com.example.weather", "forecast", test_capability_def("Forecast"));
        registry.register("com.example.calendar", "schedule", test_capability_def("Schedule"));
        
        assert_eq!(registry.len(), 3);
        registry.unregister_agent("com.example.weather");
        assert_eq!(registry.len(), 1);
        assert!(registry.get("com.example.weather", "query").is_none());
        assert!(registry.get("com.example.calendar", "schedule").is_some());
    }

    #[test]
    fn test_find_by_action() {
        let mut registry = CapabilityRegistry::new();
        registry.register("com.example.weather", "query", test_capability_def("Weather query"));
        registry.register("com.example.search", "query", test_capability_def("Search query"));
        
        let results = registry.find_by_action("query");
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_capabilities_for_agent() {
        let mut registry = CapabilityRegistry::new();
        registry.register("com.example.weather", "query", test_capability_def("Query"));
        registry.register("com.example.weather", "forecast", test_capability_def("Forecast"));
        registry.register("com.example.calendar", "schedule", test_capability_def("Schedule"));
        
        let weather_caps = registry.capabilities_for_agent("com.example.weather");
        assert_eq!(weather_caps.len(), 2);
    }

    #[test]
    fn test_overview() {
        let mut registry = CapabilityRegistry::new();
        registry.register("com.example.weather", "query", test_capability_def("Query"));
        registry.register("com.example.weather", "forecast", test_capability_def("Forecast"));
        
        let overview = registry.overview();
        assert_eq!(overview.by_agent.len(), 1);
        assert_eq!(overview.by_agent["com.example.weather"].len(), 2);
    }

    #[test]
    fn test_make_key() {
        assert_eq!(CapabilityRegistry::make_key("com.example.weather", "query"), "com.example.weather:query");
    }
}
