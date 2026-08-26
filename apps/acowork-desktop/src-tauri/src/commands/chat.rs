//! Chat commands

use tauri::State;

use crate::gateway_client::FileUploadResponse;
use crate::state::AppState;

/// Upload a single attachment (document or image) to a session.
///
/// ADR-046: replaces the legacy `upload_document` Tauri command which only
/// handled PDF/DOCX/PPTX/XLSX. The same code path now serves images too — the
/// frontend distinguishes them via the `format` field and supplies optional
/// `width` / `height` for images that the desktop measured via `new Image()`.
///
/// Reaches the runtime via `POST /api/agents/{agent_id}/sessions/{sid}/files`.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn upload_file(
    state: State<'_, AppState>,
    agent_id: String,
    session_id: String,
    file_path: String,
    format: String,
    width: Option<u32>,
    height: Option<u32>,
) -> Result<FileUploadResponse, String> {
    let client = state.gateway.read().await;
    client
        .upload_file(&agent_id, &session_id, &file_path, &format, width, height)
        .await
        .map_err(|e| e.to_string())
}

/// Return the size of a file on disk in bytes.
///
/// Used by the chat panel as a pre-flight check before
/// `upload_file` — the runtime enforces a 50 MiB cap (see
/// `acowork-runtime::usecases::MAX_UPLOAD_BYTES`) and surfaces the
/// rejection as HTTP 413 *after* a full multipart upload. We
/// short-circuit oversized files here so the user gets an instant
/// toast instead of waiting for the encoding roundtrip.
///
/// Returns an error if the path does not exist or is not a regular
/// file (mirrors the existence check inside `upload_file` itself so
/// the two paths stay consistent).
#[tauri::command]
pub async fn get_file_size(file_path: String) -> Result<u64, String> {
    let path = std::path::Path::new(&file_path);
    let metadata = std::fs::metadata(path)
        .map_err(|e| format!("Cannot read file metadata: {}", e))?;
    if !metadata.is_file() {
        return Err(format!("Not a regular file: {}", file_path));
    }
    Ok(metadata.len())
}
