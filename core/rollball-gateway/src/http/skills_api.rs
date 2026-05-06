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
    extract::{Multipart, Path, Query, State},
    http::StatusCode,
    Json,
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path as StdPath, PathBuf};

use crate::error::GatewayError;
use crate::http::routes::{ApiError, AppState};

/// Build the skill management router
pub fn skills_routes() -> Router<AppState> {
    Router::new()
        .route("/api/agents/{id}/skills", get(list_skills))
        .route("/api/agents/{id}/skills/{name}", get(get_skill_detail))
        .route("/api/agents/{id}/skills/{name}/history", get(get_skill_history))
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
// This is a lightweight implementation to avoid depending on rollball-runtime.

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

/// Extract a skill ZIP package to the agent's skills directory.
///
/// Validates the ZIP contains a SKILL.md at its root (or in a single top-level
/// directory), parses the frontmatter to get the skill name, then extracts
/// all files to `{install_path}/skills/{skill_name}/`.
///
/// Security: uses `enclosed_name()` to prevent Zip Slip path traversal.
fn install_skill_package(
    package_path: &StdPath,
    skills_dir: &StdPath,
) -> Result<String, GatewayError> {
    // 1. Read and open ZIP
    let data = std::fs::read(package_path)
        .map_err(|e| GatewayError::Package(format!(
            "Failed to read skill package '{}': {}", package_path.display(), e
        )))?;
    let reader = std::io::Cursor::new(data);
    let mut archive = zip::ZipArchive::new(reader)
        .map_err(|e| GatewayError::Package(format!(
            "Failed to read skill ZIP '{}': {}", package_path.display(), e
        )))?;

    // 2. Locate SKILL.md — it may be at root or inside a single top-level directory
    let skill_md_content = extract_skill_md(&mut archive)?;

    // 3. Parse SKILL.md frontmatter to extract skill name
    let parsed = parse_skill_md(&skill_md_content)
        .ok_or_else(|| GatewayError::Package(
            "Invalid SKILL.md format: missing or malformed YAML frontmatter".to_string()
        ))?;
    let skill_name = parsed.entry.name;

    // 4. Ensure the agent's skills directory exists
    std::fs::create_dir_all(skills_dir)
        .map_err(|e| GatewayError::Package(format!(
            "Failed to create skills directory: {}", e
        )))?;

    // 5. Check if a skill with the same name already exists
    let target_skill_dir = skills_dir.join(&skill_name);
    if target_skill_dir.exists() {
        return Err(GatewayError::Package(format!(
            "Skill '{}' already exists (will not overwrite)", skill_name
        )));
    }

    // 6. Create the target skill directory
    std::fs::create_dir_all(&target_skill_dir)
        .map_err(|e| GatewayError::Package(format!(
            "Failed to create skill directory '{}': {}", target_skill_dir.display(), e
        )))?;

    // 7. Extract all files to the target skill directory
    //    If the ZIP has a single top-level directory prefix, strip it.
    let top_dir_name = detect_top_level_dir(&mut archive);
    for i in 0..archive.len() {
        let mut file = archive.by_index(i)
            .map_err(|e| GatewayError::Package(format!("ZIP read error: {}", e)))?;

        let raw_path = match file.enclosed_name() {
            Some(p) => p,
            None => continue, // skip unsafe paths (zip-slip protection)
        };

        // Strip the top-level directory prefix if present
        let relative_path = match &top_dir_name {
            Some(prefix) => {
                // Try stripping the prefix component
                match raw_path.strip_prefix(prefix) {
                    Ok(stripped) => stripped,
                    Err(_) => &raw_path,
                }
            }
            None => &raw_path,
        };

        // Skip empty paths (the top-level directory entry itself)
        if relative_path.as_os_str().is_empty() {
            continue;
        }

        let outpath = target_skill_dir.join(relative_path);

        if file.is_dir() {
            std::fs::create_dir_all(&outpath).ok();
        } else {
            if let Some(p) = outpath.parent() {
                if !p.exists() {
                    std::fs::create_dir_all(p).ok();
                }
            }
            let mut outfile = std::fs::File::create(&outpath)
                .map_err(|e| GatewayError::Package(format!(
                    "Failed to create file '{}': {}", outpath.display(), e
                )))?;
            std::io::copy(&mut file, &mut outfile)
                .map_err(|e| GatewayError::Package(format!(
                    "Failed to write file '{}': {}", outpath.display(), e
                )))?;
        }
    }

    tracing::info!("Skill '{}' imported to {}", skill_name, target_skill_dir.display());
    Ok(skill_name)
}

/// Extract SKILL.md content from a ZIP archive.
///
/// Looks for SKILL.md at the root or inside a single top-level directory.
fn extract_skill_md(
    archive: &mut zip::ZipArchive<std::io::Cursor<Vec<u8>>>,
) -> Result<String, GatewayError> {
    // Try root-level first
    if let Ok(mut file) = archive.by_name("SKILL.md") {
        let mut content = String::new();
        file.read_to_string(&mut content)
            .map_err(|e| GatewayError::Package(format!("Failed to read SKILL.md: {}", e)))?;
        return Ok(content);
    }

    // Try inside a single top-level directory
    let top_dir = detect_top_level_dir(archive);
    if let Some(dir) = &top_dir {
        let path = format!("{}/SKILL.md", dir);
        if let Ok(mut file) = archive.by_name(&path) {
            let mut content = String::new();
            file.read_to_string(&mut content)
                .map_err(|e| GatewayError::Package(format!("Failed to read SKILL.md: {}", e)))?;
            return Ok(content);
        }
    }

    Err(GatewayError::Package(
        "SKILL.md not found in skill package".to_string()
    ))
}

/// Detect if the ZIP has a single top-level directory.
///
/// Returns the top-level directory name (e.g. "my-skill") if all entries
/// share the same single top-level directory component. Returns None if
/// entries are at root level or there are multiple top-level directories.
fn detect_top_level_dir(
    archive: &mut zip::ZipArchive<std::io::Cursor<Vec<u8>>>,
) -> Option<String> {
    let mut top_dirs: Vec<String> = Vec::new();
    for i in 0..archive.len() {
        let file = archive.by_index(i).ok()?;
        if let Some(path) = file.enclosed_name() {
            let first = path.components().next()?;
            let first_str = first.as_os_str().to_string_lossy().to_string();
            if !top_dirs.contains(&first_str) {
                top_dirs.push(first_str);
            }
            if top_dirs.len() > 1 {
                return None; // multiple top-level entries → no single prefix
            }
        }
    }
    match top_dirs.len() {
        1 => Some(top_dirs.into_iter().next().unwrap()),
        _ => None,
    }
}

/// Simple nanosecond timestamp for unique temp filenames
fn timestamp_nanos() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH)
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
    let info = gw.installed_agents.get(&agent_id)
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
    let info = gw.installed_agents.get(&agent_id)
        .ok_or_else(|| ApiError::not_found(&format!("Agent not found: {}", agent_id)))?;

    let skills_dir = agent_skills_dir(&info.install_path);
    drop(gw);

    if !skills_dir.exists() {
        return Err(ApiError::not_found(&format!(
            "Skill not found: {}", skill_name
        )));
    }

    let skills = load_skills_from_dir(&skills_dir);
    let parsed = skills.get(&skill_name)
        .ok_or_else(|| ApiError::not_found(&format!(
            "Skill not found: {}", skill_name
        )))?;

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

    while let Some(field) = multipart.next_field().await
        .map_err(|e| ApiError::bad_request(&format!("Failed to read multipart field: {}", e)))?
    {
        let name = field.name().unwrap_or_default().to_string();
        match name.as_str() {
            "package" => {
                let bytes = field.bytes().await
                    .map_err(|e| ApiError::bad_request(&format!("Failed to read package field: {}", e)))?;
                package_bytes = Some(bytes.to_vec());
            }
            "overwrite" => {
                let text = field.text().await.unwrap_or_default();
                overwrite = Some(text == "true" || text == "1");
            }
            _ => {} // ignore unknown fields
        }
    }

    let package_bytes = package_bytes
        .ok_or_else(|| ApiError::bad_request("Missing required field: 'package'"))?;

    if package_bytes.is_empty() {
        return Err(ApiError::bad_request("Package file is empty"));
    }

    // Verify agent exists and get install path
    let install_path = {
        let gw = state.gateway_state.read().await;
        let info = gw.installed_agents.get(&agent_id)
            .ok_or_else(|| ApiError::not_found(&format!("Agent not found: {}", agent_id)))?;
        info.install_path.clone()
    };

    let skills_dir = PathBuf::from(&install_path).join("skills");

    let _overwrite = overwrite.unwrap_or(false);

    // Spool uploaded bytes to a temp file, then call install_skill_package
    let install_result = tokio::task::spawn_blocking(move || {
        let temp_dir = std::env::temp_dir();
        let temp_file = temp_dir.join(format!(
            "rollball-skill-{}-{}.zip",
            std::process::id(),
            timestamp_nanos(),
        ));

        // Write bytes to temp file
        if let Err(e) = std::fs::write(&temp_file, &package_bytes) {
            return Err(GatewayError::Package(format!(
                "Failed to write upload to temp file: {}", e
            )));
        }

        // Perform skill installation
        let result = install_skill_package(&temp_file, &skills_dir);

        // Best-effort cleanup of temp file
        let _ = std::fs::remove_file(&temp_file);

        result
    }).await;

    match install_result {
        Ok(Ok(skill_name)) => Ok((StatusCode::CREATED, Json(ImportSkillResponse {
            success: true,
            skill_name: skill_name.clone(),
            message: format!("Skill '{}' imported successfully", skill_name),
        }))),
        Ok(Err(GatewayError::Package(msg))) => Err(ApiError::bad_request(&format!(
            "Skill import failed: {}", msg
        ))),
        Ok(Err(e)) => Err(ApiError::internal(&format!("Skill import failed: {}", e))),
        Err(e) => Err(ApiError::internal(&format!("Skill import task failed: {}", e))),
    }
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
        return Err(ApiError::not_found(&format!("Agent not found: {}", agent_id)));
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
    use std::io::Write;

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

    /// Helper: create a skill ZIP package at root level (SKILL.md at top)
    fn create_root_level_skill_zip(dir: &StdPath) -> PathBuf {
        let zip_path = dir.join("root-skill.zip");
        let zip_file = std::fs::File::create(&zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(zip_file);
        let options = zip::write::SimpleFileOptions::default();

        zip.start_file("SKILL.md", options).unwrap();
        zip.write_all(b"---\nname: root-skill\ndescription: A root-level skill\ntriggers:\n  - test\n---\n\nRoot skill instructions.").unwrap();

        zip.start_file("prompts/action.md", options).unwrap();
        zip.write_all(b"Action prompt content.").unwrap();

        zip.finish().unwrap();
        zip_path
    }

    /// Helper: create a skill ZIP package with a top-level directory
    fn create_nested_skill_zip(dir: &StdPath) -> PathBuf {
        let zip_path = dir.join("nested-skill.zip");
        let zip_file = std::fs::File::create(&zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(zip_file);
        let options = zip::write::SimpleFileOptions::default();

        zip.start_file("my-skill/SKILL.md", options).unwrap();
        zip.write_all(b"---\nname: my-skill\ndescription: A nested skill\ntriggers:\n  - nested\n---\n\nNested skill instructions.").unwrap();

        zip.start_file("my-skill/prompts/action.md", options).unwrap();
        zip.write_all(b"Nested action prompt.").unwrap();

        zip.finish().unwrap();
        zip_path
    }

    #[test]
    fn test_install_skill_package_root_level() {
        let tmp = std::env::temp_dir().join(format!("rollball-test-skill-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let zip_path = create_root_level_skill_zip(&tmp);
        let skills_dir = tmp.join("skills");

        let result = install_skill_package(&zip_path, &skills_dir);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "root-skill");

        // Verify extracted files
        assert!(skills_dir.join("root-skill/SKILL.md").exists());
        assert!(skills_dir.join("root-skill/prompts/action.md").exists());

        let content = std::fs::read_to_string(skills_dir.join("root-skill/prompts/action.md")).unwrap();
        assert_eq!(content, "Action prompt content.");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_install_skill_package_nested() {
        let tmp = std::env::temp_dir().join(format!("rollball-test-skill-nested-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let zip_path = create_nested_skill_zip(&tmp);
        let skills_dir = tmp.join("skills");

        let result = install_skill_package(&zip_path, &skills_dir);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "my-skill");

        // Verify extracted files (prefix should be stripped)
        assert!(skills_dir.join("my-skill/SKILL.md").exists());
        assert!(skills_dir.join("my-skill/prompts/action.md").exists());

        let content = std::fs::read_to_string(skills_dir.join("my-skill/prompts/action.md")).unwrap();
        assert_eq!(content, "Nested action prompt.");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_install_skill_package_duplicate() {
        let tmp = std::env::temp_dir().join(format!("rollball-test-skill-dup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let zip_path = create_root_level_skill_zip(&tmp);
        let skills_dir = tmp.join("skills");

        // First install should succeed
        let result1 = install_skill_package(&zip_path, &skills_dir);
        assert!(result1.is_ok());

        // Second install should fail (duplicate)
        let result2 = install_skill_package(&zip_path, &skills_dir);
        assert!(result2.is_err());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_install_skill_package_missing_skill_md() {
        let tmp = std::env::temp_dir().join(format!("rollball-test-skill-nomd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // Create ZIP without SKILL.md
        let zip_path = tmp.join("no-skill-md.zip");
        let zip_file = std::fs::File::create(&zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(zip_file);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("readme.txt", options).unwrap();
        zip.write_all(b"No SKILL.md here").unwrap();
        zip.finish().unwrap();

        let skills_dir = tmp.join("skills");
        let result = install_skill_package(&zip_path, &skills_dir);
        assert!(result.is_err());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_detect_top_level_dir() {
        let tmp = std::env::temp_dir().join(format!("rollball-test-prefix-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // Nested ZIP (single top-level dir)
        let zip_path = tmp.join("nested.zip");
        let zip_file = std::fs::File::create(&zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(zip_file);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("my-skill/SKILL.md", options).unwrap();
        zip.write_all(b"content").unwrap();
        zip.start_file("my-skill/prompts/a.md", options).unwrap();
        zip.write_all(b"prompt").unwrap();
        zip.finish().unwrap();

        let data = std::fs::read(&zip_path).unwrap();
        let reader = std::io::Cursor::new(data);
        let mut archive = zip::ZipArchive::new(reader).unwrap();
        let prefix = detect_top_level_dir(&mut archive);
        assert!(prefix.is_some());
        assert_eq!(prefix.unwrap(), "my-skill");

        // Root-level ZIP (no single top-level dir)
        let zip_path2 = tmp.join("root.zip");
        let zip_file2 = std::fs::File::create(&zip_path2).unwrap();
        let mut zip2 = zip::ZipWriter::new(zip_file2);
        zip2.start_file("SKILL.md", options).unwrap();
        zip2.write_all(b"content").unwrap();
        zip2.start_file("prompts/a.md", options).unwrap();
        zip2.write_all(b"prompt").unwrap();
        zip2.finish().unwrap();

        let data2 = std::fs::read(&zip_path2).unwrap();
        let reader2 = std::io::Cursor::new(data2);
        let mut archive2 = zip::ZipArchive::new(reader2).unwrap();
        let prefix2 = detect_top_level_dir(&mut archive2);
        assert!(prefix2.is_none());

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
