//! Embedding service lifecycle management.
//!
//! Manages the `rollball-embed` child process: auto-start at Gateway startup,
//! health-check, model switching, and graceful shutdown.

use std::path::Path;
use std::process::Stdio;
use std::fs;

use crate::error::GatewayError;

/// Default port for the embedding service.
#[allow(dead_code)]
const EMBED_DEFAULT_PORT: u16 = 18080;

/// State of the embedding service process.
#[derive(Debug, Clone)]
pub struct EmbedProcessState {
    /// PID of the running rollball-embed process.
    pub pid: u32,
    /// Port the embedding service is listening on.
    pub port: u16,
    /// Currently loaded model ID (None if still downloading/starting).
    pub active_model_id: Option<String>,
    /// Currently loaded model dimension.
    pub active_dimension: Option<usize>,
    /// Whether the process has completed startup and health check.
    pub ready: bool,
}

/// Spawn the rollball-embed process.
///
/// The embedding service runs as a sibling process to the Gateway,
/// listening on `127.0.0.1:{port}`. It downloads and loads the
/// recommended model on first startup.
pub async fn spawn_embed_process(
    data_dir: &Path,
    models_dir: &Path,
    port: u16,
    hf_mirrors: &[String],
    onnx_variant: &str,
) -> Result<EmbedProcessState, GatewayError> {
    // Locate the rollball-embed binary (sibling of current executable)
    let embed_bin = std::env::current_exe()
        .map_err(|e| GatewayError::Lifecycle(format!("Cannot find current executable: {}", e)))?
        .parent()
        .map(|p| {
            let bin_name = if cfg!(windows) {
                "rollball-embed.exe"
            } else {
                "rollball-embed"
            };
            p.join(bin_name)
        })
        .unwrap_or_else(|| {
            let bin_name = if cfg!(windows) {
                "rollball-embed.exe"
            } else {
                "rollball-embed"
            };
            std::path::PathBuf::from(bin_name)
        });

    if !embed_bin.exists() {
        return Err(GatewayError::Lifecycle(format!(
            "rollball-embed binary not found at {:?}",
            embed_bin
        )));
    }

    // Create log directory and open log file (truncate on each start)
    let log_dir = data_dir.join("logs");
    fs::create_dir_all(&log_dir).map_err(|e| {
        GatewayError::Lifecycle(format!("Failed to create log dir {:?}: {}", log_dir, e))
    })?;
    let log_path = log_dir.join("embed.log");
    let log_file = fs::File::create(&log_path).map_err(|e| {
        GatewayError::Lifecycle(format!("Failed to create embed log file {:?}: {}", log_path, e))
    })?;
    tracing::info!(path = %log_path.display(), "Embed process logging to file");

    let mut cmd = tokio::process::Command::new(&embed_bin);
    cmd.arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .arg("--models-dir")
        .arg(models_dir)
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--onnx-variant")
        .arg(onnx_variant)
        .arg("--log-level")
        .arg("info")
        // Set working directory to the binary's own directory so that
        // onnxruntime.dll / libonnxruntime.so / libonnxruntime.dylib
        // can be found as siblings of the executable.
        .current_dir(embed_bin.parent().unwrap_or(Path::new(".")))
        .stdout(Stdio::null())
        .stderr(Stdio::from(log_file));

    if !hf_mirrors.is_empty() {
        let mirrors_arg = hf_mirrors.join(",");
        cmd.arg("--hf-mirrors").arg(&mirrors_arg);
    }

    // On Unix, create a new process group
    #[cfg(unix)]
    #[allow(unused_imports)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let mut child = cmd.spawn().map_err(|e| {
        GatewayError::Lifecycle(format!(
            "Failed to spawn rollball-embed (binary: {:?}): {}",
            embed_bin, e
        ))
    })?;

    let pid = child.id().ok_or_else(|| {
        GatewayError::Lifecycle(
            "Failed to get PID for rollball-embed (process may have exited immediately)".to_string(),
        )
    })?;

    // Spawn a background task to reap the child's exit status
    tokio::spawn(async move {
        let _ = child.wait().await;
    });

    tracing::info!("Spawned rollball-embed process (PID: {}, port: {})", pid, port);

    Ok(EmbedProcessState {
        pid,
        port,
        active_model_id: None,
        active_dimension: None,
        ready: false,
    })
}

/// Kill the embedding service process.
pub async fn kill_embed_process(pid: u32) -> Result<(), GatewayError> {
    crate::lifecycle::process::kill_agent_process(pid).await
}

/// Check if the embedding service is healthy by calling its /health endpoint.
pub async fn check_embed_health(port: u16) -> Option<EmbedHealthStatus> {
    let url = format!("http://127.0.0.1:{}/health", port);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .ok()?;

    let resp = client.get(&url).send().await.ok()?;
    let body: serde_json::Value = resp.json().await.ok()?;

    let status = body.get("status")?.as_str()?.to_string();
    let model = body.get("model").and_then(|m| {
        let id = m.get("id")?.as_str()?.to_string();
        let dim = m.get("dimension")?.as_u64()? as usize;
        Some(EmbedModelInfo { id, dimension: dim })
    });

    Some(EmbedHealthStatus {
        ready: status == "ready",
        model,
    })
}

/// Health check result from rollball-embed.
#[derive(Debug, Clone)]
pub struct EmbedHealthStatus {
    pub ready: bool,
    pub model: Option<EmbedModelInfo>,
}

/// Model info from health check.
#[derive(Debug, Clone)]
pub struct EmbedModelInfo {
    pub id: String,
    pub dimension: usize,
}

/// Select an embedding model by triggering rollball-embed's load endpoint.
pub async fn select_embed_model(
    port: u16,
    model_id: &str,
) -> Result<(), GatewayError> {
    let url = format!("http://127.0.0.1:{}/models/{}/load", port, model_id);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60)) // model loading can take a while
        .build()
        .map_err(|e| GatewayError::Lifecycle(format!("Failed to build HTTP client: {}", e)))?;

    let resp = client.post(&url).send().await.map_err(|e| {
        GatewayError::Lifecycle(format!("Failed to call load endpoint for model '{}': {}", model_id, e))
    })?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(GatewayError::Lifecycle(format!(
            "Load model '{}' failed (HTTP {}): {}",
            model_id, status, body
        )));
    }

    tracing::info!(model_id, "Embedding model loaded successfully");
    Ok(())
}

/// Trigger a model download via rollball-embed's download endpoint.
///
/// The embed server runs downloads in the background (fire-and-forget)
/// and returns 202 Accepted immediately. Progress can be polled via
/// `get_embed_model_status`.
pub async fn download_embed_model(
    port: u16,
    model_id: &str,
    variant: Option<&str>,
) -> Result<(), GatewayError> {
    let url = format!("http://127.0.0.1:{}/models/{}/download", port, model_id);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30)) // quick handshake only
        .build()
        .map_err(|e| GatewayError::Lifecycle(format!("Failed to build HTTP client: {}", e)))?;

    let body = serde_json::json!({
        "variant": variant
    });

    let resp = client.post(&url).json(&body).send().await.map_err(|e| {
        GatewayError::Lifecycle(format!("Failed to call download endpoint for model '{}': {}", model_id, e))
    })?;

    // 202 Accepted (fire-and-forget) or 200 OK (already_downloaded)
    if !resp.status().is_success() && resp.status() != reqwest::StatusCode::ACCEPTED {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(GatewayError::Lifecycle(format!(
            "Download model '{}' failed (HTTP {}): {}",
            model_id, status, body
        )));
    }

    tracing::info!(model_id, "Embedding model download triggered");
    Ok(())
}

/// Get model status from rollball-embed.
pub async fn get_embed_model_status(
    port: u16,
    model_id: &str,
) -> Result<serde_json::Value, GatewayError> {
    let url = format!("http://127.0.0.1:{}/models/{}/status", port, model_id);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| GatewayError::Lifecycle(format!("Failed to build HTTP client: {}", e)))?;

    let resp = client.get(&url).send().await.map_err(|e| {
        GatewayError::Lifecycle(format!("Failed to get status for model '{}': {}", model_id, e))
    })?;

    let body: serde_json::Value = resp.json().await.map_err(|e| {
        GatewayError::Lifecycle(format!("Failed to parse status response: {}", e))
    })?;

    Ok(body)
}

/// Result of an embedding test.
#[derive(Debug, Clone)]
pub struct EmbedTestResult {
    /// Whether the test succeeded.
    pub success: bool,
    /// Model ID that was tested.
    pub model_id: Option<String>,
    /// Embedding dimension returned.
    pub dimension: Option<usize>,
    /// Inference latency in milliseconds.
    pub latency_ms: Option<u64>,
    /// Error message if test failed.
    pub error: Option<String>,
}

/// Test the currently loaded embedding model by sending a sample sentence.
///
/// Calls `POST /v1/embeddings` on the embed server with a short test input
/// and verifies a valid embedding vector is returned.
pub async fn test_embed_model(port: u16) -> Result<EmbedTestResult, GatewayError> {
    let url = format!("http://127.0.0.1:{}/v1/embeddings", port);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| GatewayError::Lifecycle(format!("Failed to build HTTP client: {}", e)))?;

    let body = serde_json::json!({
        "input": "RollBall embedding test sentence."
    });

    let start = std::time::Instant::now();
    let resp = client.post(&url).json(&body).send().await.map_err(|e| {
        GatewayError::Lifecycle(format!("Failed to call embeddings endpoint: {}", e))
    })?;
    let latency_ms = start.elapsed().as_millis() as u64;

    if !resp.status().is_success() {
        let status = resp.status();
        let body_text = resp.text().await.unwrap_or_default();
        return Ok(EmbedTestResult {
            success: false,
            model_id: None,
            dimension: None,
            latency_ms: Some(latency_ms),
            error: Some(format!("HTTP {}: {}", status, body_text)),
        });
    }

    let resp_body: serde_json::Value = resp.json().await.map_err(|e| {
        GatewayError::Lifecycle(format!("Failed to parse embedding response: {}", e))
    })?;

    // Extract model name and embedding dimension from response
    let model_id = resp_body.get("model").and_then(|m| m.as_str()).map(|s| s.to_string());
    let dimension = resp_body
        .get("data")
        .and_then(|d| d.as_array())
        .and_then(|arr| arr.first())
        .and_then(|item| item.get("embedding"))
        .and_then(|emb| emb.as_array())
        .map(|arr| arr.len());

    Ok(EmbedTestResult {
        success: true,
        model_id,
        dimension,
        latency_ms: Some(latency_ms),
        error: None,
    })
}
