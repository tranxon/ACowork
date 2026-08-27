//! Skill management HTTP API handlers
//!
//! Implements the Skill API endpoints for agent skill inspection and import:
//! - GET    /api/agents/{id}/skills              — list skills for an agent
//! - GET    /api/agents/{id}/skills/{name}       — get skill detail
//! - GET    /api/agents/{id}/skills/{name}/history — get skill execution history
//! - POST   /api/agents/{id}/skills/import        — import a skill ZIP package
//!
//! Skills are loaded from the installed agent package's `skills/` directory.
//! Each skill is defined by a SKILL.md file (YAML frontmatter + Markdown body).
//!
//! Skill import follows the same pattern as agent installation:
//! multipart upload → spool to temp file → extract ZIP to agent's skills/ dir.

use axum::{
    Json, Router,
    extract::{Multipart, Path, Query, State},
    http::StatusCode,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path as StdPath, PathBuf};

use crate::http::routes::{ApiError, AppState};

/// Build the skill management router
pub fn skills_routes() -> Router<AppState> {
    Router::new()
        .route("/api/agents/{id}/skills", get(list_skills))
        .route("/api/agents/{id}/skills/{name}", get(get_skill_detail))
        .route(
            "/api/agents/{id}/skills/{name}/history",
            get(get_skill_history),
        )
        .route("/api/agents/{id}/skills/import", post(import_skill))
}

// ── Query parameters ──────────────────────────────────────────────────

/// Query parameters for listing skills
#[derive(Debug, Deserialize)]
pub struct SkillListQuery {
    /// Page number (1-based, default: 1)
    pub page: Option<u32>,
    /// Page size (default: 20, max: 100)
    pub size: Option<u32>,
}

impl SkillListQuery {
    /// Get the effective page number (1-based)
    pub fn effective_page(&self) -> u32 {
        self.page.unwrap_or(1).max(1)
    }

    /// Get the effective page size (capped at 100)
    pub fn effective_size(&self) -> u32 {
        self.size.unwrap_or(20).clamp(1, 100)
    }
}

// ── Response types ────────────────────────────────────────────────────

/// A single skill entry in the list response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillListEntry {
    pub name: String,
    pub description: String,
    pub version: Option<String>,
    pub author: Option<String>,
    pub triggers: Vec<String>,
    pub tool_deps: Vec<String>,
}

/// Paginated list of skills
#[derive(Serialize)]
pub struct SkillListResponse {
    pub total: u64,
    pub page: u32,
    pub size: u32,
    pub skills: Vec<SkillListEntry>,
}

/// Detailed skill information
#[derive(Debug, Clone, Serialize)]
pub struct SkillDetailResponse {
    pub name: String,
    pub description: String,
    pub version: Option<String>,
    pub author: Option<String>,
    pub triggers: Vec<String>,
    pub tool_deps: Vec<String>,
    /// Full Markdown instructions body
    pub instructions: String,
}

/// Skill execution history (stub for current phase)
#[derive(Serialize)]
pub struct SkillExecutionHistoryResponse {
    pub skill_name: String,
    pub total_executions: u64,
    pub page: u32,
    pub size: u32,
    pub executions: Vec<serde_json::Value>,
}

// ── Minimal SKILL.md parser ───────────────────────────────────────────
// Parses YAML frontmatter + Markdown body from SKILL.md files.
// This is a lightweight implementation to avoid depending on acowork-runtime.

/// Parsed SKILL.md frontmatter (YAML section)
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SkillFrontmatter {
    name: String,
    description: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    triggers: Vec<String>,
    #[serde(default)]
    tool_deps: Vec<String>,
}

/// Parsed SKILL.md with frontmatter and instructions body
#[derive(Debug, Clone)]
struct ParsedSkill {
    entry: SkillListEntry,
    instructions: String,
}

/// Parse a SKILL.md content string into a ParsedSkill
fn parse_skill_md(content: &str) -> Option<ParsedSkill> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }

    let rest = trimmed.strip_prefix("---")?;
    let end_pos = rest.find("\n---")?;
    let frontmatter_str = &rest[..end_pos];
    let body = &rest[end_pos + 4..]; // skip \n---
    let instructions = body.trim().to_string();

    let frontmatter: SkillFrontmatter = serde_yaml::from_str(frontmatter_str).ok()?;

    Some(ParsedSkill {
        entry: SkillListEntry {
            name: frontmatter.name,
            description: frontmatter.description,
            version: frontmatter.version,
            author: frontmatter.author,
            triggers: frontmatter.triggers,
            tool_deps: frontmatter.tool_deps,
        },
        instructions,
    })
}

/// Load all skills from an agent's `skills/` directory
fn load_skills_from_dir(skills_dir: &StdPath) -> HashMap<String, ParsedSkill> {
    let mut skills = HashMap::new();

    let Ok(entries) = std::fs::read_dir(skills_dir) else {
        return skills;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let skill_md = path.join("SKILL.md");
            if skill_md.exists()
                && let Ok(content) = std::fs::read_to_string(&skill_md)
                && let Some(parsed) = parse_skill_md(&content)
            {
                skills.insert(parsed.entry.name.clone(), parsed);
            }
        }
    }

    skills
}

/// Resolve the skills directory for an installed agent
fn agent_skills_dir(install_path: &str) -> PathBuf {
    PathBuf::from(install_path).join("skills")
}

// ── Import skill types ─────────────────────────────────────────────────

/// Response body for skill import result
#[derive(Serialize)]
pub struct ImportSkillResponse {
    pub success: bool,
    pub skill_name: String,
    pub message: String,
}

// ── Import helpers ────────────────────────────────────────────────────
// (skill-package extraction moved to the node — ADR-055 §6.2)

/// Simple nanosecond timestamp for unique temp filenames
fn timestamp_nanos() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

// ── Handlers ──────────────────────────────────────────────────────────

/// `GET /api/agents/{id}/skills` — list skills for an agent
pub async fn list_skills(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Query(query): Query<SkillListQuery>,
) -> Result<Json<SkillListResponse>, (StatusCode, Json<ApiError>)> {
    let gw = state.gateway_state.read().await;

    // Verify agent exists and get install path
    let info = gw
        .installed_agents
        .get(&agent_id)
        .ok_or_else(|| ApiError::not_found(&format!("Agent not found: {}", agent_id)))?;

    let skills_dir = agent_skills_dir(&info.install_path);
    drop(gw);

    let page = query.effective_page();
    let size = query.effective_size();

    if !skills_dir.exists() {
        return Ok(Json(SkillListResponse {
            total: 0,
            page,
            size,
            skills: vec![],
        }));
    }

    let skills = load_skills_from_dir(&skills_dir);
    let total = skills.len() as u64;

    // Paginate
    let skip = ((page - 1) * size) as usize;
    let skill_list: Vec<SkillListEntry> = skills
        .into_values()
        .skip(skip)
        .take(size as usize)
        .map(|s| s.entry)
        .collect();

    Ok(Json(SkillListResponse {
        total,
        page,
        size,
        skills: skill_list,
    }))
}

/// `GET /api/agents/{id}/skills/{name}` — get skill detail
pub async fn get_skill_detail(
    State(state): State<AppState>,
    Path((agent_id, skill_name)): Path<(String, String)>,
) -> Result<Json<SkillDetailResponse>, (StatusCode, Json<ApiError>)> {
    let gw = state.gateway_state.read().await;

    // Verify agent exists and get install path
    let info = gw
        .installed_agents
        .get(&agent_id)
        .ok_or_else(|| ApiError::not_found(&format!("Agent not found: {}", agent_id)))?;

    let skills_dir = agent_skills_dir(&info.install_path);
    drop(gw);

    if !skills_dir.exists() {
        return Err(ApiError::not_found(&format!(
            "Skill not found: {}",
            skill_name
        )));
    }

    let skills = load_skills_from_dir(&skills_dir);
    let parsed = skills
        .get(&skill_name)
        .ok_or_else(|| ApiError::not_found(&format!("Skill not found: {}", skill_name)))?;

    Ok(Json(SkillDetailResponse {
        name: parsed.entry.name.clone(),
        description: parsed.entry.description.clone(),
        version: parsed.entry.version.clone(),
        author: parsed.entry.author.clone(),
        triggers: parsed.entry.triggers.clone(),
        tool_deps: parsed.entry.tool_deps.clone(),
        instructions: parsed.instructions.clone(),
    }))
}

/// `POST /api/agents/{id}/skills/import` — import a skill from a ZIP package
///
/// Accepts `multipart/form-data` with a `package` field containing the
/// skill ZIP file bytes. The ZIP must contain a `SKILL.md` file (with YAML
/// frontmatter) either at root or inside a single top-level directory.
///
/// The ZIP is extracted to the agent's `skills/{skill_name}/` directory,
/// where `skill_name` comes from the SKILL.md frontmatter's `name` field.
///
/// Follows the same pattern as agent installation: multipart upload →
/// spool to temp file → extract ZIP → cleanup.
pub async fn import_skill(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<ImportSkillResponse>), (StatusCode, Json<ApiError>)> {
    // Parse multipart fields
    let mut package_bytes: Option<Vec<u8>> = None;
    let mut overwrite: Option<bool> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::bad_request(&format!("Failed to read multipart field: {}", e)))?
    {
        let name = field.name().unwrap_or_default().to_string();
        match name.as_str() {
            "package" => {
                let bytes = field.bytes().await.map_err(|e| {
                    ApiError::bad_request(&format!("Failed to read package field: {}", e))
                })?;
                package_bytes = Some(bytes.to_vec());
            }
            "overwrite" => {
                let text = field.text().await.unwrap_or_default();
                overwrite = Some(text == "true" || text == "1");
            }
            _ => {} // ignore unknown fields
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

    let _overwrite = overwrite.unwrap_or(false);

    // Spool to a node-local temp file, then delegate the import to the
    // local node (ADR-055 §6.2). The node extracts into the agent's
    // skills/ dir and reports the skill name in its reply.
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

    let node_control = state.node_control.clone().ok_or_else(|| {
        ApiError::internal("Node control plane unavailable (MQTT disabled)")
    })?;
    let zip_path = temp_file.to_string_lossy().to_string();
    let event = node_control
        .skills_import(acowork_core::node::LOCAL_NODE_ID, &agent_id, &zip_path)
        .await
        .map_err(|e| {
            let _ = std::fs::remove_file(&temp_file);
            ApiError::internal(&format!("Skill import failed: {}", e))
        })?;
    let _ = std::fs::remove_file(&temp_file);
    crate::mqtt::node_control::NodeControlClient::check_reply(&agent_id, &event)
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

/// `GET /api/agents/{id}/skills/{name}/history` — get skill execution history
///
/// Current phase: returns empty history (execution tracking is future work).
pub async fn get_skill_history(
    State(state): State<AppState>,
    Path((agent_id, skill_name)): Path<(String, String)>,
    Query(query): Query<SkillListQuery>,
) -> Result<Json<SkillExecutionHistoryResponse>, (StatusCode, Json<ApiError>)> {
    let gw = state.gateway_state.read().await;

    // Verify agent exists
    if !gw.is_installed(&agent_id) {
        return Err(ApiError::not_found(&format!(
            "Agent not found: {}",
            agent_id
        )));
    }
    drop(gw);

    // Execution history tracking is not yet implemented.
    // Return empty history for now.
    Ok(Json(SkillExecutionHistoryResponse {
        skill_name,
        total_executions: 0,
        page: query.effective_page(),
        size: query.effective_size(),
        executions: vec![],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_skill_md_basic() {
        let content = r#"---
name: weekly-report
description: Generate weekly report
version: "1.0.0"
author: developer
triggers:
  - weekly report
  - 周报
tool_deps:
  - memory_recall
  - file_write
---

# Weekly Report Skill

1. Recall this week's work...
"#;
        let parsed = parse_skill_md(content).unwrap();
        assert_eq!(parsed.entry.name, "weekly-report");
        assert_eq!(parsed.entry.description, "Generate weekly report");
        assert_eq!(parsed.entry.version, Some("1.0.0".to_string()));
        assert_eq!(parsed.entry.triggers.len(), 2);
        assert_eq!(parsed.entry.tool_deps.len(), 2);
        assert!(parsed.instructions.contains("Weekly Report Skill"));
    }

    #[test]
    fn test_parse_skill_md_minimal() {
        let content = r#"---
name: hello
description: Say hello
triggers:
  - hi
---

Hello!
"#;
        let parsed = parse_skill_md(content).unwrap();
        assert_eq!(parsed.entry.name, "hello");
        assert!(parsed.entry.version.is_none());
        assert!(parsed.entry.tool_deps.is_empty());
        assert_eq!(parsed.instructions, "Hello!");
    }

    #[test]
    fn test_parse_skill_md_no_frontmatter() {
        let content = "No frontmatter here";
        assert!(parse_skill_md(content).is_none());
    }

    #[test]
    fn test_parse_skill_md_invalid_yaml() {
        let content = r#"---
name: [invalid yaml
description: Test
triggers:
  - test
---

Body
"#;
        assert!(parse_skill_md(content).is_none());
    }

    #[test]
    fn test_skill_list_query_defaults() {
        let query = SkillListQuery {
            page: None,
            size: None,
        };
        assert_eq!(query.effective_page(), 1);
        assert_eq!(query.effective_size(), 20);
    }

    #[test]
    fn test_skill_list_query_capped() {
        let query = SkillListQuery {
            page: Some(0),
            size: Some(200),
        };
        assert_eq!(query.effective_page(), 1);
        assert_eq!(query.effective_size(), 100);
    }

    #[test]
    fn test_skill_list_entry_serialization() {
        let entry = SkillListEntry {
            name: "deploy".to_string(),
            description: "Deploy service".to_string(),
            version: Some("2.0.0".to_string()),
            author: None,
            triggers: vec!["deploy".to_string()],
            tool_deps: vec![],
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"deploy\""));
        assert!(json.contains("\"2.0.0\""));
    }

    #[test]
    fn test_skill_detail_response_serialization() {
        let resp = SkillDetailResponse {
            name: "test".to_string(),
            description: "Test skill".to_string(),
            version: None,
            author: Some("developer".to_string()),
            triggers: vec!["test".to_string()],
            tool_deps: vec!["tool1".to_string()],
            instructions: "# Instructions\nDo stuff".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"instructions\""));
        assert!(json.contains("\"developer\""));
    }

    #[test]
    fn test_agent_skills_dir() {
        let dir = agent_skills_dir("/tmp/weather-agent-1.0.0");
        assert_eq!(dir, PathBuf::from("/tmp/weather-agent-1.0.0/skills"));
    }

    #[test]
    fn test_load_skills_from_nonexistent_dir() {
        let skills = load_skills_from_dir(StdPath::new("/nonexistent/path"));
        assert!(skills.is_empty());
    }

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
