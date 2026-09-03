//! End-to-end tests: ADR-063 §3.5 — `prompts/` override HTTP API.
//!
//! Spins up a real `RuntimeHttpServer` on a random localhost port and
//! exercises the three handlers exactly the way the Desktop `PromptList`
//! does (and the way Gateway `proxy.rs` reverse-proxies them). Covers
//! the wire contract end-to-end — JSON shape, status codes, atomic write
//! semantics, path-traversal defence, and the `prompts/` directory
//! auto-materialization behaviour the Debug panel relies on.
//!
//! The reload endpoint (`POST /api/debug/prompts/reload`, proxied by
//! Gateway as `/api/agents/{id}/debug/prompts/reload`) is covered by the
//! sibling `prompts_reload_e2e.rs` — it needs a populated `DebugService`
//! slot which we mock there. This file only exercises the `prompts/`
//! routes, which never touch `DebugService`.
//!
//! Tests in this file mirror the `shell_risk_e2e.rs` minimal-server
//! pattern: one `#[tokio::test]` per scenario, no shared fixture, every
//! test owns its own temp dir so they can run in parallel.

use std::sync::Arc;

const AGENT_ID: &str = "com.test.prompts-e2e";

async fn spawn_server(tag: &str) -> (u16, std::path::PathBuf) {
    let temp_dir = std::env::temp_dir().join(format!(
        "acowork-test-prompts-e2e-{}-{}",
        std::process::id(),
        tag
    ));
    let _ = std::fs::remove_dir_all(&temp_dir);
    std::fs::create_dir_all(&temp_dir).unwrap();

    // Minimal stub slots — see `shell_risk_e2e.rs` for why each is
    // `None` / empty; the prompts routes don't touch any of them.
    let snapshots = Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
    let latest = Arc::new(std::sync::RwLock::new(None));
    let dispatch_tx = Arc::new(tokio::sync::Mutex::new(None));
    let embed_dim = Arc::new(std::sync::RwLock::new(0));
    let degraded_reasons = Arc::new(std::sync::RwLock::new(Vec::new()));
    let mqtt_client = Arc::new(tokio::sync::Mutex::new(None));
    let session_metadata = Arc::new(tokio::sync::Mutex::new(None));
    let memory_query = Arc::new(tokio::sync::Mutex::new(None));
    let workspace_query = Arc::new(tokio::sync::Mutex::new(None));
    let workspace_mutation = Arc::new(tokio::sync::Mutex::new(None));
    let agent_tools = Arc::new(tokio::sync::Mutex::new(None));
    let agent_config = Arc::new(tokio::sync::Mutex::new(None));
    let attachment = Arc::new(tokio::sync::Mutex::new(None));
    let session_config = Arc::new(tokio::sync::Mutex::new(None));
    let consolidation_timer: Arc<
        std::sync::RwLock<Option<Arc<acowork_runtime::memory::ConsolidationTimer>>>,
    > = Arc::new(std::sync::RwLock::new(None));
    let rag_provider: Arc<std::sync::RwLock<Option<Arc<dyn acowork_core::rag::RagProvider>>>> =
        Arc::new(std::sync::RwLock::new(None));
    let debug_service = Arc::new(tokio::sync::Mutex::new(None));
    let workspace_resolver: Arc<
        std::sync::RwLock<acowork_runtime::tools::workspace_resolver::WorkspaceResolver>,
    > = Arc::new(std::sync::RwLock::new(
        acowork_runtime::tools::workspace_resolver::WorkspaceResolver::new_for_test(vec![]),
    ));
    let session_manager_slot: Arc<
        tokio::sync::RwLock<
            Option<Arc<tokio::sync::Mutex<acowork_runtime::agent::session::SessionManager>>>,
        >,
    > = Arc::new(tokio::sync::RwLock::new(None));

    let server = acowork_runtime::http::RuntimeHttpServer::start(
        temp_dir.clone(),
        temp_dir.clone(), // package_dir (ADR-063): same dir; tests create prompts/ inside
        AGENT_ID.to_string(),
        snapshots,
        latest,
        dispatch_tx,
        embed_dim,
        degraded_reasons,
        mqtt_client,
        session_metadata,
        memory_query,
        workspace_query,
        workspace_mutation,
        agent_tools,
        agent_config,
        attachment,
        session_config,
        consolidation_timer,
        rag_provider,
        debug_service,
        workspace_resolver,
        session_manager_slot,
        std::sync::Arc::new(std::sync::RwLock::new(None)), // no AgentCore for basic tests
    )
    .await
    .expect("runtime http server should start");

    (server.port, temp_dir)
}

// ── list ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_list_prompts_returns_all_9_with_overridden_false() {
    let (port, temp_dir) = spawn_server("list-all-9").await;

    let resp = reqwest::get(format!("http://127.0.0.1:{}/agents/{}/prompts", port, AGENT_ID))
        .await
        .expect("GET should not error");
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["agent_id"], AGENT_ID);

    let prompts = body["prompts"].as_array().expect("prompts must be an array");
    assert_eq!(
        prompts.len(),
        9,
        "ADR-063 §3.2 contract: 9 overridable prompts must always be advertised"
    );

    // Every entry must be `overridden=false, size_bytes=0` because the
    // test's temp_dir/prompts/ does not exist (and no test in this run
    // created it before this one — see per-test temp_dir naming).
    for p in prompts {
        let name = p["name"].as_str().expect("name");
        assert!(!p["overridden"].as_bool().unwrap_or(true), "{name} must report overridden=false on a fresh package dir");
        assert_eq!(
            p["size_bytes"].as_u64().unwrap_or(99),
            0,
            "{name} must report size_bytes=0 when no override file exists"
        );
        // Each entry must carry the fallback constant name — that's
        // what the Debug panel renders as the "built-in default" hint.
        assert!(
            p["fallback_constant"].is_string(),
            "{name} must expose fallback_constant as a string"
        );
        // ADR-063 §3.7 Debug panel wire contract: every entry MUST
        // also expose a `purpose` string (user-facing usage hint).
        // The Desktop renders this directly under the prompt name;
        // a missing or empty value would render as a blank row.
        let purpose = p["purpose"]
            .as_str()
            .unwrap_or_else(|| panic!("{name} must expose purpose as a string"));
        assert!(
            !purpose.trim().is_empty(),
            "{name} purpose must be non-empty (operators rely on it)"
        );
    }

    // Spot-check the 9 names by sorting the response — keeps the test
    // resilient to reordering of `PROMPT_ENTRIES` in prompts.rs.
    let mut names: Vec<&str> = prompts
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec![
            "abstention",
            "compact-template",
            "conflict-classification",
            "extraction",
            "fallback",
            "generalization",
            "search",
            "summary",
            "title",
        ],
        "the 9 names must be exactly the canonical set"
    );

    // Cleanup so the per-test temp dir doesn't accumulate.
    let _ = std::fs::remove_dir_all(&temp_dir);
}

// ── get ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_get_prompt_unknown_name_returns_404_with_canonical_list() {
    let (port, _temp) = spawn_server("get-unknown").await;

    let resp = reqwest::get(format!(
        "http://127.0.0.1:{}/agents/{}/prompts/{}",
        port,
        AGENT_ID,
        "not-a-real-prompt",
    ))
    .await
    .expect("GET should not error");
    assert_eq!(resp.status(), 404, "unknown prompt must return 404");

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "unknown_prompt");
    // The error message lists the 9 canonical names so operators can
    // see what they should have typed.
    let msg = body["message"].as_str().unwrap_or("");
    for canonical in [
        "summary",
        "fallback",
        "search",
        "compact-template",
        "title",
        "extraction",
        "conflict-classification",
        "generalization",
        "abstention",
    ] {
        assert!(
            msg.contains(canonical),
            "error message must list `{canonical}` so operators can recover; got: {msg}"
        );
    }
}

#[tokio::test]
async fn test_get_prompt_path_traversal_returns_404() {
    let (port, _temp) = spawn_server("get-traversal").await;

    // `%2F` is `/`, `%2E%2E` is `..` — axum's Path extractor decodes
    // them before our handler sees them, so the handler's
    // `resolve_prompt_path` defence kicks in.
    let resp = reqwest::get(format!(
        "http://127.0.0.1:{}/agents/{}/prompts/{}",
        port,
        AGENT_ID,
        "..%2F..%2Fetc%2Fpasswd",
    ))
    .await
    .expect("GET should not error");
    assert_eq!(resp.status(), 404, "path-traversal must be rejected");

    // Backslash variant — Windows-style separator. reqwest normalizes
    // backslashes on the client, but the server-side defence rejects
    // them too. Use raw URL form to be explicit.
    let resp = reqwest::get(format!(
        "http://127.0.0.1:{}/agents/{}/prompts/{}",
        port,
        AGENT_ID,
        "..%5C..%5Cetc%5Cpasswd",
    ))
    .await
    .expect("GET should not error");
    assert_eq!(resp.status(), 404, "backslash traversal must be rejected");
}

#[tokio::test]
async fn test_get_prompt_case_variant_returns_404() {
    // Symmetry with `is_overridable_prompt` case-sensitivity test in
    // prompt_builder.rs — only the exact basename `summary` resolves;
    // `Summary` and `SUMMARY` must NOT.
    let (port, _temp) = spawn_server("get-case").await;

    for variant in ["Summary", "SUMMARY", "sUmMaRy"] {
        let resp = reqwest::get(format!(
            "http://127.0.0.1:{}/agents/{}/prompts/{}",
            port, AGENT_ID, variant,
        ))
        .await
        .expect("GET should not error");
        assert_eq!(
            resp.status(),
            404,
            "case variant `{variant}` must not resolve (lookup_entry is case-sensitive)"
        );
    }
}

#[tokio::test]
async fn test_get_prompt_existing_override_returns_content() {
    // Pre-populate one override file, then GET it through the handler.
    let (port, temp_dir) = spawn_server("get-existing").await;
    let prompts_dir = temp_dir.join("prompts");
    std::fs::create_dir_all(&prompts_dir).unwrap();
    let payload = "USER OVERRIDE: be terse.\n# multi-line";
    std::fs::write(prompts_dir.join("compact-template.md"), payload).unwrap();

    let resp = reqwest::get(format!(
        "http://127.0.0.1:{}/agents/{}/prompts/{}",
        port, AGENT_ID, "compact-template",
    ))
    .await
    .expect("GET should not error");
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["name"], "compact-template");
    assert_eq!(body["file"], "compact-template.md");
    assert_eq!(body["overridden"], true);
    assert_eq!(
        body["content"].as_str().expect("content must be present when overridden"),
        payload,
    );
    // size_bytes reflects on-disk file size (NOT the trimmed content
    // length — they're equal here because the trailing newline is the
    // only trim target and we kept it for the test).
    assert!(body["size_bytes"].as_u64().unwrap() > 0);
}

// ── put ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_put_then_get_roundtrip() {
    let (port, temp_dir) = spawn_server("put-roundtrip").await;

    let payload = "REWRITTEN: always respond in haiku.";
    let client = reqwest::Client::new();
    let resp = client
        .put(format!(
            "http://127.0.0.1:{}/agents/{}/prompts/{}",
            port, AGENT_ID, "compact-template",
        ))
        .json(&serde_json::json!({ "content": payload }))
        .send()
        .await
        .expect("PUT should not error");
    assert_eq!(resp.status(), 200);
    let put_body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(put_body["accepted"], true);
    assert_eq!(
        put_body["reload_required"], true,
        "PUT must signal that the live AgentCore was NOT updated (reload is a separate step)"
    );
    assert_eq!(put_body["name"], "compact-template");
    assert_eq!(put_body["file"], "compact-template.md");

    // On-disk file must exist with the exact payload.
    let on_disk = std::fs::read_to_string(temp_dir.join("prompts").join("compact-template.md"))
        .expect("PUT must write the file");
    assert_eq!(on_disk, payload);

    // GET roundtrip must return the just-written content.
    let resp = reqwest::get(format!(
        "http://127.0.0.1:{}/agents/{}/prompts/{}",
        port, AGENT_ID, "compact-template",
    ))
    .await
    .expect("GET should not error");
    let get_body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(get_body["content"].as_str().unwrap(), payload);
    assert_eq!(get_body["overridden"], true);
}

#[tokio::test]
async fn test_put_creates_prompts_dir_when_missing() {
    // The Debug panel can PUT before the operator ever wrote a single
    // prompt file — `prompts/` does not exist yet on a fresh install.
    // The handler must `create_dir_all` it instead of failing with
    // ENOENT. This mirrors the "first-write materialization" pattern
    // in `get_shell_risk_rules`.
    let (port, temp_dir) = spawn_server("put-mkdir").await;
    assert!(
        !temp_dir.join("prompts").exists(),
        "test precondition: prompts/ must NOT exist before the PUT"
    );

    let client = reqwest::Client::new();
    let resp = client
        .put(format!(
            "http://127.0.0.1:{}/agents/{}/prompts/{}",
            port, AGENT_ID, "summary",
        ))
        .json(&serde_json::json!({ "content": "fresh install override\n" }))
        .send()
        .await
        .expect("PUT should not error");
    assert_eq!(resp.status(), 200, "PUT must succeed even when prompts/ doesn't exist yet");
    assert!(temp_dir.join("prompts").is_dir(), "prompts/ must be created");
    assert!(temp_dir.join("prompts").join("summary.md").is_file());
}

#[tokio::test]
async fn test_put_unknown_prompt_returns_404() {
    let (port, _temp) = spawn_server("put-unknown").await;
    let client = reqwest::Client::new();
    let resp = client
        .put(format!(
            "http://127.0.0.1:{}/agents/{}/prompts/{}",
            port, AGENT_ID, "not-a-prompt",
        ))
        .json(&serde_json::json!({ "content": "x" }))
        .send()
        .await
        .expect("PUT should not error");
    assert_eq!(resp.status(), 404);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "unknown_prompt");
}

#[tokio::test]
async fn test_put_empty_content_returns_400() {
    // ADR-063 — empty / whitespace-only PUT is rejected (would erase
    // the override without intent). The operator should delete the
    // file on disk directly if that's what they want.
    let (port, temp_dir) = spawn_server("put-empty").await;
    let client = reqwest::Client::new();

    for empty in ["", "   ", "\n\n\t  \n"] {
        let resp = client
            .put(format!(
                "http://127.0.0.1:{}/agents/{}/prompts/{}",
                port, AGENT_ID, "summary",
            ))
            .json(&serde_json::json!({ "content": empty }))
            .send()
            .await
            .expect("PUT should not error");
        assert_eq!(
            resp.status(),
            400,
            "empty/whitespace content `{empty:?}` must be rejected with 400"
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["error"], "empty_content");
    }

    // No file must have been written.
    assert!(
        !temp_dir.join("prompts").join("summary.md").exists(),
        "rejected PUT must not create the file"
    );
}

#[tokio::test]
async fn test_put_path_traversal_returns_404() {
    // Even via PUT, path traversal must be blocked before any write
    // hits the filesystem. %2F → `/` decodes to a name containing `/`,
    // which `resolve_prompt_path` rejects.
    let (port, temp_dir) = spawn_server("put-traversal").await;
    let client = reqwest::Client::new();

    for malicious in [
        "..%2F..%2Fetc%2Fpasswd",     // forward slashes
        "..%5C..%5Cboot.ini",         // backslashes (Windows)
        "summary%2F..%2F..%2Fescape", // embedded `..`
    ] {
        let resp = client
            .put(format!(
                "http://127.0.0.1:{}/agents/{}/prompts/{}",
                port, AGENT_ID, malicious,
            ))
            .json(&serde_json::json!({ "content": "pwned" }))
            .send()
            .await
            .expect("PUT should not error");
        assert_eq!(
            resp.status(),
            404,
            "malicious name `{malicious}` must return 404 (not 500, not 200)"
        );
    }

    // Crucial: no file may have been created anywhere in the package
    // dir from these attempts.
    let prompts_dir = temp_dir.join("prompts");
    if prompts_dir.exists() {
        let entries: Vec<_> = std::fs::read_dir(&prompts_dir)
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert!(
            entries.is_empty(),
            "no traversal attempts may have created files; found: {:?}",
            entries.iter().map(|e| e.path()).collect::<Vec<_>>()
        );
    }
}

// ── cross-cutting ──────────────────────────────────────────────────────

#[tokio::test]
async fn test_agent_id_mismatch_returns_404() {
    // Cross-process guard (ADR-034): if the path's agent_id differs
    // from the runtime's, the request targets the wrong agent and must
    // be rejected — not silently written elsewhere.
    let (port, _temp) = spawn_server("id-mismatch").await;
    let client = reqwest::Client::new();
    let other = "com.someone-elses.agent";

    // GET list
    let resp = reqwest::get(format!(
        "http://127.0.0.1:{}/agents/{}/prompts",
        port, other,
    ))
    .await
    .unwrap();
    assert_eq!(resp.status(), 404, "GET list with mismatched id must be 404");

    // GET single
    let resp = reqwest::get(format!(
        "http://127.0.0.1:{}/agents/{}/prompts/summary",
        port, other,
    ))
    .await
    .unwrap();
    assert_eq!(resp.status(), 404, "GET single with mismatched id must be 404");

    // PUT
    let resp = client
        .put(format!(
            "http://127.0.0.1:{}/agents/{}/prompts/summary",
            port, other,
        ))
        .json(&serde_json::json!({ "content": "leak attempt" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404, "PUT with mismatched id must be 404 (never write)");
}

#[tokio::test]
async fn test_put_does_not_mutate_other_prompts_overridden_state() {
    // After PUTting prompt A, listing must report `overridden=true`
    // ONLY for A; the other 8 must remain `overridden=false`. This
    // pins down that PUT does not accidentally re-touch sibling files.
    let (port, _temp) = spawn_server("put-isolation").await;
    let client = reqwest::Client::new();

    let resp = client
        .put(format!(
            "http://127.0.0.1:{}/agents/{}/prompts/{}",
            port, AGENT_ID, "title",
        ))
        .json(&serde_json::json!({ "content": "title-override\n" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let resp = reqwest::get(format!(
        "http://127.0.0.1:{}/agents/{}/prompts",
        port, AGENT_ID,
    ))
    .await
    .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    let prompts = body["prompts"].as_array().unwrap();

    let overridden_names: Vec<&str> = prompts
        .iter()
        .filter(|p| p["overridden"].as_bool().unwrap_or(false))
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        overridden_names,
        vec!["title"],
        "exactly one prompt must be marked overridden after a single PUT"
    );
}
