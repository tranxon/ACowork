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
