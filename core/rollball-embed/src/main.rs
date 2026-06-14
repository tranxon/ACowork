//! RollBall Embedding Runtime — ONNX-based embedding service
//! with OpenAI-compatible API.
//!
//! Entry point: parse CLI arguments, initialize logging, load model,
//! start HTTP server, and handle graceful shutdown.

use std::sync::Arc;

use clap::Parser;

use rollball_embed::config::Cli;
use rollball_embed::download::{Downloader, DownloadProgress};
use rollball_embed::event_bus::{EventBus, State as BusState};
use rollball_embed::model::EmbeddingModel;
use rollball_embed::registry::ModelRegistry;
use rollball_embed::server::AppState;
use rollball_embed::shutdown::Shutdown;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Initialize logging
    init_logging(&cli.log_level);

    tracing::info!("RollBall Embedding Runtime starting");
    tracing::info!(addr = %cli.listen_addr(), "Listen address");

    // Create shutdown signal
    let shutdown = Shutdown::new();
    rollball_embed::shutdown::install_signal_handlers(shutdown.clone());

    // Ensure models directory exists
    let models_dir = std::path::PathBuf::from(&cli.models_dir);
    if !models_dir.exists() {
        std::fs::create_dir_all(&models_dir).expect("Failed to create models directory");
        tracing::info!(dir = %models_dir.display(), "Created models directory");
    }

    // Load model registry
    let data_dir = std::path::PathBuf::from(cli.data_dir());
    let registry = ModelRegistry::load(&data_dir);
    tracing::info!(count = registry.models().len(), "Loaded model registry");

    // Create event bus for SSE — heartbeats run on a 2s cadence so the
    // gateway can detect a stuck embed process within ~10s.
    let event_bus = EventBus::new(64);
    event_bus.spawn_heartbeat(2000);
    event_bus.publish_state(BusState::Starting);

    // Create downloader
    let downloader = Downloader::new(&models_dir, cli.hf_mirrors.clone());

    // Determine which model to load (resolve to owned String before moving registry)
    let default_model_id = cli
        .model
        .clone()
        .or_else(|| registry.recommended().map(|m| m.id.clone()))
        .unwrap_or_else(|| {
            registry
                .models()
                .first()
                .map(|m| m.id.clone())
                .unwrap_or_else(|| "bge-small-zh-v1.5".to_string())
        });

    tracing::info!(model_id = %default_model_id, "Target model");

    // Try to load model at startup
    let mut initial_model = load_model_if_available(
        &default_model_id,
        &registry,
        &models_dir,
        &downloader,
        &cli.onnx_variant,
    )
    .await;

    if initial_model.is_some() {
        tracing::info!(model_id = %default_model_id, "Model loaded at startup");
        let dim = initial_model.as_ref().unwrap().dimension();
        event_bus.publish_state(BusState::Ready {
            model_id: default_model_id.clone(),
            dimension: dim,
        });
    } else {
        tracing::warn!(
            model_id = %default_model_id,
            "Model not available at startup. Auto-downloading recommended model..."
        );

        // Auto-download recommended model on first startup
        if !downloader.is_downloaded(&default_model_id) {
            tracing::info!(
                model_id = %default_model_id,
                "Auto-downloading recommended model..."
            );
            event_bus.publish_state(BusState::DownloadingRecommended {
                model_id: default_model_id.clone(),
                progress: 0,
            });

            let entry = registry.get(&default_model_id);
            if let Some(entry) = entry {
                let onnx_file = registry
                    .onnx_path(&default_model_id, &cli.onnx_variant)
                    .unwrap_or(entry.onnx_file.clone());

                let cancel_flag = std::sync::atomic::AtomicBool::new(false);
                let progress = DownloadProgress::new();
                match downloader
                    .download_model(
                        &default_model_id,
                        &entry.hf_repo,
                        &onnx_file,
                        &entry.tokenizer_file,
                        &progress,
                        &cancel_flag,
                    )
                    .await
                {
                    Ok(_) => {
                        tracing::info!(model_id = %default_model_id, "Auto-download complete, loading model...");
                        event_bus.publish_state(BusState::Loading {
                            model_id: default_model_id.clone(),
                        });
                        if let Some(model) = try_load_model(&default_model_id, &registry, &models_dir)
                        {
                            tracing::info!(model_id = %default_model_id, "Model loaded after auto-download");
                            let dim = model.dimension();
                            event_bus.publish_state(BusState::Ready {
                                model_id: default_model_id.clone(),
                                dimension: dim,
                            });
                            initial_model = Some(model);
                        } else {
                            event_bus.publish_state(BusState::Error {
                                message: format!(
                                    "Model '{default_model_id}' downloaded but failed to load"
                                ),
                            });
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            model_id = %default_model_id,
                            error = %e,
                            "Auto-download failed. Server will start without a loaded model."
                        );
                        event_bus.publish_state(BusState::Error {
                            message: format!("Auto-download of '{default_model_id}' failed: {e}"),
                        });
                    }
                }
            }
        }
    }

    // Build application state
    let state = Arc::new(AppState {
        model: tokio::sync::RwLock::new(initial_model.map(Arc::new)),
        registry,
        downloader,
        download_status: tokio::sync::RwLock::new(std::collections::HashMap::new()),
        download_progress: tokio::sync::RwLock::new(std::collections::HashMap::new()),
        shutdown: shutdown.clone(),
        models_dir: models_dir.clone(),
        onnx_variant: cli.onnx_variant.clone(),
        default_model: Some(default_model_id),
        download_cancel_flags: tokio::sync::RwLock::new(std::collections::HashMap::new()),
        event_bus: event_bus.clone(),
    });

    // Build router
    let app = rollball_embed::server::build_router(state.clone());

    // Start HTTP server
    let listener = tokio::net::TcpListener::bind(cli.listen_addr())
        .await
        .expect("Failed to bind listen address");

    tracing::info!(addr = %cli.listen_addr(), "HTTP server listening");

    // Run server with graceful shutdown
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown))
        .await
        .expect("HTTP server error");

    tracing::info!("RollBall Embedding Runtime stopped");
}

/// Try to load a model if its files are on disk.
async fn load_model_if_available(
    model_id: &str,
    registry: &ModelRegistry,
    models_dir: &std::path::Path,
    _downloader: &Downloader,
    _onnx_variant: &str,
) -> Option<EmbeddingModel> {
    try_load_model(model_id, registry, models_dir)
}

/// Synchronously try to load a model from disk.
fn try_load_model(
    model_id: &str,
    registry: &ModelRegistry,
    models_dir: &std::path::Path,
) -> Option<EmbeddingModel> {
    let entry = registry.get(model_id)?;

    let model_dir = models_dir.join(model_id);
    let onnx_path = model_dir.join("model.onnx");
    let tokenizer_path = model_dir.join("tokenizer.json");

    if !onnx_path.exists() {
        tracing::debug!(path = %onnx_path.display(), "ONNX file not found");
        return None;
    }
    if !tokenizer_path.exists() {
        tracing::debug!(path = %tokenizer_path.display(), "Tokenizer file not found");
        return None;
    }

    match EmbeddingModel::load(
        model_id,
        &onnx_path,
        &tokenizer_path,
        entry.pooling_strategy.clone(),
        entry.dimension,
        entry.max_tokens,
    ) {
        Ok(model) => Some(model),
        Err(e) => {
            tracing::error!(model_id, error = %e, "Failed to load model");
            None
        }
    }
}

/// Wait for shutdown signal.
async fn shutdown_signal(shutdown: Arc<Shutdown>) {
    // Wait until shutdown flag is set
    while !shutdown.is_shutting_down() {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    tracing::info!("Graceful shutdown initiated, waiting for in-flight requests...");

    // Grace period: wait up to 5 seconds for in-flight requests
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    tracing::info!("Grace period elapsed, shutting down");
}

/// Initialize the tracing subscriber.
fn init_logging(level: &str) {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(level));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_thread_ids(false)
        .init();
}
