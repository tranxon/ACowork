//! Model Ratio Store
//!
//! Maintains a persistent map of model → chars-per-token ratio, calibrated
//! from LLM API usage feedback (`prompt_tokens`). Replaces the complex
//! `sampling_ratios` + `observed_ratios` dual-layer system with a single
//! unified lookup table.
//!
//! ## Calibration
//!
//! After each LLM call, the ratio is computed as:
//!
//! ```text
//! ratio = total_input_chars / prompt_tokens
//! ```
//!
//! where `total_input_chars` is the byte-length of all text in the
//! ChatRequest (messages + tool definitions), and `prompt_tokens` is
//! the API-reported ground truth.
//!
//! ## Smoothing
//!
//! First measurement replaces the hardcoded default directly (no EMA
//! dilution). Subsequent measurements use EMA with α = 0.3:
//!
//! ```text
//! new_ratio = old_ratio * 0.7 + sample * 0.3
//! ```

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

/// The default chars/token ratio used when no calibration data exists.
/// Empirically observed across major LLM families for English/natural-language
/// text. CJK-heavy content will naturally converge to a lower value (~2.0-2.5)
/// through calibration.
pub const DEFAULT_RATIO: f64 = 3.5;

/// EMA smoothing factor for ratio updates.
/// 0.3 means each new measurement contributes 30% to the running value.
const EMA_ALPHA: f64 = 0.3;

/// Minimum and maximum valid chars/token ratios.
/// Below 1.0: impossible (can't be fewer chars than tokens in UTF-8).
/// Above 10.0: unrealistic (would mean ~10 chars per token).
const RATIO_MIN: f64 = 1.0;
const RATIO_MAX: f64 = 10.0;

/// Persistent store of model → chars/token ratios.
///
/// Key format: `"ModelName:ProviderId"` for uniqueness.
/// When no persistence path is set, operates in-memory only.
pub struct ModelRatioStore {
    /// Model key → chars/token ratio
    ratios: HashMap<String, f64>,
    /// Models that have received at least one API calibration.
    /// Used to distinguish hardcoded defaults from measured values.
    calibrated: HashSet<String>,
    /// Optional persistence path. When `Some`, `update()` auto-saves.
    path: Option<PathBuf>,
}

impl ModelRatioStore {
    /// Create an empty, in-memory store with no persistence.
    pub fn new() -> Self {
        Self {
            ratios: HashMap::new(),
            calibrated: HashSet::new(),
            path: None,
        }
    }

    /// Create a store that loads from / saves to the given path.
    ///
    /// If the file exists and is valid JSON, its contents are loaded
    /// as the initial ratio table. If the file is missing or corrupt,
    /// the store starts empty (will be populated by calibration).
    pub fn with_persistence(path: PathBuf) -> Self {
        let mut store = Self::new();
        store.path = Some(path.clone());
        if let Ok(loaded) = Self::load_from_json(&path) {
            store.ratios = loaded.ratios;
            store.calibrated = loaded.calibrated;
            tracing::info!(
                entries = store.ratios.len(),
                path = %path.display(),
                "Loaded model ratio store from disk"
            );
        }
        store
    }

    /// Get the chars/token ratio for a model.
    ///
    /// Lookup order:
    /// 1. Exact match on the full model key
    /// 2. Model-family match: compare the model name portion (before ':') —
    ///    same model across different providers shares the same tokenizer.
    /// 3. Default ratio (3.5)
    pub fn get(&self, model: &str) -> f64 {
        // Exact match
        if let Some(&ratio) = self.ratios.get(model) {
            return ratio;
        }

        // Model-family match: same model name, possibly different provider.
        // Extract model prefix (before ':'), which identifies the tokenizer.
        let model_prefix = model.split(':').next().unwrap_or(model);
        for (key, &ratio) in &self.ratios {
            let key_prefix = key.split(':').next().unwrap_or(key);
            if model_prefix.eq_ignore_ascii_case(key_prefix) {
                return ratio;
            }
        }

        // Default fallback
        DEFAULT_RATIO
    }

    /// Update the ratio for a model with a new sample from API calibration.
    ///
    /// Safety: clamps the sample to [1.0, 10.0] before applying.
    ///
    /// First calibration for a model: directly replaces any default.
    /// Subsequent calibrations: EMA smoothing with α = 0.3.
    ///
    /// Auto-saves to disk if a persistence path is configured.
    pub fn update(&mut self, model: &str, sample: f64) {
        let clamped = sample.clamp(RATIO_MIN, RATIO_MAX);

        if clamped != sample {
            tracing::warn!(
                model = %model,
                raw_sample = %sample,
                clamped = %clamped,
                "Ratio sample out of realistic range, clamping"
            );
        }

        let key = model.to_string();
        if self.calibrated.contains(&key) {
            // EMA smoothing
            let old = self.ratios.get(&key).copied().unwrap_or(DEFAULT_RATIO);
            let new = old * (1.0 - EMA_ALPHA) + clamped * EMA_ALPHA;
            tracing::debug!(
                model = %model,
                old_ratio = %old,
                sample = %clamped,
                new_ratio = %new,
                "Model ratio updated (EMA)"
            );
            self.ratios.insert(key.clone(), new);
        } else {
            // First calibration: direct replace
            tracing::info!(
                model = %model,
                ratio = %clamped,
                "Model ratio calibrated (first measurement)"
            );
            self.ratios.insert(key.clone(), clamped);
            self.calibrated.insert(key.clone());
        }

        // Auto-save
        if let Some(ref path) = self.path
            && let Err(e) = self.save_to_json(path) {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Failed to save model ratio store"
                );
            }
    }

    /// Check if a model has been calibrated at least once.
    pub fn is_calibrated(&self, model: &str) -> bool {
        self.calibrated.contains(model)
    }

    /// Number of entries in the store.
    pub fn len(&self) -> usize {
        self.ratios.len()
    }

    /// Whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.ratios.is_empty()
    }

    // ── Persistence ──────────────────────────────────────────────────────

    /// Load ratios from a JSON file.
    ///
    /// Expected format:
    /// ```json
    /// {
    ///   "ratios": {"MiniMax-M2.7:provider-abc": 3.42},
    ///   "calibrated": ["MiniMax-M2.7:provider-abc"]
    /// }
    /// ```
    pub fn load_from_json(path: &Path) -> io::Result<Self> {
        let data = std::fs::read_to_string(path)?;
        let parsed: serde_json::Value =
            serde_json::from_str(&data).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        let mut store = Self::new();
        store.path = Some(path.to_path_buf());

        if let Some(ratios_obj) = parsed.get("ratios").and_then(|v| v.as_object()) {
            for (key, val) in ratios_obj {
                if let Some(ratio) = val.as_f64() {
                    store.ratios.insert(key.clone(), ratio);
                }
            }
        }

        if let Some(arr) = parsed.get("calibrated").and_then(|v| v.as_array()) {
            for item in arr {
                if let Some(key) = item.as_str() {
                    store.calibrated.insert(key.to_string());
                }
            }
        }

        Ok(store)
    }

    /// Save ratios to a JSON file.
    pub fn save_to_json(&self, path: &Path) -> io::Result<()> {
        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut json = serde_json::Map::new();

        let ratios_obj: serde_json::Map<String, serde_json::Value> = self
            .ratios
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::Number(
                serde_json::Number::from_f64(*v).unwrap_or_else(|| serde_json::Number::from(0)),
            )))
            .collect();
        json.insert("ratios".to_string(), serde_json::Value::Object(ratios_obj));

        let calibrated_arr: Vec<serde_json::Value> = self
            .calibrated
            .iter()
            .map(|k| serde_json::Value::String(k.clone()))
            .collect();
        json.insert("calibrated".to_string(), serde_json::Value::Array(calibrated_arr));

        let data = serde_json::to_string_pretty(&serde_json::Value::Object(json))?;
        std::fs::write(path, data)?;

        Ok(())
    }
}

impl Default for ModelRatioStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_default_ratio_for_unknown_model() {
        let store = ModelRatioStore::new();
        assert_eq!(store.get("unknown-model"), DEFAULT_RATIO);
    }

    #[test]
    fn test_first_calibration_direct_replace() {
        let mut store = ModelRatioStore::new();
        store.update("test-model", 3.8);
        assert!((store.get("test-model") - 3.8).abs() < 0.001);
        assert!(store.is_calibrated("test-model"));
    }

    #[test]
    fn test_ema_smoothing() {
        let mut store = ModelRatioStore::new();
        // First: direct replace
        store.update("test-model", 3.0);
        assert!((store.get("test-model") - 3.0).abs() < 0.001);

        // Second: EMA 0.3
        store.update("test-model", 4.0);
        let expected = 3.0 * 0.7 + 4.0 * 0.3; // = 3.3
        assert!((store.get("test-model") - expected).abs() < 0.001);
    }

    #[test]
    fn test_clamp_out_of_range() {
        let mut store = ModelRatioStore::new();
        store.update("test-model", 0.05);
        assert!((store.get("test-model") - RATIO_MIN).abs() < 0.001);

        let mut store2 = ModelRatioStore::new();
        store2.update("test-model2", 20.0);
        assert!((store2.get("test-model2") - RATIO_MAX).abs() < 0.001);
    }

    #[test]
    fn test_family_match() {
        let mut store = ModelRatioStore::new();
        store.update("MiniMax-M2.7:provider-a", 3.42);
        // Different model with same family should match
        assert!((store.get("MiniMax-M2.7:provider-b") - 3.42).abs() < 0.001);
    }

    #[test]
    fn test_persistence_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("model_ratios.json");

        let mut store = ModelRatioStore::with_persistence(path.clone());
        store.update("test-model:provider-a", 3.42);

        // Load from disk
        let loaded = ModelRatioStore::load_from_json(&path).unwrap();
        assert!((loaded.get("test-model:provider-a") - 3.42).abs() < 0.001);
        assert!(loaded.is_calibrated("test-model:provider-a"));
    }
}
