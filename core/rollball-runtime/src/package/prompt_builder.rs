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
use crate::skills::parser::SkillRegistry;

/// Build system prompt from package files (default: Manual skill mode).
///
/// Backward-compatible wrapper that defaults to `SkillMode::Manual`,
/// meaning no skill content is injected into the system prompt.
/// For explicit mode control, use [`build_system_prompt_with_mode`].
pub fn build_system_prompt(package_dir: &Path) -> Result<String> {
    build_system_prompt_with_mode(package_dir, rollball_core::SkillMode::Manual)
}

/// Build system prompt from package files with explicit skill mode.
///
/// Reads prompt files in alphabetical order and concatenates them.
/// Skill injection behavior is controlled by `skill_mode`:
/// - `Manual`: no skill content is injected.
/// - `Progressive`: a compact summary (name + description) of available skills
///   is appended after prompt sections.
pub fn build_system_prompt_with_mode(
    package_dir: &Path,
    skill_mode: rollball_core::SkillMode,
) -> Result<String> {
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

    // Load skill files based on skill_mode
    let skills_dir = package_dir.join("skills");
    if skills_dir.exists() {
        match skill_mode {
            rollball_core::SkillMode::Manual => {
                tracing::debug!(
                    skills_dir = %skills_dir.display(),
                    "Skill mode is Manual — skipping skill injection"
                );
            }
            rollball_core::SkillMode::Progressive => {
                match SkillRegistry::load_from_dir(&skills_dir) {
                    Ok(registry) => {
                        let summary = registry.build_skill_summary();
                        if !summary.is_empty() {
                            sections.push(summary);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            skills_dir = %skills_dir.display(),
                            error = %e,
                            "Failed to load skills for summary injection"
                        );
                    }
                }
            }
        }
    }

    if sections.is_empty() {
        // Default system prompt if no files found
        return Ok(crate::prompt::PROMPT_BUILDER_FALLBACK.to_string());
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn create_test_package(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rollball-test-prompt-builder-{name}"));
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
        let dir = create_test_package("default");
        let prompt = build_system_prompt(&dir).unwrap();
        assert!(prompt.contains("weather assistant"));
        assert!(prompt.contains("user's language"));
        // Default mode is Manual — skill content should NOT be injected
        assert!(!prompt.contains("Greeting"));
    }

    #[test]
    fn test_build_system_prompt_manual_mode() {
        let dir = create_test_package("manual");
        let prompt = build_system_prompt_with_mode(&dir, rollball_core::SkillMode::Manual).unwrap();
        assert!(prompt.contains("weather assistant"));
        assert!(!prompt.contains("Greeting"));
        assert!(!prompt.contains("Skill:"));
    }

    #[test]
    fn test_build_system_prompt_progressive_mode() {
        let dir = std::env::temp_dir().join("rollball-test-prompt-progressive");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("prompts")).unwrap();
        fs::create_dir_all(dir.join("skills").join("greeting")).unwrap();

        fs::write(
            dir.join("prompts").join("system.md"),
            "You are a test assistant.",
        )
        .unwrap();

        // Write a valid SKILL.md with YAML frontmatter
        fs::write(
            dir.join("skills").join("greeting").join("SKILL.md"),
            r#"---
name: greeting
description: Greet users warmly
triggers:
  - hello
---

# Greeting Skill

Be friendly and welcoming.
"#,
        )
        .unwrap();

        let prompt = build_system_prompt_with_mode(&dir, rollball_core::SkillMode::Progressive).unwrap();
        assert!(prompt.contains("You are a test assistant."));
        assert!(prompt.contains("## Available Skills"));
        assert!(prompt.contains("greeting"));
        assert!(prompt.contains("Greet users warmly"));
        // Full instructions should NOT appear in progressive mode
        assert!(!prompt.contains("Be friendly and welcoming."));
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
