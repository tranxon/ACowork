//! Manifest parsing integration test

use std::fs;

#[test]
fn test_parse_valid_manifest() {
    let toml_str = r#"
agent_id = "com.example.weather"
version = "1.0.0"
name = "Weather Agent"
description = "A weather query agent"
author = "RollBall.AI"
runtime_version = "0.1.0"

[llm]
provider = "openai"
model = "gpt-4o-mini"
temperature = 0.7

[memory]
enabled = true

[[tools]]
name = "http_request"

[[tools]]
name = "memory_store"

[[tools]]
name = "memory_recall"

[[permissions]]
type = "Network"
value = "https://wttr.in/*"

[[permissions]]
type = "MemoryWrite"

[[permissions]]
type = "MemoryRead"

[resources]
max_memory_mb = 100
max_execution_time_ms = 30000
"#;

    let manifest = rollball_core::AgentManifest::from_toml(toml_str).expect("Failed to parse manifest");
    assert_eq!(manifest.agent_id, "com.example.weather");
    assert_eq!(manifest.version, "1.0.0");
    assert_eq!(manifest.name, "Weather Agent");
    assert_eq!(manifest.llm.suggested_provider, "openai");
    assert_eq!(manifest.llm.suggested_model, "gpt-4o-mini");
    assert_eq!(manifest.tools.len(), 3);
    assert!(manifest.has_tool("http_request"));
    assert!(manifest.has_tool("memory_store"));
    assert!(manifest.has_tool("memory_recall"));
    assert!(!manifest.has_tool("shell"));
}

#[test]
fn test_parse_minimal_manifest() {
    let toml_str = r#"
agent_id = "com.test.minimal"
version = "0.1.0"
name = "Minimal Agent"
description = "Just testing"
author = "test"
runtime_version = "0.1.0"

[llm]
provider = "ollama"
model = "llama3"
"#;

    let manifest = rollball_core::AgentManifest::from_toml(toml_str).expect("Failed to parse minimal manifest");
    assert_eq!(manifest.agent_id, "com.test.minimal");
    assert!(manifest.tools.is_empty());
    assert!(manifest.permissions.is_empty());
}

#[test]
fn test_parse_invalid_manifest_missing_agent_id() {
    let toml_str = r#"
version = "1.0.0"
name = "Broken Agent"
description = "Missing agent_id"
author = "test"
runtime_version = "0.1.0"

[llm]
provider = "openai"
model = "gpt-4"
"#;

    let result = rollball_core::AgentManifest::from_toml(toml_str);
    assert!(result.is_err(), "Should fail without agent_id");
}

#[test]
fn test_parse_invalid_manifest_missing_llm() {
    let toml_str = r#"
agent_id = "com.test.broken"
version = "1.0.0"
name = "Broken Agent"
description = "Missing LLM config"
author = "test"
runtime_version = "0.1.0"
"#;

    let result = rollball_core::AgentManifest::from_toml(toml_str);
    assert!(result.is_err(), "Should fail without llm config");
}

#[test]
fn test_parse_weather_agent_example() {
    let manifest_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("weather-agent")
        .join("manifest.toml");

    if !manifest_path.exists() {
        eprintln!("Skipping: weather-agent manifest not found");
        return;
    }

    let toml_str = fs::read_to_string(&manifest_path).expect("Failed to read manifest.toml");
    let manifest = rollball_core::AgentManifest::from_toml(&toml_str).expect("Failed to parse weather manifest");

    assert_eq!(manifest.agent_id, "com.example.weather");
    assert_eq!(manifest.llm.suggested_provider, "openai");
    assert!(manifest.has_tool("http_request"));
    assert!(manifest.has_tool("memory_store"));
    assert!(manifest.has_tool("memory_recall"));
}
