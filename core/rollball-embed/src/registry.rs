//! Model registry — reads and manages the embedding_models.json registry.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Pooling strategy for embedding models.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PoolingStrategy {
    /// Use [CLS] token output (BGE models).
    Cls,
    /// Mean pooling over token embeddings weighted by attention_mask (MiniLM).
    Mean,
    /// Use last token output (causal LMs).
    LastToken,
}

impl Default for PoolingStrategy {
    fn default() -> Self {
        Self::Cls
    }
}

/// Embedding model entry in embedding_models.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingModelEntry {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub dimension: usize,
    pub max_tokens: usize,
    pub size_mb: u64,
    pub languages: Vec<String>,
    pub hf_repo: String,
    #[serde(default)]
    pub pooling_strategy: PoolingStrategy,
    pub onnx_file: String,
    pub tokenizer_file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub onnx_variants: Option<HashMap<String, String>>,
    pub bundled: bool,
    pub recommended: bool,
}

/// Versioned embedding model list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingModelsFile {
    pub version: u64,
    pub models: Vec<EmbeddingModelEntry>,
}

/// Model download/load status (internal representation).
///
/// For API responses, use [`ModelStatusFlat`] or [`ModelStatus::to_api_parts`]
/// to get a consistent flat format (always string `status` + optional fields).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelStatus {
    /// Model registry entry exists but not downloaded.
    NotDownloaded,
    /// Model is currently being downloaded (0-100 progress).
    Downloading(u8),
    /// Model files are on disk, ready to load.
    Downloaded,
    /// Model is loaded into ONNX Runtime and ready for inference.
    Loaded,
    /// Download or load failed.
    Failed(String),
}

/// Flat API representation of [`ModelStatus`].
///
/// Always serializes as a JSON object with a string `status` field,
/// plus optional `progress` / `error` fields. This avoids the
/// inconsistent string-vs-object output that raw enum serialization
/// would produce.
#[derive(Debug, Clone, Serialize)]
pub struct ModelStatusFlat {
    /// One of: `"not_downloaded"`, `"downloading"`, `"downloaded"`,
    /// `"loaded"`, `"failed"`.
    pub status: &'static str,
    /// Download progress percentage (0-100). Only present when downloading.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<u8>,
    /// Error message. Only present on failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ModelStatus {
    /// Convert to a flat API representation with consistent JSON shape.
    pub fn to_api_parts(&self) -> ModelStatusFlat {
        match self {
            ModelStatus::NotDownloaded => ModelStatusFlat {
                status: "not_downloaded",
                progress: None,
                error: None,
            },
            ModelStatus::Downloading(pct) => ModelStatusFlat {
                status: "downloading",
                progress: Some(*pct),
                error: None,
            },
            ModelStatus::Downloaded => ModelStatusFlat {
                status: "downloaded",
                progress: None,
                error: None,
            },
            ModelStatus::Loaded => ModelStatusFlat {
                status: "loaded",
                progress: None,
                error: None,
            },
            ModelStatus::Failed(reason) => ModelStatusFlat {
                status: "failed",
                progress: None,
                error: Some(reason.clone()),
            },
        }
    }
}

/// Model info with status (for API responses).
#[derive(Debug, Clone, Serialize)]
pub struct ModelInfo {
    #[serde(flatten)]
    pub entry: EmbeddingModelEntry,
    #[serde(flatten)]
    pub status: ModelStatusFlat,
}

/// The model registry, loaded from embedding_models.json.
pub struct ModelRegistry {
    models: Vec<EmbeddingModelEntry>,
    /// Map from model ID to index in the models vec.
    index: HashMap<String, usize>,
}

impl ModelRegistry {
    /// Load registry from the given data directory.
    ///
    /// Search order (matches the `offline_providers.json` pattern):
    ///   1. `{data_dir}/embedding_models.json`  (user-writable, primary)
    ///   2. `{exe_dir}/embedding_models.json`   (installer-provided)
    ///   3. `$CARGO_MANIFEST_DIR/assets/`       (dev / test via cargo)
    ///   4. `{cwd}/embedding_models.json`        (dev convenience)
    ///
    /// Returns an empty registry if no file is found anywhere.
    pub fn load(data_dir: &Path) -> Self {
        let candidates = Self::build_candidates(data_dir);

        for path in &candidates {
            if path.exists() {
                match std::fs::read_to_string(path) {
                    Ok(content) => {
                        match serde_json::from_str::<EmbeddingModelsFile>(&content) {
                            Ok(reg) => {
                                tracing::info!(
                                    path = %path.display(),
                                    count = reg.models.len(),
                                    "Loaded embedding model registry"
                                );
                                return Self::from_models(reg.models);
                            }
                            Err(e) => {
                                tracing::warn!(
                                    path = %path.display(),
                                    error = %e,
                                    "Failed to parse embedding_models.json"
                                );
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %e,
                            "Failed to read embedding_models.json"
                        );
                    }
                }
            }
        }

        tracing::warn!(
            "embedding_models.json not found in any candidate path, using empty registry"
        );
        Self::from_models(Vec::new())
    }

    /// Build candidate file paths in priority order.
    fn build_candidates(data_dir: &Path) -> Vec<PathBuf> {
        let mut candidates = Vec::new();

        // 1. Data directory (user-writable, primary location)
        candidates.push(data_dir.join("embedding_models.json"));

        // 2. Same directory as the executable (installer-provided)
        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(exe_dir) = exe_path.parent() {
                candidates.push(exe_dir.join("embedding_models.json"));
            }
        }

        // 3. CARGO_MANIFEST_DIR assets/ (dev and test via cargo)
        if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
            let assets = PathBuf::from(&manifest_dir)
                .join("assets")
                .join("embedding_models.json");
            if assets.exists() {
                candidates.push(assets);
            }
        }

        // 4. Current working directory (dev convenience)
        if let Ok(cwd) = std::env::current_dir() {
            candidates.push(cwd.join("embedding_models.json"));
        }

        candidates
    }

    /// Create registry from a list of model entries.
    fn from_models(models: Vec<EmbeddingModelEntry>) -> Self {
        let index = models
            .iter()
            .enumerate()
            .map(|(i, m)| (m.id.clone(), i))
            .collect();
        Self { models, index }
    }

    /// Get all model entries.
    pub fn models(&self) -> &[EmbeddingModelEntry] {
        &self.models
    }

    /// Get a model entry by ID.
    pub fn get(&self, id: &str) -> Option<&EmbeddingModelEntry> {
        self.index.get(id).map(|&i| &self.models[i])
    }

    /// Get the recommended model (first model with recommended=true).
    pub fn recommended(&self) -> Option<&EmbeddingModelEntry> {
        self.models.iter().find(|m| m.recommended)
    }

    /// Get the ONNX file path for a model, respecting variant selection.
    pub fn onnx_path(&self, model_id: &str, variant: &str) -> Option<String> {
        let model = self.get(model_id)?;
        if let Some(variants) = &model.onnx_variants {
            if let Some(path) = variants.get(variant) {
                return Some(path.clone());
            }
        }
        // Fallback to the default onnx_file
        Some(model.onnx_file.clone())
    }

    /// Check if a model is downloaded (its directory exists on disk).
    pub fn is_downloaded(&self, models_dir: &Path, model_id: &str) -> bool {
        let model_dir = models_dir.join(model_id);
        model_dir.exists() && model_dir.is_dir()
    }

    /// Get the local directory for a model.
    pub fn model_dir(&self, models_dir: &Path, model_id: &str) -> PathBuf {
        models_dir.join(model_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_registry_from_fallback_path() {
        // In test environment, CARGO_MANIFEST_DIR is set so the assets/ fallback is found
        let dir = tempfile::tempdir().unwrap();
        let registry = ModelRegistry::load(dir.path());
        assert!(!registry.models().is_empty());
        assert!(registry.get("bge-small-zh-v1.5").is_some());
        assert!(registry.get("all-MiniLM-L6-v2").is_some());
        assert!(registry.get("nonexistent").is_none());
    }

    #[test]
    fn test_load_registry_from_data_dir() {
        // When data_dir contains embedding_models.json, it takes priority over fallbacks
        let dir = tempfile::tempdir().unwrap();
        let custom_json = r#"{"version": 1, "models": [{
            "id": "custom-model",
            "name": "Custom",
            "dimension": 256,
            "max_tokens": 128,
            "size_mb": 50,
            "languages": ["en"],
            "hf_repo": "test/repo",
            "pooling_strategy": "mean",
            "onnx_file": "model.onnx",
            "tokenizer_file": "tokenizer.json",
            "bundled": false,
            "recommended": true
        }]}"#;
        std::fs::write(dir.path().join("embedding_models.json"), custom_json).unwrap();
        let registry = ModelRegistry::load(dir.path());
        assert_eq!(registry.models().len(), 1);
        assert!(registry.get("custom-model").is_some());
        assert!(registry.get("bge-small-zh-v1.5").is_none());
    }

    #[test]
    fn test_recommended_model() {
        let dir = tempfile::tempdir().unwrap();
        let registry = ModelRegistry::load(dir.path());
        let rec = registry.recommended().unwrap();
        assert_eq!(rec.id, "bge-small-zh-v1.5");
        assert_eq!(rec.pooling_strategy, PoolingStrategy::Cls);
        assert_eq!(rec.dimension, 512);
    }

    #[test]
    fn test_onnx_variant_selection() {
        let dir = tempfile::tempdir().unwrap();
        let registry = ModelRegistry::load(dir.path());

        // fp16 variant
        let path = registry.onnx_path("bge-small-zh-v1.5", "fp16").unwrap();
        assert_eq!(path, "onnx/model_fp16.onnx");

        // unknown variant falls back to default
        let path = registry.onnx_path("bge-small-zh-v1.5", "int8").unwrap();
        assert_eq!(path, "onnx/model_quantized.onnx");

        // model without variants falls back to default onnx_file
        // (bge-m3 only has fp32 in variants)
        let path = registry.onnx_path("bge-m3", "fp16").unwrap();
        assert_eq!(path, "model.onnx");
    }

    #[test]
    fn test_pooling_strategy_deserialize() {
        let json = r#"{"pooling_strategy": "mean"}"#;
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        let strategy: PoolingStrategy = serde_json::from_value(v["pooling_strategy"].clone()).unwrap();
        assert_eq!(strategy, PoolingStrategy::Mean);
    }
}
