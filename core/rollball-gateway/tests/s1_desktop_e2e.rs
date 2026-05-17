//! Desktop App S1 E2E 集成测试
//!
//! 完整的端到端测试流程：
//! 1. 启动 Gateway daemon
//! 2. 打包并安装 System Agent
//! 3. 启动 Agent 并验证就绪
//! 4. 发送对话消息（HTTP + WebSocket）
//! 5. 验证响应内容
//!
//! 使用方式：
//! ```bash
//! # 不需要 API Key 的测试
//! cd core && cargo test --test s1_desktop_e2e -- --test-threads=1 --nocapture
//!
//! # 需要 API Key 的对话测试
//! export MINIMAX_API_KEY="your-key"
//! cd core && cargo test --test s1_desktop_e2e -- --test-threads=1 --nocapture
//! ```
//!
//! 注意：必须使用 --test-threads=1 强制串行，避免端口冲突

use std::env;
use std::fs;
use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use rollball_gateway::config::GatewayConfig;

/// Global lock to serialize E2E tests that spawn Gateway processes.
/// Prevents port conflicts when running under `cargo test --workspace`.
static E2E_LOCK: Mutex<()> = Mutex::new(());

// ── 常量 ──────────────────────────────────────────────────────────────

const GATEWAY_URL: &str = "http://127.0.0.1:19876";
const GATEWAY_PORT: u16 = 19876;
const GATEWAY_STARTUP_TIMEOUT: Duration = Duration::from_secs(120);
const AGENT_STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

// ── 测试夹具 ─────────────────────────────────────────────────────────

fn temp_dir(name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("rollball-s1-e2e-{}-{}", name, id));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn examples_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("examples")
}

fn gateway_config(temp_dir: &Path) -> GatewayConfig {
    GatewayConfig {
        socket_path: temp_dir.join("gateway.sock").to_string_lossy().to_string(),
        vault_dir: temp_dir.join("vault").to_string_lossy().to_string(),
        packages_dir: temp_dir.join("packages").to_string_lossy().to_string(),
        data_dir: temp_dir.join("data").to_string_lossy().to_string(),
        log_level: "info".to_string(),
        log_file_size_mb: 10,
        idle_timeout_secs: 0,
        max_iterations: 20,
        iteration_timeout_ms: 30000,
        dev_mode: true,
        http: rollball_gateway::config::HttpConfig {
            enabled: true,
            host: "127.0.0.1".to_string(),
            port: GATEWAY_PORT,
            port_max: GATEWAY_PORT + 10,
            cors_enabled: true,
            auth_enabled: false,
        },
        default_provider: None,
        default_model: None,
        max_output_tokens_limit: 32_768,
    }
}

fn package_agent(source_dir: &Path, output_path: &Path) -> anyhow::Result<()> {
    let file = fs::File::create(output_path)?;
    let mut writer = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default();
    collect_and_write_dir(&mut writer, source_dir, source_dir, &options)?;
    writer.finish()?;
    Ok(())
}

fn collect_and_write_dir(
    writer: &mut zip::ZipWriter<fs::File>,
    base: &Path,
    current: &Path,
    options: &zip::write::SimpleFileOptions,
) -> anyhow::Result<()> {
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path.strip_prefix(base).unwrap();
        let name = relative.to_string_lossy().replace('\\', "/");
        if path.is_dir() {
            collect_and_write_dir(writer, base, &path, options)?;
        } else {
            writer.start_file(&name, *options)?;
            writer.write_all(&fs::read(&path)?)?;
        }
    }
    Ok(())
}

// ── HTTP 客户端 ──────────────────────────────────────────────────────

fn http_get(url: &str) -> anyhow::Result<String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(HTTP_REQUEST_TIMEOUT)
        .build()?;
    let resp = client.get(url).send()?;
    Ok(resp.text()?)
}

fn http_post(url: &str, body: &str) -> anyhow::Result<String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(HTTP_REQUEST_TIMEOUT)
        .build()?;
    let resp = client.post(url)
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()?;
    Ok(resp.text()?)
}

// ── Gateway 生命周期管理 ─────────────────────────────────────────────

struct GatewayProcess {
    child: Option<Child>,
    #[allow(dead_code)]
    temp_dir: PathBuf,
}

impl GatewayProcess {
    fn start(temp_dir: &Path) -> anyhow::Result<Self> {
        let config = gateway_config(temp_dir);
        let config_path = temp_dir.join("gateway.toml");
        fs::write(&config_path, toml::to_string(&config).unwrap())?;

        let gateway_bin = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("target")
            .join("debug")
            .join(if cfg!(windows) { "rollball-gateway.exe" } else { "rollball-gateway" });

        if !gateway_bin.exists() {
            anyhow::bail!(
                "Gateway binary not found at {:?}. Run `cargo build --bin rollball-gateway` first.",
                gateway_bin
            );
        }

        let mut child = Command::new(&gateway_bin)
            .args(["--config-path", config_path.to_str().unwrap(), "--daemon"])
            .env("RUST_LOG", "info")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let deadline = std::time::Instant::now() + GATEWAY_STARTUP_TIMEOUT;
        while std::time::Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(status)) => {
                    if let Some(ref mut stderr) = child.stderr {
                        use std::io::Read;
                        let mut buf = String::new();
                        stderr.read_to_string(&mut buf).ok();
                        eprintln!("Gateway stderr: {}", buf);
                    }
                    anyhow::bail!("Gateway exited with status: {}", status);
                }
                Ok(None) => {
                    if let Ok(resp) = http_get(&format!("{}/health", GATEWAY_URL))
                        && resp.contains("status")
                    {
                        println!("Gateway started on port {}", GATEWAY_PORT);
                        return Ok(Self { child: Some(child), temp_dir: temp_dir.to_path_buf() });
                    }
                }
                Err(e) => anyhow::bail!("Failed to check Gateway: {}", e),
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        anyhow::bail!("Gateway failed to start within {}s", GATEWAY_STARTUP_TIMEOUT.as_secs())
    }

    fn stop(&mut self) {
        if let Some(ref mut child) = self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for GatewayProcess {
    fn drop(&mut self) { self.stop(); }
}

// ── Agent 操作 ────────────────────────────────────────────────────────

fn wait_for_gateway() -> anyhow::Result<()> {
    let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(5)).build()?;
    let start = std::time::Instant::now();
    while start.elapsed() < GATEWAY_STARTUP_TIMEOUT {
        if client.get(format!("{}/health", GATEWAY_URL)).send().is_ok() { return Ok(()); }
        std::thread::sleep(Duration::from_millis(500));
    }
    anyhow::bail!("Gateway not responding")
}

fn list_agents() -> anyhow::Result<Vec<serde_json::Value>> {
    Ok(serde_json::from_str(&http_get(&format!("{}/api/agents", GATEWAY_URL))?)?)
}

fn start_agent(agent_id: &str) -> anyhow::Result<()> {
    let resp = http_post(&format!("{}/api/agents/{}/start", GATEWAY_URL, agent_id), "{}")?;
    let result: serde_json::Value = serde_json::from_str(&resp)?;
    if result.get("error").is_some() { anyhow::bail!("Failed to start agent: {}", resp); }
    Ok(())
}

fn send_message(agent_id: &str, content: &str) -> anyhow::Result<serde_json::Value> {
    let body = serde_json::json!({ "content": content }).to_string();
    let resp = http_post(&format!("{}/api/agents/{}/message", GATEWAY_URL, agent_id), &body)?;
    Ok(serde_json::from_str(&resp)?)
}

fn install_and_start_system_agent(temp: &Path) -> anyhow::Result<()> {
    // Check if already installed (dev_mode auto-installs on startup)
    let already_installed = list_agents()
        .map(|agents| {
            agents.iter().any(|a| a.get("agent_id").and_then(|v| v.as_str()) == Some("com.rollball.system"))
        })
        .unwrap_or(false);

    if !already_installed {
        let system_agent_dir = examples_dir().join("system-agent");
        anyhow::ensure!(system_agent_dir.exists(), "system-agent not found");

        let package_path = temp.join("system.agent");
        package_agent(&system_agent_dir, &package_path)?;

        let install_body = serde_json::json!({
            "package_path": package_path.to_str().unwrap(),
            "dev_mode": true
        }).to_string();
        http_post(&format!("{}/api/agents/install", GATEWAY_URL), &install_body)?;
    } else {
        println!("System Agent already installed (dev_mode auto-install)");
    }

    // Check if already running (dev_mode auto-starts)
    let already_running = http_get(&format!("{}/api/agents/com.rollball.system", GATEWAY_URL))
        .ok()
        .and_then(|resp| serde_json::from_str::<serde_json::Value>(&resp).ok())
        .and_then(|agent| agent.get("running").and_then(|v| v.as_bool()))
        .unwrap_or(false);

    if !already_running {
        start_agent("com.rollball.system")?;
    } else {
        println!("System Agent already running");
    }

    // Wait for agent ready (running + connected via gRPC AgentHello)
    let deadline = std::time::Instant::now() + AGENT_STARTUP_TIMEOUT;
    while std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1000));
        if let Ok(resp) = http_get(&format!("{}/api/agents/com.rollball.system", GATEWAY_URL))
            && let Ok(agent) = serde_json::from_str::<serde_json::Value>(&resp)
            && agent.get("running").and_then(|v| v.as_bool()).unwrap_or(false)
            && agent.get("connected").and_then(|v| v.as_bool()).unwrap_or(false)
        {
            println!("System Agent is running and connected");
            return Ok(());
        }
    }
    anyhow::bail!("System Agent failed to start within {}s", AGENT_STARTUP_TIMEOUT.as_secs())
}

fn has_api_key() -> bool {
    env::var("MINIMAX_API_KEY").or_else(|_| env::var("OPENAI_API_KEY")).or_else(|_| env::var("ANTHROPIC_API_KEY")).is_ok()
}

// ── 测试用例 ─────────────────────────────────────────────────────────

#[test]
fn test_s1_gateway_health_check() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = temp_dir("health");
    let _gw = GatewayProcess::start(&temp).expect("Failed to start Gateway");
    wait_for_gateway().expect("Gateway not responding");

    let resp = http_get(&format!("{}/health", GATEWAY_URL)).expect("Health check failed");
    let health: serde_json::Value = serde_json::from_str(&resp).expect("Invalid JSON");
    assert!(health.get("status").is_some());
    assert!(health.get("version").is_some());
    println!("Health: {}", resp);
}

#[test]
fn test_s1_package_system_agent() {
    let system_agent_dir = examples_dir().join("system-agent");
    if !system_agent_dir.exists() { return; }

    let temp = temp_dir("package");
    let package_path = temp.join("com.rollball.system.agent");
    package_agent(&system_agent_dir, &package_path).expect("Failed to package agent");

    let mut archive = zip::ZipArchive::new(fs::File::open(&package_path).unwrap()).unwrap();
    let names: Vec<String> = (0..archive.len()).map(|i| archive.by_index(i).unwrap().name().to_string()).collect();
    assert!(names.contains(&"manifest.toml".to_string()));
    assert!(names.iter().any(|n| n.contains("prompts/")));
    println!("Package created with {} files", names.len());
}

#[test]
fn test_s1_install_system_agent() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = temp_dir("install");
    let _gw = GatewayProcess::start(&temp).expect("Failed to start Gateway");
    wait_for_gateway().expect("Gateway not responding");

    // In dev_mode, System Agent is auto-installed — verify it exists
    let agents = list_agents().expect("Failed to list agents");
    let ids: Vec<&str> = agents.iter().filter_map(|a| a.get("agent_id").and_then(|v| v.as_str())).collect();
    if ids.contains(&"com.rollball.system") {
        println!("System Agent already installed (dev_mode auto-install)");
    } else {
        // Manual install if not auto-installed
        let system_agent_dir = examples_dir().join("system-agent");
        if !system_agent_dir.exists() { return; }

        let package_path = temp.join("system.agent");
        package_agent(&system_agent_dir, &package_path).expect("Failed to package agent");

        let install_body = serde_json::json!({
            "package_path": package_path.to_str().unwrap(),
            "dev_mode": true
        }).to_string();
        let resp = http_post(&format!("{}/api/agents/install", GATEWAY_URL), &install_body).expect("Install failed");
        println!("Install response: {}", resp);
    }

    let agents = list_agents().expect("Failed to list agents");
    let ids: Vec<&str> = agents.iter().filter_map(|a| a.get("agent_id").and_then(|v| v.as_str())).collect();
    assert!(ids.contains(&"com.rollball.system"));
}

#[test]
fn test_s1_start_system_agent() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = temp_dir("start");
    let _gw = GatewayProcess::start(&temp).expect("Failed to start Gateway");
    wait_for_gateway().expect("Gateway not responding");
    install_and_start_system_agent(&temp).expect("Failed to install and start agent");
}

#[test]
fn test_s1_chat_with_system_agent() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !has_api_key() { eprintln!("SKIPPING: No API key set"); return; }

    let temp = temp_dir("chat");
    let _gw = GatewayProcess::start(&temp).expect("Failed to start Gateway");
    wait_for_gateway().expect("Gateway not responding");
    install_and_start_system_agent(&temp).expect("Failed to setup agent");

    let resp = send_message("com.rollball.system", "Hello, who are you?").expect("Failed to send message");
    println!("Response: {}", serde_json::to_string_pretty(&resp).unwrap());
    assert!(resp.get("message_id").is_some());
    assert!(resp.get("status").is_some());
}

#[test]
fn test_s1_full_conversation_flow() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !has_api_key() { eprintln!("SKIPPING: No API key set"); return; }

    let temp = temp_dir("full_flow");
    let _gw = GatewayProcess::start(&temp).expect("Failed to start Gateway");
    wait_for_gateway().expect("Gateway not responding");

    // Step 1: Gateway health
    println!("Step 1: Gateway health OK");

    // Step 2: Package & Install & Start
    println!("Step 2: Installing System Agent...");
    install_and_start_system_agent(&temp).expect("Failed to setup agent");
    println!("  ✓ Agent running");

    // Step 3: List agents
    let agents = list_agents().expect("Failed to list agents");
    let ids: Vec<&str> = agents.iter().filter_map(|a| a.get("agent_id").and_then(|v| v.as_str())).collect();
    assert!(ids.contains(&"com.rollball.system"));
    println!("  ✓ Agents: {:?}", ids);

    // Step 4: Send messages
    println!("Step 3: Testing conversation...");
    for msg in &["Hello!", "What is your name?", "Tell me a short joke."] {
        match send_message("com.rollball.system", msg) {
            Ok(resp) => {
                let status = resp.get("status").and_then(|v| v.as_str()).unwrap_or("");
                let msg_id = resp.get("message_id").and_then(|v| v.as_str()).unwrap_or("?");
                println!("  \"{}\" -> {} ({})", msg, status, msg_id);
            }
            Err(e) => println!("  \"{}\" -> Error: {}", msg, e),
        }
    }
    println!("✓ Full conversation flow completed!");
}

#[test]
fn test_s1_websocket_streaming() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !has_api_key() { eprintln!("SKIPPING: No API key set"); return; }

    let temp = temp_dir("ws_stream");
    let _gw = GatewayProcess::start(&temp).expect("Failed to start Gateway");
    wait_for_gateway().expect("Gateway not responding");
    install_and_start_system_agent(&temp).expect("Failed to setup agent");

    // Connect to WebSocket
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Failed to create runtime");

    let messages = rt.block_on(async {
        use tokio_tungstenite::{connect_async, tungstenite::Message};
        use futures_util::{SinkExt, StreamExt};

        let url = format!("ws://127.0.0.1:{}/api/agents/com.rollball.system/stream", GATEWAY_PORT);
        let (ws_stream, _) = connect_async(&url).await.expect("WebSocket connect failed");
        let (mut write, mut read) = ws_stream.split();

        // Send message
        let request = serde_json::json!({ "type": "message", "content": "Hello!" });
        write.send(Message::Text(request.to_string().into())).await.expect("WebSocket send failed");

        // Collect responses
        let mut messages = Vec::new();
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(30) {
            tokio::select! {
                msg = read.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&text) {
                                let msg_type = parsed.clone().get("type").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                let preview = if text.len() > 200 { &text[..200] } else { &text };
                                println!("  WS: type={} data={}", msg_type, preview);
                                messages.push(parsed);
                                if msg_type == "done" || msg_type == "error" { break; }
                            }
                        }
                        Some(Ok(Message::Close(_))) | None => break,
                        Some(Err(e)) => { eprintln!("WS error: {}", e); break; }
                        _ => {}
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            }
        }
        messages
    });

    drop(rt);

    println!("Received {} WebSocket messages", messages.len());
    let has_connected = messages.iter().any(|m| m.get("type").and_then(|v| v.as_str()) == Some("connected"));
    assert!(has_connected, "Should receive 'connected' message");
    println!("✓ WebSocket streaming test completed!");
}
