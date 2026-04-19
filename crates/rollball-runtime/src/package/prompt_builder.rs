//! System prompt builder (from prompts/ + skills/)
//!
//! Assembles the complete system prompt from:
//! 1. prompts/system.md — Agent identity definition
//! 2. prompts/constraints.md — Behavioral constraints
//! 3. prompts/*.md — Additional prompt sections
//! 4. skills/*/SKILL.md — Skill instructions

use std::fs;
use std::path::Path;

use crate::error::{Result, RuntimeError};

/// Build system prompt from package files
///
/// Reads prompt files in alphabetical order and concatenates them.
/// Skills are appended after prompt sections.
pub fn build_system_prompt(package_dir: &Path) -> Result<String> {
    let mut sections = Vec::new();

    // Load prompt files
    let prompts_dir = package_dir.join("prompts");
    if prompts_dir.exists() {
        let mut prompt_files = collect_markdown_files(&prompts_dir)?;
        prompt_files.sort();

        for path in &prompt_files {
            let content = fs::read_to_string(path).map_err(|e| {
                RuntimeError::Package(format!(
                    "Failed to read prompt file {}: {e}",
                    path.display()
                ))
            })?;

            // Use filename (without extension) as section header
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown");

            sections.push(format!("## {name}\n\n{content}"));
        }
    }

    // Load skill files
    let skills_dir = package_dir.join("skills");
    if skills_dir.exists() {
        let skill_files = collect_skill_files(&skills_dir)?;

        for path in &skill_files {
            let content = fs::read_to_string(path).map_err(|e| {
                RuntimeError::Package(format!(
                    "Failed to read skill file {}: {e}",
                    path.display()
                ))
            })?;

            let skill_name = path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .unwrap_or("unknown");

            sections.push(format!("## Skill: {skill_name}\n\n{content}"));
        }
    }

    if sections.is_empty() {
        // Default system prompt if no files found
        return Ok("You are a helpful AI assistant.".to_string());
    }

    Ok(sections.join("\n\n---\n\n"))
}

/// Collect markdown/text files from a directory
fn collect_markdown_files(dir: &Path) -> Result<Vec<std::path::PathBuf>> {
    let mut files = Vec::new();
    let entries = fs::read_dir(dir).map_err(|e| {
        RuntimeError::Package(format!("Failed to read prompts dir: {e}"))
    })?;

    for entry in entries {
        let entry = entry.map_err(|e| {
            RuntimeError::Package(format!("Failed to read dir entry: {e}"))
        })?;

        let path = entry.path();
        if path.is_file()
            && path
                .extension()
                .is_some_and(|ext| ext == "md" || ext == "txt")
        {
            files.push(path);
        }
    }

    Ok(files)
}

/// Collect SKILL.md files from skills/ subdirectories
fn collect_skill_files(dir: &Path) -> Result<Vec<std::path::PathBuf>> {
    let mut files = Vec::new();
    let entries = fs::read_dir(dir).map_err(|e| {
        RuntimeError::Package(format!("Failed to read skills dir: {e}"))
    })?;

    for entry in entries {
        let entry = entry.map_err(|e| {
            RuntimeError::Package(format!("Failed to read dir entry: {e}"))
        })?;

        let path = entry.path();
        if path.is_dir() {
            let skill_md = path.join("SKILL.md");
            if skill_md.exists() {
                files.push(skill_md);
            }
        }
    }

    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn create_test_package() -> PathBuf {
        let dir = std::env::temp_dir().join("rollball-test-prompt-builder");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("prompts")).unwrap();
        fs::create_dir_all(dir.join("skills").join("greeting")).unwrap();

        fs::write(
            dir.join("prompts").join("system.md"),
            "You are a weather assistant.",
        )
        .unwrap();
        fs::write(
            dir.join("prompts").join("constraints.md"),
            "Always respond in the user's language.",
        )
        .unwrap();
        fs::write(
            dir.join("skills").join("greeting").join("SKILL.md"),
            "# Greeting\nBe friendly.",
        )
        .unwrap();

        dir
    }

    #[test]
    fn test_build_system_prompt_with_files() {
        let dir = create_test_package();
        let prompt = build_system_prompt(&dir).unwrap();
        assert!(prompt.contains("weather assistant"));
        assert!(prompt.contains("user's language"));
        assert!(prompt.contains("Greeting"));
    }

    #[test]
    fn test_build_system_prompt_empty_dir() {
        let dir = std::env::temp_dir().join("rollball-test-prompt-empty");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let prompt = build_system_prompt(&dir).unwrap();
        assert_eq!(prompt, "You are a helpful AI assistant.");
    }
}
