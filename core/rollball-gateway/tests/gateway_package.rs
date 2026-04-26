//! Gateway package management integration test

use std::fs;
use std::io::Write;
use std::path::Path;

use rollball_gateway::config::GatewayConfig;
use rollball_gateway::gateway::Gateway;

fn create_test_agent_zip(path: &Path, agent_id: &str) {
    let file = fs::File::create(path).expect("Failed to create test ZIP");
    let mut writer = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default();

    let manifest_toml = format!(
        r#"agent_id = "{agent_id}"
version = "1.0.0"
name = "Test Agent"
description = "Test agent"
author = "test"
runtime_version = "0.1.0"

[llm]
provider = "openai"
model = "gpt-4"
"#
    );

    writer.start_file("manifest.toml", options).expect("Failed to add manifest");
    writer.write_all(manifest_toml.as_bytes()).expect("Failed to write manifest");

    writer.start_file("prompts/system.md", options).expect("Failed to add system.md");
    writer.write_all(b"You are a test agent.").expect("Failed to write system.md");

    writer.finish().expect("Failed to finish ZIP");
}

fn test_gateway_config(temp_dir: &Path) -> GatewayConfig {
    GatewayConfig {
        socket_path: temp_dir.join("test.sock").to_string_lossy().to_string(),
        vault_dir: temp_dir.join("vault").to_string_lossy().to_string(),
        packages_dir: temp_dir.join("packages").to_string_lossy().to_string(),
        data_dir: temp_dir.join("data").to_string_lossy().to_string(),
        log_level: "warn".to_string(),
        idle_timeout_secs: 0,
        max_iterations: 20,
        iteration_timeout_ms: 30000,
        dev_mode: true, // tests use unsigned packages
        http: rollball_gateway::config::HttpConfig { enabled: false, ..Default::default() },
    }
}

#[test]
fn test_gateway_install_and_list() {
    let temp_dir = std::env::temp_dir().join("rollball-test-gw-install");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).expect("Failed to create temp dir");

    let config = test_gateway_config(&temp_dir);
    let mut gateway = Gateway::new(config).unwrap();

    let list = gateway.list_agents();
    assert!(list.is_empty(), "Should start with no agents");

    let zip_path = temp_dir.join("test.agent");
    create_test_agent_zip(&zip_path, "com.test.install");

    let result = gateway.install_package(zip_path.to_str().unwrap());
    assert!(result.is_ok(), "Install should succeed: {:?}", result);

    let list = gateway.list_agents();
    assert_eq!(list.len(), 1, "Should have 1 installed agent");
    assert_eq!(list[0].agent_id, "com.test.install");
    assert!(!list[0].running, "Should not be running yet");
}

#[test]
fn test_gateway_uninstall() {
    let temp_dir = std::env::temp_dir().join("rollball-test-gw-uninstall");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).expect("Failed to create temp dir");

    let config = test_gateway_config(&temp_dir);
    let mut gateway = Gateway::new(config).unwrap();

    let zip_path = temp_dir.join("test.agent");
    create_test_agent_zip(&zip_path, "com.test.uninstall");
    gateway.install_package(zip_path.to_str().unwrap()).expect("Install should succeed");

    let list = gateway.list_agents();
    assert_eq!(list.len(), 1);

    let result = gateway.uninstall_package("com.test.uninstall");
    assert!(result.is_ok(), "Uninstall should succeed: {:?}", result);

    let list = gateway.list_agents();
    assert!(list.is_empty(), "Should have no agents after uninstall");
}

#[test]
fn test_gateway_install_duplicate_fails() {
    let temp_dir = std::env::temp_dir().join("rollball-test-gw-dup");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).expect("Failed to create temp dir");

    let config = test_gateway_config(&temp_dir);
    let mut gateway = Gateway::new(config).unwrap();

    let zip_path = temp_dir.join("test.agent");
    create_test_agent_zip(&zip_path, "com.test.dup");
    gateway.install_package(zip_path.to_str().unwrap()).expect("First install should succeed");

    let result = gateway.install_package(zip_path.to_str().unwrap());
    assert!(result.is_err(), "Duplicate install should fail");
}
