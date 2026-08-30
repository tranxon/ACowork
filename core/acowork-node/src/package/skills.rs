//! Skill package operations (ADR-055 §6.20 — migrated from gateway
//! `http/skills_api.rs` file operations).
//!
//! Contains the node-local file operations for skills: import a skill
//! ZIP into an installed agent's `skills/` directory, parse SKILL.md
//! frontmatter, and enumerate skills on disk. The HTTP handlers stay in
//! the Gateway and drive these via the node control plane
//! (`skills_import` command) / Runtime HTTP (list).

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::error::{NodeError, Result};

/// A single skill entry in a list result.
#[derive(Debug, Clone)]
pub struct SkillListEntry {
    pub name: String,
    pub description: String,
    pub version: Option<String>,
    pub author: Option<String>,
    pub triggers: Vec<String>,
    pub tool_deps: Vec<String>,
}

/// Parsed SKILL.md frontmatter (YAML section).
#[derive(Debug, Clone, serde::Deserialize)]
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

/// Parsed SKILL.md with frontmatter and instructions body.
#[derive(Debug, Clone)]
pub struct ParsedSkill {
    pub entry: SkillListEntry,
    pub instructions: String,
}

/// Parse a SKILL.md content string into a ParsedSkill.
pub fn parse_skill_md(content: &str) -> Option<ParsedSkill> {
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

/// Load all skills from an agent's `skills/` directory.
pub fn load_skills_from_dir(skills_dir: &Path) -> HashMap<String, ParsedSkill> {
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

/// Resolve the skills directory for an installed agent's install path.
pub fn agent_skills_dir(install_path: &str) -> PathBuf {
    PathBuf::from(install_path).join("skills")
}

/// Extract a skill ZIP package to the agent's skills directory.
///
/// Validates the ZIP contains a SKILL.md at its root (or in a single
/// top-level directory), parses the frontmatter to get the skill name,
/// then extracts all files to `{install_path}/skills/{skill_name}/`.
///
/// Security: uses `enclosed_name()` to prevent Zip Slip path traversal.
pub fn install_skill_package(package_path: &Path, skills_dir: &Path) -> Result<String> {
    // 1. Read and open ZIP
    let data = std::fs::read(package_path).map_err(|e| {
        NodeError::Package(format!(
            "Failed to read skill package '{}': {}",
            package_path.display(),
            e
        ))
    })?;
    let reader = std::io::Cursor::new(data);
    let mut archive = zip::ZipArchive::new(reader).map_err(|e| {
        NodeError::Package(format!(
            "Failed to read skill ZIP '{}': {}",
            package_path.display(),
            e
        ))
    })?;

    // 2. Locate SKILL.md — root or inside a single top-level directory
    let skill_md_content = extract_skill_md(&mut archive)?;

    // 3. Parse SKILL.md frontmatter to extract skill name
    let parsed = parse_skill_md(&skill_md_content).ok_or_else(|| {
        NodeError::Package(
            "Invalid SKILL.md format: missing or malformed YAML frontmatter".to_string(),
        )
    })?;
    let skill_name = parsed.entry.name;

    // 4. Ensure the agent's skills directory exists
    std::fs::create_dir_all(skills_dir)
        .map_err(|e| NodeError::Package(format!("Failed to create skills directory: {}", e)))?;

    // 5. Check if a skill with the same name already exists
    let target_skill_dir = skills_dir.join(&skill_name);
    if target_skill_dir.exists() {
        return Err(NodeError::Package(format!(
            "Skill '{}' already exists (will not overwrite)",
            skill_name
        )));
    }

    // 6. Create the target skill directory
    std::fs::create_dir_all(&target_skill_dir).map_err(|e| {
        NodeError::Package(format!(
            "Failed to create skill directory '{}': {}",
            target_skill_dir.display(),
            e
        ))
    })?;

    // 7. Extract all files to the target skill directory
    let top_dir_name = detect_top_level_dir(&mut archive);
    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| NodeError::Package(format!("ZIP read error: {}", e)))?;

        let raw_path = match file.enclosed_name() {
            Some(p) => p,
            None => continue, // skip unsafe paths (zip-slip protection)
        };

        // Strip the top-level directory prefix if present
        let relative_path = match &top_dir_name {
            Some(prefix) => match raw_path.strip_prefix(prefix) {
                Ok(stripped) => stripped,
                Err(_) => &raw_path,
            },
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
            if let Some(p) = outpath.parent()
                && !p.exists()
            {
                std::fs::create_dir_all(p).ok();
            }
            let mut outfile = std::fs::File::create(&outpath).map_err(|e| {
                NodeError::Package(format!(
                    "Failed to create file '{}': {}",
                    outpath.display(),
                    e
                ))
            })?;
            std::io::copy(&mut file, &mut outfile).map_err(|e| {
                NodeError::Package(format!(
                    "Failed to write file '{}': {}",
                    outpath.display(),
                    e
                ))
            })?;
        }
    }

    tracing::info!(
        "Skill '{}' imported to {}",
        skill_name,
        target_skill_dir.display()
    );
    Ok(skill_name)
}

/// Extract SKILL.md content from a ZIP archive (root or single top-level
/// directory).
fn extract_skill_md(
    archive: &mut zip::ZipArchive<std::io::Cursor<Vec<u8>>>,
) -> Result<String> {
    // Try root-level first
    if let Ok(mut file) = archive.by_name("SKILL.md") {
        let mut content = String::new();
        file.read_to_string(&mut content)
            .map_err(|e| NodeError::Package(format!("Failed to read SKILL.md: {}", e)))?;
        return Ok(content);
    }

    // Try inside a single top-level directory
    let top_dir = detect_top_level_dir(archive);
    if let Some(dir) = &top_dir {
        let path = format!("{}/SKILL.md", dir);
        if let Ok(mut file) = archive.by_name(&path) {
            let mut content = String::new();
            file.read_to_string(&mut content)
                .map_err(|e| NodeError::Package(format!("Failed to read SKILL.md: {}", e)))?;
            return Ok(content);
        }
    }

    Err(NodeError::Package(
        "SKILL.md not found in skill package".to_string(),
    ))
}

/// Detect if the ZIP has a single top-level directory.
fn detect_top_level_dir(archive: &mut zip::ZipArchive<std::io::Cursor<Vec<u8>>>) -> Option<String> {
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
    fn test_parse_skill_md_no_frontmatter() {
        let content = "No frontmatter here";
        assert!(parse_skill_md(content).is_none());
    }

    #[test]
    fn test_agent_skills_dir() {
        let dir = agent_skills_dir("/tmp/weather-agent-1.0.0");
        assert_eq!(dir, PathBuf::from("/tmp/weather-agent-1.0.0/skills"));
    }

    #[test]
    fn test_load_skills_from_nonexistent_dir() {
        let skills = load_skills_from_dir(Path::new("/nonexistent/path"));
        assert!(skills.is_empty());
    }

    fn create_root_level_skill_zip(dir: &Path) -> PathBuf {
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

    fn create_nested_skill_zip(dir: &Path) -> PathBuf {
        let zip_path = dir.join("nested-skill.zip");
        let zip_file = std::fs::File::create(&zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(zip_file);
        let options = zip::write::SimpleFileOptions::default();

        zip.start_file("my-skill/SKILL.md", options).unwrap();
        zip.write_all(b"---\nname: my-skill\ndescription: A nested skill\ntriggers:\n  - nested\n---\n\nNested skill instructions.").unwrap();

        zip.start_file("my-skill/prompts/action.md", options)
            .unwrap();
        zip.write_all(b"Nested action prompt.").unwrap();

        zip.finish().unwrap();
        zip_path
    }

    #[test]
    fn test_install_skill_package_root_level() {
        let tmp =
            std::env::temp_dir().join(format!("acowork-test-skill-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let zip_path = create_root_level_skill_zip(&tmp);
        let skills_dir = tmp.join("skills");

        let result = install_skill_package(&zip_path, &skills_dir);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "root-skill");

        assert!(skills_dir.join("root-skill/SKILL.md").exists());
        assert!(skills_dir.join("root-skill/prompts/action.md").exists());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_install_skill_package_nested() {
        let tmp =
            std::env::temp_dir().join(format!("acowork-test-skill-nested-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let zip_path = create_nested_skill_zip(&tmp);
        let skills_dir = tmp.join("skills");

        let result = install_skill_package(&zip_path, &skills_dir);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "my-skill");

        assert!(skills_dir.join("my-skill/SKILL.md").exists());
        assert!(skills_dir.join("my-skill/prompts/action.md").exists());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_install_skill_package_duplicate() {
        let tmp =
            std::env::temp_dir().join(format!("acowork-test-skill-dup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let zip_path = create_root_level_skill_zip(&tmp);
        let skills_dir = tmp.join("skills");

        assert!(install_skill_package(&zip_path, &skills_dir).is_ok());
        assert!(install_skill_package(&zip_path, &skills_dir).is_err());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_install_skill_package_missing_skill_md() {
        let tmp =
            std::env::temp_dir().join(format!("acowork-test-skill-nomd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let zip_path = tmp.join("no-skill-md.zip");
        let zip_file = std::fs::File::create(&zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(zip_file);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("readme.txt", options).unwrap();
        zip.write_all(b"No SKILL.md here").unwrap();
        zip.finish().unwrap();

        let skills_dir = tmp.join("skills");
        assert!(install_skill_package(&zip_path, &skills_dir).is_err());

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
