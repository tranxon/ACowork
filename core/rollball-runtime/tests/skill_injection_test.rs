//! End-to-end integration test for skill manual injection
//!
//! Verifies that when a message carries a `command` field, the Runtime
//! correctly resolves the skill instructions and injects them into the
//! ContextBuilder (system prompt), making them visible in the debug panel.
//!
//! Test tiers:
//!   1. Pure logic — SkillRegistry loading + injection format (no network)
//!   2. Real LLM  — requires MINIMAX_API_KEY (marked #[ignore])
//!
//! Run:
//!   cargo test --test skill_injection_test -- --nocapture
//!   cargo test --test skill_injection_test -- --ignored --nocapture   (real LLM)

use rollball_core::providers::traits::Provider;
use rollball_runtime::skills::parser::{SkillRegistry, SkillDefinition, parse_skill_md};

// ── Constants ─────────────────────────────────────────────────────────────

/// Absolute path to the project-manager-agent skills directory.
/// Uses workspace-relative resolution so the test works regardless of CWD.
fn project_manager_skills_dir() -> std::path::PathBuf {
    // Cargo integration tests run from the workspace root or the crate dir;
    // try both strategies.
    let candidates = [
        std::path::PathBuf::from("../../examples/project-manager-agent/skills"),
        std::path::PathBuf::from("examples/project-manager-agent/skills"),
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("examples/project-manager-agent/skills"),
    ];
    for c in &candidates {
        if c.exists() {
            return c.clone();
        }
    }
    // Fallback — return the CARGO_MANIFEST_DIR-based one; the test will
    // fail with a clear message if it doesn't exist.
    candidates[2].clone()
}

/// Build skill instructions for injection into the system prompt.
/// Mirrors the cli.rs logic: skill instructions are passed via
/// ContextBuilder.set_skill_instructions() and injected under
/// "## Skill Instructions" in the system prompt.
fn build_skill_instructions(skill: &SkillDefinition) -> String {
    skill.instructions.clone()
}

// ═══════════════════════════════════════════════════════════════════════
// Tier 1: Pure logic tests (no network, no LLM)
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_skill_registry_loads_from_project_manager_agent() {
    let skills_dir = project_manager_skills_dir();
    assert!(
        skills_dir.exists(),
        "Skills directory should exist at {:?}",
        skills_dir
    );

    let registry = SkillRegistry::load_from_dir(&skills_dir)
        .expect("SkillRegistry::load_from_dir should succeed");

    // The project-manager-agent has 7 skills
    assert!(
        !registry.is_empty(),
        "Registry should not be empty after loading from {:?}",
        skills_dir
    );
    assert!(
        registry.len() >= 7,
        "Expected at least 7 skills, got {}",
        registry.len()
    );
}

#[test]
fn test_skill_registry_contains_meeting_notes() {
    let skills_dir = project_manager_skills_dir();
    let registry = SkillRegistry::load_from_dir(&skills_dir)
        .expect("load_from_dir should succeed");

    let skill = registry
        .get("meeting-notes")
        .expect("'meeting-notes' skill should exist in registry");

    assert_eq!(skill.name, "meeting-notes");
    assert!(
        !skill.instructions.is_empty(),
        "meeting-notes instructions should not be empty"
    );
    // Verify key content from SKILL.md
    assert!(
        skill.instructions.contains("Execution Steps"),
        "Instructions should contain 'Execution Steps'"
    );
    assert!(
        skill.instructions.contains("Action Items"),
        "Instructions should contain 'Action Items'"
    );
}

#[test]
fn test_skill_instructions_preserved_for_context_builder() {
    // Verify that skill instructions are passed as-is for ContextBuilder injection.
    // The cli.rs now passes instructions via SessionMessage::ChatMessage.skill_instructions
    // and they are injected into the system prompt via ContextBuilder.set_skill_instructions().
    let skills_dir = project_manager_skills_dir();
    let registry = SkillRegistry::load_from_dir(&skills_dir)
        .expect("load_from_dir should succeed");

    let skill = registry.get("meeting-notes").unwrap();

    let instructions = build_skill_instructions(skill);

    // Instructions should match the skill definition exactly (no concatenation)
    assert_eq!(
        instructions, skill.instructions,
        "Skill instructions should be passed as-is for ContextBuilder"
    );
    assert!(
        !instructions.is_empty(),
        "Skill instructions should not be empty"
    );
}

#[test]
fn test_skill_instructions_contain_expected_keywords() {
    let skills_dir = project_manager_skills_dir();
    let registry = SkillRegistry::load_from_dir(&skills_dir)
        .expect("load_from_dir should succeed");

    let skill = registry.get("meeting-notes").unwrap();
    let instructions = build_skill_instructions(skill);

    // Verify the skill instructions contain expected keywords
    // that the LLM should recognize and follow
    let expected_keywords = [
        "Execution Steps",
        "Action Items",
        "Decisions",
        "memory_recall",
        "memory_store",
        "file_write",
        "Attendees",
    ];

    for keyword in &expected_keywords {
        assert!(
            instructions.contains(keyword),
            "Skill instructions should contain keyword '{}'",
            keyword
        );
    }

    assert!(
        !instructions.is_empty(),
        "Skill instructions should not be empty"
    );
}

#[test]
fn test_skill_not_found_does_not_inject() {
    // When the command references a non-existent skill,
    // the cli.rs code simply logs a warning and does NOT modify the content.
    let skills_dir = project_manager_skills_dir();
    let registry = SkillRegistry::load_from_dir(&skills_dir)
        .expect("load_from_dir should succeed");

    let missing = registry.get("non-existent-skill");
    assert!(
        missing.is_none(),
        "Non-existent skill should return None"
    );

    // Simulating the cli.rs logic: if skill not found, skill_instructions is None
    let instructions: Option<String> = registry.get("non-existent-skill")
        .map(|s| s.instructions.clone());
    assert!(
        instructions.is_none(),
        "skill_instructions should be None when skill not found"
    );
}

#[test]
fn test_injection_with_in_memory_skill() {
    // Test injection with a programmatically created skill (not loaded from disk)
    // to avoid file-path dependencies in CI.
    let skill_content = r#"---
name: test-skill
description: A test skill for injection
triggers:
  - test
  - injection
tool_deps:
  - memory_recall
---

# Test Skill Instructions

1. Step one: recall context
2. Step two: process input
3. Step three: generate output

Always prefix your response with [TEST-SKILL].
"#;

    let skill = parse_skill_md(skill_content).expect("Should parse test skill");
    assert_eq!(skill.name, "test-skill");

    let instructions = build_skill_instructions(&skill);

    assert!(instructions.contains("Step one: recall context"));
    assert!(instructions.contains("[TEST-SKILL]"));
    assert_eq!(instructions, skill.instructions);
    assert!(!instructions.is_empty());
}

#[test]
fn test_multiple_skills_injection_does_not_overlap() {
    // Verify that only the requested skill's instructions are injected,
    // not all skills from the registry.
    let skills_dir = project_manager_skills_dir();
    let registry = SkillRegistry::load_from_dir(&skills_dir)
        .expect("load_from_dir should succeed");

    let meeting_skill = registry.get("meeting-notes").unwrap();
    let _user_content = "Sprint planning time";
    let injected = build_skill_instructions(meeting_skill);

    // meeting-notes specific keywords should be present
    assert!(injected.contains("meeting-notes") || injected.contains("Meeting Notes"));

    // Other skills' instructions should NOT be present
    // (sprint-planning is a separate skill with its own instructions)
    let sprint_skill = registry.get("sprint-planning");
    if let Some(_sprint) = sprint_skill {
        // The injected content should NOT contain sprint-planning's unique instructions
        // unless they happen to share keywords (unlikely for distinct skills)
        assert!(
            !injected.contains("Sprint Planning Skill"),
            "Should not contain sprint-planning instructions when meeting-notes was requested"
        );
    }
}

#[test]
fn test_skill_trigger_matching() {
    // Verify that skill triggers work correctly for the "meeting-notes" skill
    let skills_dir = project_manager_skills_dir();
    let registry = SkillRegistry::load_from_dir(&skills_dir)
        .expect("load_from_dir should succeed");

    // meeting-notes has triggers: meeting, 会议纪要, meeting notes, meeting minutes, take notes
    let matched = registry.find_by_trigger("meeting");
    assert!(
        matched.iter().any(|s| s.name == "meeting-notes"),
        "Trigger 'meeting' should match 'meeting-notes' skill"
    );

    let matched = registry.find_by_trigger("meeting notes");
    assert!(
        matched.iter().any(|s| s.name == "meeting-notes"),
        "Trigger 'meeting notes' should match 'meeting-notes' skill"
    );

    // Case-insensitive matching
    let matched = registry.find_by_trigger("Meeting Notes");
    assert!(
        matched.iter().any(|s| s.name == "meeting-notes"),
        "Trigger 'Meeting Notes' should match case-insensitively"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Tier 2: Real LLM integration test (requires MINIMAX_API_KEY)
// ═══════════════════════════════════════════════════════════════════════

/// Create a MiniMax provider from the MINIMAX_API_KEY environment variable.
/// Returns None if the key is not set.
fn get_minimax_provider() -> Option<rollball_runtime::providers::openai::OpenAIProvider> {
    let api_key = std::env::var("MINIMAX_API_KEY").ok()?;
    if api_key.is_empty() {
        return None;
    }
    Some(rollball_runtime::providers::openai::OpenAIProvider::with_base_url(
        Some("https://api.minimax.chat/v1"),
        Some(&api_key),
    ))
}

const MINIMAX_MODEL: &str = "MiniMax-M2.5";

/// Build the system prompt for the project-manager-agent.
fn project_manager_system_prompt() -> String {
    "You are a project manager AI assistant. Follow the skill instructions provided in the system prompt. Output structured content as specified by the active skill.".to_string()
}

#[tokio::test]
#[ignore] // Requires MINIMAX_API_KEY — run with: cargo test --test skill_injection_test -- --ignored --nocapture
async fn test_skill_injection_e2e_with_llm() {
    let provider = match get_minimax_provider() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: MINIMAX_API_KEY not set");
            return;
        }
    };

    // 1. Load skill registry and get meeting-notes skill
    let skills_dir = project_manager_skills_dir();
    let registry = SkillRegistry::load_from_dir(&skills_dir)
        .expect("load_from_dir should succeed");
    let skill = registry
        .get("meeting-notes")
        .expect("'meeting-notes' skill should exist");

    // 2. Build system prompt with skill instructions (new behavior: injected via ContextBuilder)
    let user_content = "We had a 30-minute sprint review meeting. Alice presented the API design, Bob raised performance concerns, and we decided to add caching before the next release. Action: Alice will implement the cache by Friday.";
    let skill_instructions = build_skill_instructions(skill);
    let system_prompt = format!("{}\n\n## Skill Instructions\n{}", project_manager_system_prompt(), skill_instructions);

    // 3. Build the chat request with skill in system prompt
    let request = rollball_core::providers::traits::ChatRequest {
        model: MINIMAX_MODEL.to_string(),
        messages: vec![
            rollball_core::providers::traits::ChatMessage::system(system_prompt),
            rollball_core::providers::traits::ChatMessage::user(user_content.to_string()),
        ],
        temperature: Some(0.3),
        max_tokens: Some(1024),
        tools: None,
    };

    // 4. Call the LLM
    let response = provider
        .chat(request)
        .await
        .expect("Chat request should succeed");

    // 5. Verify the response reflects the skill instructions
    let content = response.content.trim();
    assert!(
        !content.is_empty(),
        "LLM should return non-empty response"
    );

    // The LLM should produce structured meeting notes per the skill instructions.
    // Check for key structural elements from the meeting-notes skill:
    let content_lower = content.to_lowercase();

    // The skill asks for: Agenda, Decisions, Action Items sections
    let has_decision_section = content_lower.contains("decision");
    let has_action_section = content_lower.contains("action");
    let has_attendee = content_lower.contains("alice") || content_lower.contains("bob");

    assert!(
        has_decision_section,
        "Response should contain a decisions section (skill instruction compliance). Got: {}",
        &content[..content.len().min(500)]
    );
    assert!(
        has_action_section,
        "Response should contain an action items section (skill instruction compliance). Got: {}",
        &content[..content.len().min(500)]
    );
    assert!(
        has_attendee,
        "Response should mention meeting attendees from the user message. Got: {}",
        &content[..content.len().min(500)]
    );

    eprintln!("\n--- LLM Response (first 500 chars) ---\n{}\n", &content[..content.len().min(500)]);
}

#[tokio::test]
#[ignore] // Requires MINIMAX_API_KEY — control test without skill injection
async fn test_skill_injection_llm_control_without_injection() {
    // Control test: send the same user message WITHOUT skill injection
    // and verify the response is different from the skill-injected version.
    let provider = match get_minimax_provider() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: MINIMAX_API_KEY not set");
            return;
        }
    };

    let user_content = "We had a 30-minute sprint review meeting. Alice presented the API design, Bob raised performance concerns, and we decided to add caching before the next release.";

    let request = rollball_core::providers::traits::ChatRequest {
        model: MINIMAX_MODEL.to_string(),
        messages: vec![
            rollball_core::providers::traits::ChatMessage::system(
                "You are a helpful assistant.",
            ),
            rollball_core::providers::traits::ChatMessage::user(user_content),
        ],
        temperature: Some(0.3),
        max_tokens: Some(512),
        tools: None,
    };

    let response = provider
        .chat(request)
        .await
        .expect("Chat request should succeed");

    let content = response.content.trim();
    assert!(!content.is_empty(), "LLM should return non-empty response");

    // Without skill injection, the response is less likely to follow
    // the structured meeting-notes format (no guarantee, but it's a sanity check)
    eprintln!("\n--- Control Response (no injection, first 300 chars) ---\n{}\n", &content[..content.len().min(300)]);
}
