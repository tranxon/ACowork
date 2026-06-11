//! CLI arguments and configuration for the embedding runtime.

use clap::Parser;

/// RollBall Embedding Runtime — ONNX-based embedding service
/// with OpenAI-compatible API.
#[derive(Debug, Parser)]
#[command(name = "rollball-embed", version, about)]
pub struct Cli {
    /// Host to bind the HTTP server to.
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// Port to bind the HTTP server to.
    #[arg(long, default_value_t = 18080)]
    pub port: u16,

    /// Directory containing downloaded embedding models.
    #[arg(long)]
    pub models_dir: String,

    /// Directory for embedding_models.json registry.
    /// Defaults to `models_dir` if not specified.
    #[arg(long)]
    pub data_dir: Option<String>,

    /// Active embedding model ID to load at startup.
    /// If not specified, the recommended model will be used
    /// (or the first available model if none is recommended).
    #[arg(long)]
    pub model: Option<String>,

    /// HuggingFace mirror URLs (comma-separated, tried in order before the official site).
    /// Example: "https://hf-mirror.com,https://hf-mirror2.com"
    #[arg(long, value_delimiter = ',', env = "HF_MIRRORS")]
    pub hf_mirrors: Vec<String>,

    /// ONNX variant to download/load (fp32, fp16, int8).
    /// Defaults to "fp16" for smaller model size.
    #[arg(long, default_value = "fp16")]
    pub onnx_variant: String,

    /// Log level (trace, debug, info, warn, error).
    #[arg(long, default_value = "info")]
    pub log_level: String,
}

impl Cli {
    /// Returns the full listen address string (e.g., "127.0.0.1:18080").
    pub fn listen_addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Returns the data directory, defaulting to models_dir.
    pub fn data_dir(&self) -> &str {
        self.data_dir.as_deref().unwrap_or(&self.models_dir)
    }
}
