//! Session document upload API handlers.
//!
//! Handles document file uploads scoped to a session:
//! - POST /api/sessions/{session_id}/documents — multipart upload
//! - GET  /api/sessions/{session_id}/documents — list uploaded documents
//! - DELETE /api/sessions/{session_id}/documents/{doc_id} — delete a document
//!
//! Files are stored under `{data_dir}/sessions/{session_id}/documents/`
//! and metadata is persisted alongside as `{filename}.meta.json`.

use axum::{
    extract::{Multipart, Path, State},
    http::StatusCode,
    Json,
    routing::{delete, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::http::routes::{ApiError, AppState};

/// Maximum file size for document upload (50 MB).
const MAX_UPLOAD_SIZE_BYTES: u64 = 50 * 1024 * 1024;

/// Build the document management router.
pub fn documents_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/sessions/{session_id}/documents",
            post(upload_document).get(list_documents),
        )
        .route(
            "/api/sessions/{session_id}/documents/{doc_id}",
            delete(delete_document),
        )
}

// ── Request / Response types ──────────────────────────────────────────

/// Response after a successful document upload.
#[derive(Serialize)]
pub struct DocumentUploadResponse {
    /// Unique document identifier (derived from filename).
    pub document_id: String,
    /// Original filename.
    pub filename: String,
    /// Detected format: "pdf", "docx", "pptx", "xlsx".
    pub format: String,
    /// File size in bytes.
    pub size_bytes: u64,
}

/// A document entry in the list response.
#[derive(Serialize)]
pub struct DocumentEntry {
    pub document_id: String,
    pub filename: String,
    pub format: String,
    pub size_bytes: u64,
}

/// List of documents for a session.
#[derive(Serialize)]
pub struct DocumentListResponse {
    pub session_id: String,
    pub documents: Vec<DocumentEntry>,
}

/// Per-document metadata stored alongside the uploaded file.
#[derive(Serialize, Deserialize)]
struct DocumentMeta {
    pub document_id: String,
    pub filename: String,
    pub format: String,
    pub size_bytes: u64,
    pub abs_path: String,
}

// ── Helpers ───────────────────────────────────────────────────────────

/// Resolve the session documents directory from the gateway data dir.
fn session_docs_dir(data_dir: &std::path::Path, session_id: &str) -> PathBuf {
    data_dir.join("sessions").join(session_id).join("documents")
}

/// Detect document format from a filename extension.
fn detect_format(filename: &str) -> Option<&'static str> {
    let lower = filename.to_lowercase();
    if lower.ends_with(".pdf") {
        Some("pdf")
    } else if lower.ends_with(".docx") {
        Some("docx")
    } else if lower.ends_with(".pptx") {
        Some("pptx")
    } else if lower.ends_with(".xlsx") {
        Some("xlsx")
    } else {
        None
    }
}

/// Sanitize a filename — keep only the base name, strip path separators.
fn safe_filename(raw: &str) -> String {
    let name = std::path::Path::new(raw)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(raw);
    // Replace any path separators and null bytes
    name.replace(['\\', '/', '\0'], "_")
}

/// Derive a document_id from the filename (stem only, no extension).
fn doc_id_from_filename(filename: &str) -> String {
    let path = std::path::Path::new(filename);
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(filename)
        .to_string()
}

/// Extract the Gateway data_dir from AppState.
fn get_data_dir(state: &AppState) -> PathBuf {
    state
        .gateway_state
        .try_read()
        .ok()
        .and_then(|gw| gw.config.as_ref().map(|c| PathBuf::from(&c.data_dir)))
        .unwrap_or_else(|| PathBuf::from("./data"))
}

// ── Handlers ──────────────────────────────────────────────────────────

/// `POST /api/sessions/{session_id}/documents` — upload a document.
///
/// Accepts `multipart/form-data` with:
/// - `file`: the document binary (required)
/// - `filename`: original filename (optional, extracted from the file field)
///
/// The file is saved to `{data_dir}/sessions/{session_id}/documents/{safe_name}`.
/// Metadata is persisted alongside as `{safe_name}.meta.json`.
pub async fn upload_document(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<DocumentUploadResponse>), (StatusCode, Json<ApiError>)> {
    let mut file_bytes: Option<Vec<u8>> = None;
    let mut original_filename: Option<String> = None;

    while let Some(field) = multipart.next_field().await.map_err(|e| {
        ApiError::bad_request(&format!("Failed to read multipart field: {e}"))
    })? {
        let name = field.name().unwrap_or_default().to_string();
        match name.as_str() {
            "file" => {
                let maybe_name = field.file_name().map(|n| n.to_string());
                let bytes = field.bytes().await.map_err(|e| {
                    ApiError::bad_request(&format!("Failed to read file field: {e}"))
                })?;
                file_bytes = Some(bytes.to_vec());
                original_filename = maybe_name;
            }
            "filename" => {
                let text = field.text().await.unwrap_or_default();
                if !text.is_empty() {
                    original_filename = Some(text);
                }
            }
            _ => {} // ignore unknown fields
        }
    }

    let file_bytes = file_bytes
        .ok_or_else(|| ApiError::bad_request("Missing required field: 'file'"))?;

    if file_bytes.is_empty() {
        return Err(ApiError::bad_request("File is empty"));
    }

    if file_bytes.len() as u64 > MAX_UPLOAD_SIZE_BYTES {
        return Err(ApiError::bad_request(&format!(
            "File too large: {} bytes (limit: {MAX_UPLOAD_SIZE_BYTES} bytes)",
            file_bytes.len()
        )));
    }

    let filename = original_filename
        .as_deref()
        .unwrap_or("document.bin");

    let safe_name = safe_filename(filename);
    let format = detect_format(&safe_name)
        .ok_or_else(|| {
            ApiError::bad_request(&format!(
                "Unsupported document format: '{}'. Supported: pdf, docx, pptx, xlsx",
                safe_name
            ))
        })?
        .to_string();

    let doc_id = doc_id_from_filename(&safe_name);
    let data_dir = get_data_dir(&state);
    let docs_dir = session_docs_dir(&data_dir, &session_id);

    // Create the session documents directory
    std::fs::create_dir_all(&docs_dir).map_err(|e| {
        ApiError::internal(&format!("Failed to create documents directory: {e}"))
    })?;

    let abs_path = docs_dir.join(&safe_name);

    // Write the file
    std::fs::write(&abs_path, &file_bytes).map_err(|e| {
        ApiError::internal(&format!("Failed to write document file: {e}"))
    })?;

    let size_bytes = file_bytes.len() as u64;

    // Persist metadata
    let meta = DocumentMeta {
        document_id: doc_id.clone(),
        filename: safe_name.clone(),
        format: format.clone(),
        size_bytes,
        abs_path: abs_path.to_string_lossy().to_string(),
    };
    let meta_path = docs_dir.join(format!("{safe_name}.meta.json"));
    let meta_json = serde_json::to_string(&meta).map_err(|e| {
        ApiError::internal(&format!("Failed to serialize metadata: {e}"))
    })?;
    std::fs::write(&meta_path, meta_json).map_err(|e| {
        ApiError::internal(&format!("Failed to write metadata file: {e}"))
    })?;

    tracing::info!(
        session_id = %session_id,
        doc_id = %doc_id,
        filename = %safe_name,
        format = %format,
        size = size_bytes,
        "Document uploaded"
    );

    Ok((
        StatusCode::OK,
        Json(DocumentUploadResponse {
            document_id: doc_id,
            filename: safe_name,
            format,
            size_bytes,
        }),
    ))
}

/// `GET /api/sessions/{session_id}/documents` — list uploaded documents.
pub async fn list_documents(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Json<DocumentListResponse>, (StatusCode, Json<ApiError>)> {
    let data_dir = get_data_dir(&state);
    let docs_dir = session_docs_dir(&data_dir, &session_id);

    if !docs_dir.exists() {
        return Ok(Json(DocumentListResponse {
            session_id,
            documents: vec![],
        }));
    }

    let mut documents = Vec::new();
    let entries = std::fs::read_dir(&docs_dir).map_err(|e| {
        ApiError::internal(&format!("Failed to read documents directory: {e}"))
    })?;

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        if ext == "json" {
            continue; // skip metadata files
        }

        let Some(filename) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        let format = detect_format(filename).unwrap_or("unknown");
        let size_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let doc_id = doc_id_from_filename(filename);

        documents.push(DocumentEntry {
            document_id: doc_id,
            filename: filename.to_string(),
            format: format.to_string(),
            size_bytes,
        });
    }

    Ok(Json(DocumentListResponse {
        session_id,
        documents,
    }))
}

/// `DELETE /api/sessions/{session_id}/documents/{doc_id}` — delete a document.
pub async fn delete_document(
    State(state): State<AppState>,
    Path((session_id, doc_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ApiError>)> {
    let data_dir = get_data_dir(&state);
    let docs_dir = session_docs_dir(&data_dir, &session_id);

    if !docs_dir.exists() {
        return Err(ApiError::not_found("Session documents directory not found"));
    }

    // Find the file matching the doc_id (stem matches, any extension)
    let mut found = false;
    let entries = std::fs::read_dir(&docs_dir).map_err(|e| {
        ApiError::internal(&format!("Failed to read documents directory: {e}"))
    })?;

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if stem != doc_id {
            continue;
        }

        // Delete the document file
        let _ = std::fs::remove_file(&path);

        // Delete the metadata file
        let meta_path = path.with_extension("meta.json");
        let _ = std::fs::remove_file(&meta_path);

        found = true;
        tracing::info!(
            session_id = %session_id,
            doc_id = %doc_id,
            "Document deleted"
        );
        break;
    }

    if !found {
        return Err(ApiError::not_found(&format!(
            "Document '{}' not found in session '{}'",
            doc_id, session_id
        )));
    }

    Ok(Json(serde_json::json!({ "deleted": true, "document_id": doc_id })))
}
