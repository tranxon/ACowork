//! System prompt builder (from prompts/ + skills/)
//!
//! Assembles the complete system prompt from:
//! 1. prompts/system.md — Agent identity definition
//! 2. prompts/constraints.md — Behavioral constraints
//! 3. prompts/*.md — Additional prompt sections
//! 4. skills/*/SKILL.md — Skill instructions
//!
//! [`COMPACTION_PROMPT_FILE`] (`prompts/summary.md`) is the one exception to
//! the "everything in prompts/ goes into the system prompt" rule: it is the
//! agent-specific context-compaction directive and is deliberately excluded
//! from the main system prompt (see [`load_compaction_prompt`]).

use std::fs;
use std::path::Path;

use crate::error::{Result, RuntimeError};
use crate::skills::parser::SkillRegistry;

/// Filename of the agent-specific compaction prompt inside `prompts/`.
///
/// When present, this file replaces the built-in
/// [`crate::prompt::COMPACTION_SYSTEM_PROMPT`] as the system prompt for
/// context compaction and episode distillation. It is a summarization
/// directive, not a dialog identity section, so [`build_system_prompt_with_mode`]
/// skips it when assembling the main system prompt.
pub const COMPACTION_PROMPT_FILE: &str = "summary.md";

/// Load the agent-specific compaction prompt from `prompts/summary.md`.
///
/// Returns `None` when the file is missing or contains only whitespace —
/// the caller then falls back to the built-in default
/// [`crate::prompt::COMPACTION_SYSTEM_PROMPT`]. Surrounding whitespace is
/// stripped so a file consisting only of blank lines behaves like an
/// absent file.
///
/// An existing-but-unreadable file (permissions, invalid UTF-8) also
/// yields `None` but is logged as a warning — a broken package must not
/// be silently masked by the fallback.
///
/// The filename is matched **exactly** (`summary.md`) — the same criterion
/// [`build_system_prompt_with_mode`] uses to exclude it from the main
/// dialog system prompt — so "excluded from main prompt" and "loaded as
/// compaction prompt" always refer to the same file. Any other name
/// (`SUMMARY.md`, `summary.txt`) is an ordinary prompt section.
pub fn load_compaction_prompt(package_dir: &Path) -> Option<String> {
    let path = package_dir.join("prompts").join(COMPACTION_PROMPT_FILE);
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        // File (or the prompts/ dir) absent → normal fallback, no log spam.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        // File exists but is unreadable (permissions) or not valid UTF-8 —
        // surface it so a broken package is not silently masked by the
        // built-in default.
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "Failed to read compaction prompt; falling back to built-in default"
            );
            return None;
        }
    };
    let trimmed = content.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Build system prompt from package files (default: Manual skill mode).
///
/// Backward-compatible wrapper that defaults to `SkillMode::Manual`,
/// meaning no skill content is injected into the system prompt.
/// For explicit mode control, use [`build_system_prompt_with_mode`].
pub fn build_system_prompt(package_dir: &Path) -> Result<String> {
    build_system_prompt_with_mode(package_dir, acowork_core::SkillMode::Manual)
}

/// Build system prompt from package files with explicit skill mode.
///
/// Reads prompt files in alphabetical order and concatenates them.
/// Skill injection behavior is controlled by `skill_mode`:
/// - `Manual`: no skill content is injected.
/// - `Progressive`: a compact summary (name + description) of available skills
///   is appended after prompt sections.
///
/// [`COMPACTION_PROMPT_FILE`] is excluded — it is the compaction directive,
/// not a dialog section.
pub fn build_system_prompt_with_mode(
    package_dir: &Path,
    skill_mode: acowork_core::SkillMode,
) -> Result<String> {
    let mut sections = Vec::new();

    // Load prompt files
    let prompts_dir = package_dir.join("prompts");
    if prompts_dir.exists() {
        let mut prompt_files = collect_markdown_files(&prompts_dir)?;
        prompt_files.sort();

        for path in &prompt_files {
            // Use filename (without extension) as section header
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown");

            // Skip the compaction prompt — it is a summarization directive
            // for context compaction / episode distillation, not a section
            // of the main dialog system prompt. Including it here would
            // leak summarization meta-instructions into every LLM call.
            //
            // Exact filename match keeps the exclusion symmetric with
            // `load_compaction_prompt`: only `summary.md` has special
            // semantics; any other name (SUMMARY.md, summary.txt) is an
            // ordinary prompt section.
            if path.file_name().is_some_and(|n| n == COMPACTION_PROMPT_FILE) {
                continue;
            }

            let content = fs::read_to_string(path).map_err(|e| {
                RuntimeError::Package(format!(
                    "Failed to read prompt file {}: {e}",
                    path.display()
                ))
            })?;

            sections.push(format!("## {name}\n\n{content}"));
        }
    }

    // Load skill files based on skill_mode
    let skills_dir = package_dir.join("skills");
    if skills_dir.exists() {
        match skill_mode {
            acowork_core::SkillMode::Manual => {
                tracing::debug!(
                    skills_dir = %skills_dir.display(),
                    "Skill mode is Manual — skipping skill injection"
                );
            }
            acowork_core::SkillMode::Progressive => {
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
    let entries = fs::read_dir(dir)
        .map_err(|e| RuntimeError::Package(format!("Failed to read prompts dir: {e}")))?;

    for entry in entries {
        let entry =
            entry.map_err(|e| RuntimeError::Package(format!("Failed to read dir entry: {e}")))?;

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
        let dir = std::env::temp_dir().join(format!("acowork-test-prompt-builder-{name}"));
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
        let prompt = build_system_prompt_with_mode(&dir, acowork_core::SkillMode::Manual).unwrap();
        assert!(prompt.contains("weather assistant"));
        assert!(!prompt.contains("Greeting"));
        assert!(!prompt.contains("Skill:"));
    }

    #[test]
    fn test_build_system_prompt_progressive_mode() {
        let dir = std::env::temp_dir().join("acowork-test-prompt-progressive");
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

        let prompt =
            build_system_prompt_with_mode(&dir, acowork_core::SkillMode::Progressive).unwrap();
        assert!(prompt.contains("You are a test assistant."));
        assert!(prompt.contains("## Available Skills"));
        assert!(prompt.contains("greeting"));
        assert!(prompt.contains("Greet users warmly"));
        // Full instructions should NOT appear in progressive mode
        assert!(!prompt.contains("Be friendly and welcoming."));
    }

    #[test]
    fn test_build_system_prompt_empty_dir() {
        let dir = std::env::temp_dir().join("acowork-test-prompt-empty");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let prompt = build_system_prompt(&dir).unwrap();
        assert_eq!(prompt, "You are a helpful AI assistant.");
    }

    // ----- load_compaction_prompt -----

    /// Unique temp dir per test — the fixed-name helper below caused
    /// parallel test flakiness (two tests deleting each other's files).
    fn create_package_with_summary(name: &str, summary: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("acowork-test-compaction-prompt-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("prompts")).unwrap();
        fs::write(dir.join("prompts").join("system.md"), "You are a test agent.").unwrap();
        fs::write(dir.join("prompts").join(COMPACTION_PROMPT_FILE), summary).unwrap();
        dir
    }

    #[test]
    fn test_load_compaction_prompt_missing_file_returns_none() {
        let dir = std::env::temp_dir().join("acowork-test-compaction-missing");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("prompts")).unwrap();
        fs::write(dir.join("prompts").join("system.md"), "You are a test agent.").unwrap();

        assert_eq!(load_compaction_prompt(&dir), None);
    }

    #[test]
    fn test_load_compaction_prompt_reads_file() {
        let dir = create_package_with_summary("reads-file", "Summarize with code details.\n");
        let loaded = load_compaction_prompt(&dir);
        assert_eq!(loaded.as_deref(), Some("Summarize with code details."));
    }

    #[test]
    fn test_load_compaction_prompt_whitespace_only_is_none() {
        // A file containing only blank lines behaves like an absent file —
        // the caller falls back to the built-in default prompt.
        let dir = create_package_with_summary("whitespace", "   \n\t\n  ");
        assert_eq!(load_compaction_prompt(&dir), None);
    }

    #[test]
    fn test_build_system_prompt_excludes_summary_md() {
        // summary.md must NOT leak into the main dialog system prompt.
        let dir = create_package_with_summary(
            "excludes",
            "You are a summarizer. Do NOT appear in the main system prompt.",
        );
        let prompt = build_system_prompt(&dir).unwrap();
        assert!(prompt.contains("test agent"));
        assert!(!prompt.contains("summarizer"));
        assert!(!prompt.contains("## summary"));
    }

    #[test]
    fn test_build_system_prompt_still_includes_other_sections() {
        // Only summary.md is excluded; sibling prompt files keep working.
        let dir = create_package_with_summary("includes", "summarizer directive");
        fs::write(
            dir.join("prompts").join("constraints.md"),
            "Always be polite.",
        )
        .unwrap();
        let prompt = build_system_prompt(&dir).unwrap();
        assert!(prompt.contains("test agent"));
        assert!(prompt.contains("Always be polite."));
        assert!(!prompt.contains("summarizer directive"));
    }

    #[test]
    fn test_build_system_prompt_does_not_exclude_summary_case_variant() {
        // Symmetry with load_compaction_prompt: only the exact filename
        // `summary.md` has special semantics. A case variant like
        // `SUMMARY.md` is an ordinary prompt section — it must NOT be
        // silently dropped (on case-insensitive filesystems it would still
        // be read as the compaction prompt, so excluding it here would make
        // it invisible in BOTH places).
        let dir = std::env::temp_dir().join("acowork-test-compaction-case-variant");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("prompts")).unwrap();
        fs::write(dir.join("prompts").join("system.md"), "You are a test agent.").unwrap();
        fs::write(
            dir.join("prompts").join("SUMMARY.md"),
            "Case variant summary directive.",
        )
        .unwrap();
        let prompt = build_system_prompt(&dir).unwrap();
        assert!(prompt.contains("test agent"));
        assert!(prompt.contains("Case variant summary directive."));
        assert!(prompt.contains("## SUMMARY"));
    }

    #[test]
    fn test_build_system_prompt_does_not_exclude_summary_txt() {
        // Symmetry with load_compaction_prompt: only `summary.md` is
        // excluded. `summary.txt` (collected by collect_markdown_files but
        // never loaded as the compaction prompt) must surface as an
        // ordinary section rather than being silently dropped.
        let dir = std::env::temp_dir().join("acowork-test-compaction-txt-variant");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("prompts")).unwrap();
        fs::write(dir.join("prompts").join("system.md"), "You are a test agent.").unwrap();
        fs::write(
            dir.join("prompts").join("summary.txt"),
            "Txt summary directive.",
        )
        .unwrap();
        let prompt = build_system_prompt(&dir).unwrap();
        assert!(prompt.contains("test agent"));
        assert!(prompt.contains("Txt summary directive."));
        assert!(prompt.contains("## summary"));
    }
}
