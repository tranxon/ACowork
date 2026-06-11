//! HuggingFace model downloader with concurrent multi-source racing.
//!
//! Downloads ONNX model files and tokenizers from HuggingFace Hub.
//! All known sources (official + built-in mirrors + custom mirrors)
//! are raced **concurrently** — the fastest responder wins and
//! the losers are cancelled. This eliminates the need for users
//! to know their network environment or configure mirrors manually.
//!
//! # Built-in sources
//!
//! The official HuggingFace URL (`https://huggingface.co`) and
//! `hf-mirror.com` are always included. Users behind the GFW
//! benefit from the mirror automatically; overseas users benefit
//! from the official source — no configuration needed.
//!
//! Additional custom mirrors can be supplied via `hf_mirrors`
//! for enterprise or private registry scenarios.
//!
//! # Cross-platform notes
//!
//! `std::fs::rename` behaves differently across platforms:
//! - **Unix**: atomically replaces the target if it exists.
//! - **Windows**: fails if the target already exists.
//!
//! We use a `rename_or_replace` helper that handles both cases correctly.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures_util::StreamExt;
use reqwest::Client;

// ── Error type ──────────────────────────────────────────────────────────

/// Error type for download operations.
#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Download cancelled")]
    Cancelled,

    #[error("Invalid URL: {0}")]
    InvalidUrl(String),
}

// ── Cross-platform rename helper ────────────────────────────────────────

/// Rename a file or directory, replacing the target if it already exists.
///
/// On Unix, `std::fs::rename` atomically replaces the target.
/// On Windows, `std::fs::rename` fails if the target exists, so we
/// remove the target first and then rename.
fn rename_or_replace(src: &Path, dst: &Path) -> std::io::Result<()> {
    // Try the simple rename first (works on Unix and when dst doesn't exist)
    if let Ok(()) = std::fs::rename(src, dst) {
        return Ok(());
    }
    // Fallback: remove target then rename (needed on Windows when dst exists)
    if dst.exists() {
        if dst.is_dir() {
            std::fs::remove_dir_all(dst)?;
        } else {
            std::fs::remove_file(dst)?;
        }
    }
    std::fs::rename(src, dst)
}

// ── HuggingFace URL builder ─────────────────────────────────────────────

const HF_DEFAULT_BASE: &str = "https://huggingface.co";

/// Built-in mirror URLs that are always raced alongside the official source.
/// Users never need to configure these — the app handles network adaptation
/// automatically (GFW users benefit from the mirror; overseas users from the
/// official source).
const HF_BUILTIN_MIRRORS: &[&str] = &["https://hf-mirror.com"];

/// Build the download URL for a HuggingFace file.
///
/// `{base}/{repo}/resolve/main/{path}`
fn hf_file_url(hf_repo: &str, file_path: &str, base: &str) -> String {
    format!("{base}/{hf_repo}/resolve/main/{file_path}")
}

// ── Download result ─────────────────────────────────────────────────────

/// Result of a model download operation.
#[derive(Debug, Clone)]
pub struct DownloadResult {
    /// Local directory where files were saved.
    pub model_dir: PathBuf,
    /// List of files that were downloaded.
    pub downloaded_files: Vec<String>,
}

/// Progress callback type: `(downloaded_bytes, total_bytes)`.
pub type ProgressCb = dyn Fn(u64, u64) + Send + Sync;

// ── Shared download progress tracker ──────────────────────────────────

/// Thread-safe download progress tracker shared across concurrent racers.
///
/// All racers update the same instance via atomic operations, so the UI
/// always sees the progress of the fastest (winning) source.
pub struct DownloadProgress {
    /// Bytes downloaded so far for the current file.
    pub bytes_downloaded: AtomicU64,
    /// Total bytes of the current file (0 if unknown).
    pub total_bytes: AtomicU64,
    /// Name of the file currently being downloaded (e.g., "model.onnx").
    pub current_file: std::sync::Mutex<String>,
}

impl DownloadProgress {
    /// Create a new progress tracker with zero state.
    pub fn new() -> Self {
        Self {
            bytes_downloaded: AtomicU64::new(0),
            total_bytes: AtomicU64::new(0),
            current_file: std::sync::Mutex::new(String::new()),
        }
    }

    /// Return progress as `(percentage 0-100, bytes_downloaded, total_bytes)`.
    pub fn snapshot(&self) -> (u8, u64, u64) {
        let downloaded = self.bytes_downloaded.load(Ordering::Relaxed);
        let total = self.total_bytes.load(Ordering::Relaxed);
        let pct = if total > 0 {
            ((downloaded as f64 / total as f64) * 100.0).min(100.0) as u8
        } else {
            0
        };
        (pct, downloaded, total)
    }

    /// Reset progress for a new file.
    fn reset_for_file(&self, file_name: &str) {
        self.bytes_downloaded.store(0, Ordering::Relaxed);
        self.total_bytes.store(0, Ordering::Relaxed);
        if let Ok(mut name) = self.current_file.lock() {
            *name = file_name.to_string();
        }
    }
}

// ── Downloader ──────────────────────────────────────────────────────────

/// HuggingFace model downloader with concurrent multi-source racing.
///
/// All known sources (official + built-in mirrors + optional custom
/// mirrors) are raced concurrently. The fastest responder wins —
/// no manual configuration needed.
pub struct Downloader {
    /// HTTP client (shared across all concurrent racers via Arc).
    http_client: Arc<Client>,
    /// Base models directory.
    models_dir: PathBuf,
    /// Additional custom mirror URLs (beyond built-in mirrors).
    hf_mirrors: Vec<String>,
}

impl Downloader {
    /// Create a new downloader.
    ///
    /// `hf_mirrors` provides additional custom mirror URLs beyond the
    /// built-in ones. Typically left empty — the built-in mirrors
    /// already cover the common network environments.
    pub fn new(models_dir: &Path, hf_mirrors: Vec<String>) -> Self {
        let http_client = Client::builder()
            .timeout(std::time::Duration::from_secs(600)) // 10 min per file
            .connect_timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("Failed to build HTTP client for downloader");

        Self {
            http_client: Arc::new(http_client),
            models_dir: models_dir.to_path_buf(),
            hf_mirrors,
        }
    }

    /// Build the list of all source base URLs.
    ///
    /// Always includes the official HuggingFace URL + built-in mirrors,
    /// plus any user-configured custom mirrors. All sources are raced
    /// concurrently, so ordering does not affect latency.
    fn sources(&self) -> Vec<String> {
        let mut sources = vec![HF_DEFAULT_BASE.to_string()];
        for &m in HF_BUILTIN_MIRRORS {
            sources.push(m.to_string());
        }
        for m in &self.hf_mirrors {
            if !sources.contains(m) {
                sources.push(m.clone());
            }
        }
        sources
    }

    /// Download a model from HuggingFace.
    ///
    /// Downloads the ONNX model file (selected variant) and tokenizer.
    /// Files are written to a temp directory first, then atomically renamed.
    ///
    /// `progress` is a shared tracker that receives real-time byte-level
    /// progress — suitable for exposing to the UI via polling.
    ///
    /// # Arguments
    /// * `model_id` - Model identifier (used as directory name).
    /// * `hf_repo` - HuggingFace repository (e.g., "onnx-community/bge-small-zh-v1.5-ONNX").
    /// * `onnx_file` - Path within the repo to the ONNX file (e.g., "onnx/model_fp16.onnx").
    /// * `tokenizer_file` - Path within the repo to the tokenizer (e.g., "tokenizer.json").
    /// * `progress` - Shared progress tracker updated by the winning racer.
    /// * `cancel` - Cancellation flag (if true, abort download).
    pub async fn download_model(
        &self,
        model_id: &str,
        hf_repo: &str,
        onnx_file: &str,
        tokenizer_file: &str,
        progress: &DownloadProgress,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<DownloadResult, DownloadError> {
        let model_dir = self.models_dir.join(model_id);
        let tmp_dir = self.models_dir.join(format!("{model_id}.downloading"));

        // Clean up any previous incomplete download
        if tmp_dir.exists() {
            std::fs::remove_dir_all(&tmp_dir)?;
        }

        // Create temp directory
        std::fs::create_dir_all(&tmp_dir)?;

        let mut downloaded_files = Vec::new();

        // Files to download: (remote_path, local_name)
        // The ONNX file is always saved as "model.onnx" for consistent loading.
        // The tokenizer is always saved as "tokenizer.json".
        let files_to_download = [
            (onnx_file, "model.onnx"),
            (tokenizer_file, "tokenizer.json"),
        ];

        let sources = self.sources();

        tracing::info!(
            sources = ?sources,
            "Starting concurrent download race with {} sources",
            sources.len()
        );

        for (remote_path, local_name) in &files_to_download {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = std::fs::remove_dir_all(&tmp_dir);
                return Err(DownloadError::Cancelled);
            }

            progress.reset_for_file(local_name);
            let local_path = tmp_dir.join(local_name);
            download_file_race(
                &self.http_client,
                hf_repo,
                remote_path,
                &local_path,
                &sources,
                progress,
            ).await?;

            downloaded_files.push(local_name.to_string());
        }

        // Download ONNX external data file if present.
        // Many ONNX models use external data (e.g., model_fp16.onnx_data) where
        // the ONNX file contains the graph structure and the actual weights are
        // in a companion file. The external data filename is referenced inside
        // the ONNX file (e.g., "model_fp16.onnx_data"), so we must preserve
        // the original filename from the HF repo but strip the directory prefix.
        // ONNX Runtime resolves external data relative to the directory containing
        // model.onnx, using the bare filename stored in the protobuf.
        let onnx_ext_data_remote = format!("{}_data", onnx_file);
        let onnx_ext_data_local = Path::new(&onnx_ext_data_remote)
            .file_name()
            .expect("onnx_ext_data_remote should have a filename")
            .to_str()
            .expect("filename should be valid UTF-8")
            .to_string();
        let ext_data_path = tmp_dir.join(&onnx_ext_data_local);

        progress.reset_for_file(&onnx_ext_data_local);
        match download_file_race(
            &self.http_client,
            hf_repo,
            &onnx_ext_data_remote,
            &ext_data_path,
            &sources,
            progress,
        ).await {
            Ok(()) => {
                downloaded_files.push(onnx_ext_data_local);
            }
            Err(e) => {
                // External data is optional — some models embed weights directly
                tracing::info!(
                    path = %onnx_ext_data_remote,
                    error = %e,
                    "External data file not found (model may have embedded weights)"
                );
            }
        }

        // Atomic rename: tmp_dir → model_dir (cross-platform)
        rename_or_replace(&tmp_dir, &model_dir)?;

        tracing::info!(
            model_id,
            dir = %model_dir.display(),
            files = ?downloaded_files,
            "Model download complete"
        );

        Ok(DownloadResult {
            model_dir,
            downloaded_files,
        })
    }

    /// Check if a model is already downloaded.
    pub fn is_downloaded(&self, model_id: &str) -> bool {
        let model_dir = self.models_dir.join(model_id);
        model_dir.exists()
            && model_dir.join("model.onnx").exists()
            && model_dir.join("tokenizer.json").exists()
    }

    /// Delete a downloaded model.
    pub fn delete_model(&self, model_id: &str) -> Result<(), DownloadError> {
        let model_dir = self.models_dir.join(model_id);
        if model_dir.exists() {
            std::fs::remove_dir_all(&model_dir)?;
            tracing::info!(model_id, "Deleted model files");
        }
        Ok(())
    }

    /// Get the models directory path.
    pub fn models_dir_path(&self) -> &Path {
        &self.models_dir
    }
}

// ── Concurrent download race ───────────────────────────────────────────

/// Race multiple download sources concurrently.
///
/// All sources start downloading simultaneously via `tokio::task::JoinSet`.
/// The first successful download wins — all other tasks are aborted,
/// their partial temp files cleaned up. If all sources fail, the last
/// error is returned.
///
/// Each racer writes to a unique per-source temp file (`{dest}_{i}.tmp`)
/// to avoid write conflicts. The winner's temp file is atomically renamed
/// to the final destination.
async fn download_file_race(
    client: &Arc<Client>,
    hf_repo: &str,
    remote_path: &str,
    dest: &Path,
    sources: &[String],
    progress: &DownloadProgress,
) -> Result<(), DownloadError> {
    let mut set = tokio::task::JoinSet::new();

    for (idx, base) in sources.iter().enumerate() {
        let url = hf_file_url(hf_repo, remote_path, base);
        let client = Arc::clone(client);
        let dest = dest.to_path_buf();
        // SAFETY: progress is borrowed from the caller and lives for the
        // duration of this function. We extend its lifetime for spawned
        // tasks by converting the reference to a raw pointer, then back
        // inside the task. This is safe because:
        // 1. The JoinSet is awaited to completion (or abort_all) before
        //    this function returns, so all tasks finish before `progress`
        //    is dropped.
        // 2. DownloadProgress uses atomic fields, so concurrent writes
        //    are safe.
        let progress_ptr = progress as *const DownloadProgress as usize;

        set.spawn(async move {
            // SAFETY: see comment above
            let progress = unsafe { &*(progress_ptr as *const DownloadProgress) };
            let result = download_file_with_retries(&client, &url, &dest, idx, progress).await;
            (idx, url, result)
        });
    }

    let total = sources.len();
    let mut last_err: Option<DownloadError> = None;

    while let Some(outcome) = set.join_next().await {
        match outcome {
            Ok((idx, url, Ok(()))) => {
                tracing::info!(
                    source = idx + 1,
                    total,
                    url = %url,
                    "Download race winner (source {})",
                    idx + 1
                );
                // Abort all remaining tasks — JoinSet drop also handles this
                set.abort_all();
                return Ok(());
            }
            Ok((idx, url, Err(e))) => {
                tracing::warn!(
                    source = idx + 1,
                    total,
                    url = %url,
                    error = %e,
                    "Source {}/{} failed in race",
                    idx + 1, total
                );
                last_err = Some(e);
            }
            Err(join_err) => {
                // Task was cancelled or panicked — only record if no other error yet
                if join_err.is_cancelled() && last_err.is_none() {
                    last_err = Some(DownloadError::Cancelled);
                } else if last_err.is_none() {
                    last_err = Some(DownloadError::InvalidUrl(
                        format!("Download task panicked: {}", join_err),
                    ));
                }
            }
        }
    }

    Err(last_err.unwrap_or_else(|| {
        DownloadError::InvalidUrl("No download sources available".to_string())
    }))
}

/// Download a single file with retry logic (used inside race tasks).
///
/// Retries up to 3 times on transient HTTP errors (5xx, timeout).
/// Each racer writes to a unique temp file (`{dest}_{idx}.tmp`) to
/// avoid write conflicts between concurrent racers.
async fn download_file_with_retries(
    client: &Client,
    url: &str,
    dest: &Path,
    idx: usize,
    progress: &DownloadProgress,
) -> Result<(), DownloadError> {
    const MAX_RETRIES: u32 = 3;
    const RETRY_DELAY_MS: u64 = 2000;

    let mut attempt = 0;
    loop {
        attempt += 1;
        match download_single(client, url, dest, idx, progress).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                let is_transient = match &e {
                    DownloadError::Http(req_err) => {
                        req_err.is_timeout()
                            || req_err.is_connect()
                            || req_err.status().is_some_and(|s| s.is_server_error())
                    }
                    _ => false,
                };

                if is_transient && attempt < MAX_RETRIES {
                    tracing::warn!(
                        url,
                        source = idx + 1,
                        attempt,
                        error = %e,
                        "Transient error, retrying..."
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(
                        RETRY_DELAY_MS * attempt as u64,
                    ))
                    .await;
                    continue;
                }
                return Err(e);
            }
        }
    }
}

/// Inner download implementation (single attempt, single source).
///
/// Uses streaming to avoid loading the entire file into memory.
/// Writes to a per-source temp file (`{dest}_{idx}.tmp`), then
/// atomically renames to the final destination.
async fn download_single(
    client: &Client,
    url: &str,
    dest: &Path,
    idx: usize,
    progress: &DownloadProgress,
) -> Result<(), DownloadError> {
    let response = client
        .get(url)
        .send()
        .await?
        .error_for_status()
        .map_err(DownloadError::Http)?;

    let total_size = response.content_length().unwrap_or(0);
    progress.total_bytes.fetch_max(total_size, Ordering::Relaxed);
    tracing::info!(size = total_size, source = idx + 1, url, "Downloading from source {}", idx + 1);

    // Per-source temp file to avoid write conflicts between racers
    let stem = dest
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("download");
    let tmp_path = dest.with_file_name(format!("{}_{}.tmp", stem, idx));

    let mut file = tokio::io::BufWriter::new(tokio::fs::File::create(&tmp_path).await?);

    let mut downloaded: u64 = 0;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(DownloadError::Http)?;
        downloaded += chunk.len() as u64;
        progress.bytes_downloaded.fetch_max(downloaded, Ordering::Relaxed);
        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
    }
    tokio::io::AsyncWriteExt::flush(&mut file).await?;
    drop(file);

    // Rename temp → final destination
    if let Err(rename_err) = rename_or_replace(&tmp_path, dest) {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(DownloadError::Io(rename_err));
    }

    Ok(())
}
