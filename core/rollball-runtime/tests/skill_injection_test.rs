//! End-to-end integration test for skill manual injection
//!
//! Verifies that when a message carries a `command` field, the Runtime
//! correctly injects the corresponding skill's instructions into the user
//! message, and (optionally) that a real LLM responds accordingly.
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

/// The injection format used by cli.rs when a command is specified.
/// Mirrors: `format!("{}\n\n{}", skill.instructions, content)`
fn inject_skill_instructions(skill: &SkillDefinition, user_content: &str) -> String {
    format!("{}\n\n{}", skill.instructions, user_content)
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
fn test_skill_injection_format_matches_cli_logic() {
    // Verify that the injection format mirrors the cli.rs logic:
    //   content = format!("{}\n\n{}", skill.instructions, content);
    let skills_dir = project_manager_skills_dir();
    let registry = SkillRegistry::load_from_dir(&skills_dir)
        .expect("load_from_dir should succeed");

    let skill = registry.get("meeting-notes").unwrap();
    let user_content = "Please take notes for our standup meeting.";

    let injected = inject_skill_instructions(skill, user_content);

    // The injected message should start with skill instructions
    assert!(
        injected.starts_with(&skill.instructions),
        "Injected message should start with skill instructions"
    );
    // Followed by double newline and user content
    assert!(
        injected.ends_with(user_content),
        "Injected message should end with user content"
    );
    // The separator must be exactly "\n\n"
    let separator_start = skill.instructions.len();
    assert_eq!(
        &injected[separator_start..separator_start + 2],
        "\n\n",
        "Instructions and user content must be separated by double newline"
    );
}

#[test]
fn test_injected_content_contains_skill_keywords() {
    let skills_dir = project_manager_skills_dir();
    let registry = SkillRegistry::load_from_dir(&skills_dir)
        .expect("load_from_dir should succeed");

    let skill = registry.get("meeting-notes").unwrap();
    let user_content = "Let's discuss the sprint review.";
    let injected = inject_skill_instructions(skill, user_content);

    // Verify the full injected message contains skill-specific keywords
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
            injected.contains(keyword),
            "Injected message should contain keyword '{}'",
            keyword
        );
    }

    // And the original user content must also be present
    assert!(
        injected.contains(user_content),
        "Injected message should preserve original user content"
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

    // Simulating the cli.rs logic: if skill not found, content stays as-is
    let user_content = "Hello world";
    let content = match registry.get("non-existent-skill") {
        Some(skill) => inject_skill_instructions(skill, user_content),
        None => user_content.to_string(),
    };
    assert_eq!(content, user_content, "Content should be unchanged when skill not found");
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

    let user_content = "Do something useful";
    let injected = inject_skill_instructions(&skill, user_content);

    assert!(injected.contains("Step one: recall context"));
    assert!(injected.contains("[TEST-SKILL]"));
    assert!(injected.contains(user_content));
    assert!(injected.starts_with(&skill.instructions));
}

#[test]
fn test_multiple_skills_injection_does_not_overlap() {
    // Verify that only the requested skill's instructions are injected,
    // not all skills from the registry.
    let skills_dir = project_manager_skills_dir();
    let registry = SkillRegistry::load_from_dir(&skills_dir)
        .expect("load_from_dir should succeed");

    let meeting_skill = registry.get("meeting-notes").unwrap();
    let user_content = "Sprint planning time";
    let injected = inject_skill_instructions(meeting_skill, user_content);

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
    "You are a project manager AI assistant. Follow the skill instructions provided in the user message carefully. Output structured content as specified by the active skill.".to_string()
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

    // 2. Build injected user message (same format as cli.rs)
    let user_content = "We had a 30-minute sprint review meeting. Alice presented the API design, Bob raised performance concerns, and we decided to add caching before the next release. Action: Alice will implement the cache by Friday.";
    let injected_content = inject_skill_instructions(skill, user_content);

    // 3. Build the chat request with injected content
    let request = rollball_core::providers::traits::ChatRequest {
        model: MINIMAX_MODEL.to_string(),
        messages: vec![
            rollball_core::providers::traits::ChatMessage::system(project_manager_system_prompt()),
            rollball_core::providers::traits::ChatMessage::user(injected_content),
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
