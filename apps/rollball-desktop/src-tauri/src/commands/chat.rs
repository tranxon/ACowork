//! Chat commands

use tauri::State;

use crate::gateway_client::{DocumentUploadResponse, SendMessageResponse};
use crate::state::AppState;

/// Send a message to an agent (HTTP POST, non-streaming)
#[tauri::command]
pub async fn send_message(
    state: State<'_, AppState>,
    agent_id: String,
    content: String,
    session_id: Option<String>,
    command: Option<String>,
    document_ids: Option<Vec<String>>,
) -> Result<SendMessageResponse, String> {
    let client = state.gateway.read().await;
    client.send_message(&agent_id, &content, session_id.as_deref(), command.as_deref(), document_ids.as_deref()).await.map_err(|e| e.to_string())
}

/// Upload a document to a session (multipart POST)
#[tauri::command]
pub async fn upload_document(
    state: State<'_, AppState>,
    session_id: String,
    file_path: String,
) -> Result<DocumentUploadResponse, String> {
    let client = state.gateway.read().await;
    client.upload_document(&session_id, &file_path).await.map_err(|e| e.to_string())
}
