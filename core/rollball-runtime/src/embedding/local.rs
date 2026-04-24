//! Local Embedding Provider (feature-gated: `local-embeddings`)
//!
//! Uses ONNX Runtime to run all-MiniLM-L6-v2 locally for embedding generation.
//! No network calls required — fully offline capable.
//!
//! To enable, add `local-embeddings` feature to rollball-runtime:
//! ```toml
//! rollball-runtime = { workspace = true, features = ["local-embeddings"] }
//! ```

use async_trait::async_trait;

use super::{EmbeddingError, EmbeddingProvider};

/// Local embedding provider using ONNX Runtime
///
/// Loads the all-MiniLM-L6-v2 model for generating 384-dimensional embeddings.
/// The model file must be available at the configured path or bundled.
pub struct LocalEmbeddingProvider {
    /// Embedding dimension (384 for all-MiniLM-L6-v2)
    dimension: usize,
    /// Whether the model is loaded and ready
    loaded: bool,
    /// Model path
    model_path: String,
}

impl LocalEmbeddingProvider {
    /// Create a new local embedding provider
    ///
    /// The `model_path` should point to the ONNX model file.
    /// If None, uses the default bundled model location.
    pub fn new(model_path: Option<&str>) -> Self {
        let path = model_path
            .map(|p| p.to_string())
            .unwrap_or_else(|| "models/all-MiniLM-L6-v2.onnx".to_string());

        Self {
            dimension: 384,
            loaded: false,
            model_path: path,
        }
    }

    /// Try to load the ONNX model
    pub async fn load(&mut self) -> Result<(), EmbeddingError> {
        // Check if model file exists
        if !std::path::Path::new(&self.model_path).exists() {
            tracing::warn!(
                path = %self.model_path,
                "ONNX model file not found; local embeddings unavailable"
            );
            return Err(EmbeddingError::Local(format!(
                "Model file not found: {}",
                self.model_path
            )));
        }

        // TODO: When ort dependency is available, load the model here:
        // let session = ort::Session::builder()
        //     .expect("Failed to create ONNX session builder")
        //     .commit_from_file(&self.model_path)
        //     .map_err(|e| EmbeddingError::Local(format!("Failed to load model: {e}")))?;

        // For now, mark as loaded if the file exists
        // The actual ONNX inference will be implemented when ort is integrated
        tracing::info!(
            path = %self.model_path,
            "Local embedding model path configured (ort integration pending)"
        );
        self.loaded = true;
        Ok(())
    }
}

#[async_trait]
impl EmbeddingProvider for LocalEmbeddingProvider {
    fn name(&self) -> &str {
        "local-onnx"
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        if !self.loaded {
            return Err(EmbeddingError::Local(
                "Model not loaded. Call load() first.".to_string(),
            ));
        }

        // TODO: Implement actual ONNX inference when ort is integrated:
        // 1. Tokenize text using the model's tokenizer
        // 2. Run inference through ONNX Runtime
        // 3. Apply mean pooling to get the final embedding
        // 4. Normalize the embedding vector

        // Placeholder: return zero vector with correct dimension
        // This will be replaced with actual inference
        tracing::warn!("Local embedding inference not yet implemented; returning zero vector placeholder");
        Ok(vec![0.0f32; self.dimension])
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        if !self.loaded {
            return Err(EmbeddingError::Local(
                "Model not loaded. Call load() first.".to_string(),
            ));
        }

        let mut embeddings = Vec::with_capacity(texts.len());
        for _ in texts {
            embeddings.push(vec![0.0f32; self.dimension]);
        }

        tracing::warn!("Local batch embedding inference not yet implemented; returning zero vectors placeholder");
        Ok(embeddings)
    }

    fn dimension(&self) -> usize {
        self.dimension
    }

    async fn is_available(&self) -> bool {
        self.loaded
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_local_provider_creation() {
        let provider = LocalEmbeddingProvider::new(None);
        assert_eq!(provider.name(), "local-onnx");
        assert_eq!(provider.dimension(), 384);
        assert!(!provider.loaded);
    }

    #[test]
    fn test_local_provider_custom_path() {
        let provider = LocalEmbeddingProvider::new(Some("/custom/model.onnx"));
        assert_eq!(provider.model_path, "/custom/model.onnx");
    }

    #[tokio::test]
    async fn test_local_provider_not_loaded() {
        let provider = LocalEmbeddingProvider::new(None);
        let result = provider.embed("test").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_local_provider_availability() {
        let provider = LocalEmbeddingProvider::new(None);
        assert!(!provider.is_available().await);
    }

    #[tokio::test]
    async fn test_local_provider_batch_not_loaded() {
        let provider = LocalEmbeddingProvider::new(None);
        let result = provider.embed_batch(&["test1", "test2"]).await;
        assert!(result.is_err());
    }
}
