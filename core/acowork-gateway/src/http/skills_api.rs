//! Skill import HTTP API handler (ADR-009 §V-A).
//!
//! Exactly one endpoint lives here now:
//! - `POST /api/agents/{id}/skills/import` — import a skill ZIP package
//!
//! The three read endpoints (`GET .../skills`, `.../skills/{name}`,
//! `.../skills/{name}/history`) used to be implemented in this module by a
//! Gateway-side "Minimal SKILL.md parser" reading `{install_path}/skills/`.
//! That was an ADR-009 violation: `install_path` is **Runtime-private and
//! node-local** (ADR-055), so the second parser (a) drifted from the Runtime's
//! real parser and (b) would 5xx outright once Gateway and Runtime lived on
//! different machines. Those reads are now reverse-proxied to the Runtime
//! (`http/proxy.rs` → Runtime `http/skills.rs`), which owns the only parser.
//!
//! Import is different: it never reads Runtime-private state. It spools the
//! upload to a Gateway-local temp file and delegates extraction to the local
//! Node control plane (ADR-055 §6.2), which writes into the node-local package
//! — already correct across machines, so it stays.

use axum::{
    Json, Router,
    extract::{Multipart, Path, State},
    http::StatusCode,
    routing::post,
};
use serde::Serialize;

use crate::http::routes::{ApiError, AppState};

/// Build the skill import router.
pub fn skills_routes() -> Router<AppState> {
    Router::new().route("/api/agents/{id}/skills/import", post(import_skill))
}

/// Response body for skill import result
#[derive(Serialize)]
pub struct ImportSkillResponse {
    pub success: bool,
    pub skill_name: String,
    pub message: String,
}

/// Simple nanosecond timestamp for unique temp filenames
fn timestamp_nanos() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// `POST /api/agents/{id}/skills/import` — import a skill from a ZIP package
///
/// Accepts `multipart/form-data` with a `package` field containing the skill
/// ZIP bytes. The ZIP must contain a `SKILL.md` (YAML frontmatter) either at
/// root or inside a single top-level directory; the node extracts it to the
/// agent's `skills/{skill_name}/` directory, where `skill_name` comes from the
/// SKILL.md frontmatter's `name` field.
pub async fn import_skill(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<ImportSkillResponse>), ApiError> {
    // Parse multipart fields
    let mut package_bytes: Option<Vec<u8>> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::bad_request(&format!("Failed to read multipart field: {}", e)))?
    {
        // Only the `package` field matters; anything else (e.g. the legacy
        // `overwrite` flag) is ignored.
        if field.name() == Some("package") {
            let bytes = field.bytes().await.map_err(|e| {
                ApiError::bad_request(&format!("Failed to read package field: {}", e))
            })?;
            package_bytes = Some(bytes.to_vec());
        }
    }

    let package_bytes =
        package_bytes.ok_or_else(|| ApiError::bad_request("Missing required field: 'package'"))?;

    if package_bytes.is_empty() {
        return Err(ApiError::bad_request("Package file is empty"));
    }

    // Verify agent exists
    {
        let gw = state.gateway_state.read().await;
        if !gw.installed_agents.contains_key(&agent_id) {
            return Err(ApiError::not_found(&format!("Agent not found: {}", agent_id)));
        }
    }

    // Spool to a Gateway-local temp file, then delegate the extraction to the
    // local node (ADR-055 §6.2). The node extracts into the agent's skills/
    // dir and reports the skill name in its reply.
    let temp_file = std::env::temp_dir().join(format!(
        "acowork-skill-{}-{}.zip",
        std::process::id(),
        timestamp_nanos(),
    ));
    if let Err(e) = std::fs::write(&temp_file, &package_bytes) {
        return Err(ApiError::internal(&format!(
            "Failed to write upload to temp file: {}",
            e
        )));
    }

    let node_control = state
        .node_control
        .clone()
        .ok_or_else(|| ApiError::internal("Node control plane unavailable (MQTT disabled)"))?;
    // ADR-073: resolve the route variable to the instance identity.
    let (instance_id, resolved_agent_id) =
        crate::http::agents::resolve_agent_identity(&state, &agent_id).await?;
    let zip_path = temp_file.to_string_lossy().to_string();
    let event = node_control
        .skills_import(
            &acowork_core::node::local_node_id(),
            &instance_id,
            &resolved_agent_id,
            &zip_path,
        )
        .await
        .map_err(|e| {
            let _ = std::fs::remove_file(&temp_file);
            ApiError::internal(&format!("Skill import failed: {}", e))
        })?;
    let _ = std::fs::remove_file(&temp_file);
    crate::mqtt::node_control::NodeControlClient::check_reply(&instance_id, &event)
        .map_err(|e| ApiError::internal(&format!("Skill import failed: {}", e)))?;

    // The node reply carries "skill '{name}' imported".
    let skill_name = event
        .message
        .strip_prefix("skill '")
        .and_then(|s| s.strip_suffix("' imported"))
        .unwrap_or("")
        .to_string();

    Ok((
        StatusCode::CREATED,
        Json(ImportSkillResponse {
            success: true,
            skill_name: skill_name.clone(),
            message: format!("Skill '{}' imported successfully", skill_name),
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_import_skill_response_serialization() {
        let resp = ImportSkillResponse {
            success: true,
            skill_name: "weekly-report".to_string(),
            message: "Skill 'weekly-report' imported successfully".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"success\":true"));
        assert!(json.contains("weekly-report"));
    }
}
