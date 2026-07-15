//! Chat commands

use tauri::State;

use crate::gateway_client::DocumentUploadResponse;
use crate::state::AppState;

/// Upload a document to a session (multipart POST)
#[tauri::command]
pub async fn upload_document(
    state: State<'_, AppState>,
    session_id: String,
    file_path: String,
) -> Result<DocumentUploadResponse, String> {
    let client = state.gateway.read().await;
    client
        .upload_document(&session_id, &file_path)
        .await
        .map_err(|e| e.to_string())
}
