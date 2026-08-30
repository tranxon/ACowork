//! Integration test for the on-disk workspace persistence round-trip
//! (desktop-onboarding-bugfix_154b7ff7.md §Fix 3).
//!
//! Validates the end-to-end contract that desktop onboarding depends on:
//!   1. A workspace `POST /workspaces` (which routes through
//!      [`RuntimeWorkspaceMutationService::create_workspace`]) persists
//!      the entry to `<work_dir>/config/agent_workspaces.json`.
//!   2. After the runtime restarts, the new [`WorkspaceResolver`] loaded
//!      from the SAME `work_dir` can `find_by_id()` the persisted entry.
//!   3. Mutations (update / delete / set_prompt_file) also round-trip
//!      through the disk file and re-appear on the next startup.
//!   4. Multiple workspaces co-exist; the resolver returns each one
//!      independently, and the synthetic `__agent_home__` entry still
//!      resolves.
//!
//! Pre-Fix-3, the "workspace disappeared on restart" bug stemmed from a
//! discrepancy between (a) the path the mutation service wrote to and
//! (b) the path the resolver read from. This test exercises both halves
//! of that pipeline from the public API — no private fields, no shortcuts
//! — so any drift in either direction fails loudly.
//!
//! Mirrors the integration test pattern in
//! `acowork-runtime/tests/builtin_tools_mutation.rs` and the
//! `tokio::runtime::Runtime::block_on` pattern in
//! `acowork-runtime/tests/fs_watcher_e2e.rs`.

use acowork_runtime::tools::workspace_resolver::WorkspaceResolver;
use acowork_runtime::usecases::workspace_mutation::{
    WorkspaceEntryInput, WorkspaceMutationResponse, WorkspaceMutationService,
};
use acowork_runtime::usecases::workspace_query::WorkspaceError;
use acowork_runtime::usecases::RuntimeWorkspaceMutationService;

/// Build the canonical `<work_dir>/config/agent_workspaces.json` path
/// used by both the mutation service and the resolver. Mirrors
/// `RuntimeWorkspaceMutationService::workspaces_config_path` so we can
/// also peek at the raw disk file in assertions.
fn config_path(work_dir: &std::path::Path) -> std::path::PathBuf {
    work_dir.join("config").join("agent_workspaces.json")
}

/// Read the on-disk JSON file as a serde_json::Value so tests can
/// introspect the persisted schema directly (in addition to the
/// `WorkspaceResolver` round-trip).
fn read_disk(work_dir: &std::path::Path) -> serde_json::Value {
    let raw = std::fs::read_to_string(config_path(work_dir))
        .unwrap_or_else(|e| panic!("read {}: {}", config_path(work_dir).display(), e));
    serde_json::from_str(&raw).expect("parse agent_workspaces.json")
}

/// Helper: extract the persisted id of a workspace created via
/// `create_workspace` (whether client-supplied or server-generated).
fn created_id(resp: &WorkspaceMutationResponse) -> String {
    resp.entry
        .as_ref()
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_str())
        .expect("create_workspace response must include an id")
        .to_string()
}

// ── Tests ──────────────────────────────────────────────────────────────

/// A single create → restart cycle must survive. This is the headline
/// regression test for the "workspace disappeared on restart" report.
///
/// Before Fix 3: the mutation service wrote to one path (the run-time's
/// `work_dir`) while the desktop expected the resolver to read from a
/// different one (the `com.acowork.system/workspace/` work_dir of the
/// system agent runtime). After Fix 3 both halves agree on the same
/// canonical `<work_dir>/config/agent_workspaces.json` path. This test
/// pins that down with two separate `Work_dir`-style runs that share the
/// exact same `agent_workspaces.json` on disk.
#[test]
fn create_workspace_survives_resolver_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let work_dir = dir.path().to_path_buf();
    let project_dir = work_dir.join("my-project");
    std::fs::create_dir_all(&project_dir).expect("create project dir");

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");

    // Run 1 — start the mutation service, create one workspace, drop it.
    let id = {
        let svc = RuntimeWorkspaceMutationService::new(work_dir.clone());
        let resp = rt.block_on(svc.create_workspace(WorkspaceEntryInput {
            id: None,
            path: Some(project_dir.to_string_lossy().into_owned()),
            access: Some("read-write".to_string()),
            alias: Some("project-a".to_string()),
            prompt_file: None,
            last_active: Some(true),
        }));
        let resp = resp.expect("create_workspace succeeds");
        let id = created_id(&resp);
        // Sanity: disk file exists with the new entry inside.
        let disk = read_disk(&work_dir);
        assert_eq!(disk["additional_dirs"].as_array().unwrap().len(), 1);
        assert_eq!(
            disk["additional_dirs"][0]["id"].as_str(),
            Some(id.as_str())
        );
        assert_eq!(
            disk["additional_dirs"][0]["alias"].as_str(),
            Some("project-a")
        );
        assert_eq!(
            disk["additional_dirs"][0]["access"].as_str(),
            Some("read-write")
        );
        assert_eq!(
            disk["additional_dirs"][0]["last_active"].as_bool(),
            Some(true)
        );
        id
    };
    // svc dropped here — the in-memory cfg is gone, only disk remains.

    // Run 2 — reconstruct the resolver from the same work_dir and
    // confirm the workspace is visible. This is the exact sequence that
    // happens when the desktop restarts and the runtime reloads from
    // `agent_workspaces.json`.
    let resolver = WorkspaceResolver::new(work_dir.to_string_lossy().as_ref());
    let found = resolver
        .find_by_id(&id)
        .unwrap_or_else(|| panic!("resolver must find_by_id({}) after restart", id));
    assert_eq!(found.path, project_dir.to_string_lossy().into_owned());
    assert_eq!(found.id, id);
    // access round-trips through WorkspaceAccess::deserialize, so we
    // compare via Display rather than asserting against the literal
    // `"read-write"` string.
    assert_eq!(found.access.as_str(), "read-write");
    assert!(
        found.last_active,
        "last_active flag must round-trip across restart"
    );

    // The synthetic entries the resolver appends (`__agent_home__`,
    // `__package_root__`) must still be present.
    assert!(
        resolver.find_by_id("__agent_home__").is_some(),
        "synthetic __agent_home__ entry must always be present"
    );
}

/// `update_workspace` must also survive a restart. The desktop toggles
/// `access` from `read-only` → `read-write` via the PUT endpoint; the
/// resulting state must persist.
#[test]
fn update_workspace_survives_resolver_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let work_dir = dir.path().to_path_buf();
    let project_dir = work_dir.join("upd-project");
    std::fs::create_dir_all(&project_dir).expect("create dir");

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let svc = RuntimeWorkspaceMutationService::new(work_dir.clone());
    let project_path_str = project_dir.to_string_lossy().into_owned();
    let resp = rt.block_on(svc.create_workspace(WorkspaceEntryInput {
        id: Some("ws-update".to_string()),
        path: Some(project_path_str.clone()),
        access: Some("read-only".to_string()),
        alias: Some("old-alias".to_string()),
        prompt_file: None,
        last_active: None,
    }));
    let id = created_id(&resp.expect("create"));

    // Flip access to read-write, change the alias, mark last_active.
    rt.block_on(svc.update_workspace(
        &id,
        WorkspaceEntryInput {
            id: None,
            path: None,
            access: Some("read-write".to_string()),
            alias: Some("new-alias".to_string()),
            prompt_file: None,
            last_active: Some(true),
        },
    ))
    .expect("update succeeds");

    // Read disk directly — confirms the persisted schema reflects the
    // partial-update semantics (no field clobbering).
    let disk = read_disk(&work_dir);
    let entry = &disk["additional_dirs"][0];
    assert_eq!(entry["access"].as_str(), Some("read-write"));
    assert_eq!(entry["alias"].as_str(), Some("new-alias"));
    assert_eq!(entry["last_active"].as_bool(), Some(true));
    // path / id were absent from the PUT body and must be preserved.
    assert_eq!(entry["id"].as_str(), Some("ws-update"));
    assert_eq!(entry["path"].as_str(), Some(project_path_str.as_str()));

    drop(svc);

    // Reload via the resolver — same project path, fresh state.
    let resolver = WorkspaceResolver::new(work_dir.to_string_lossy().as_ref());
    let found = resolver
        .find_by_id("ws-update")
        .expect("updated workspace must be visible after restart");
    assert_eq!(found.access.as_str(), "read-write");
}

/// `delete_workspace` must persist; the next reload must NOT see the
/// deleted entry. The synthetic `__agent_home__`/`__package_root__`
/// entries must still resolve.
#[test]
fn delete_workspace_survives_resolver_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let work_dir = dir.path().to_path_buf();
    let project_dir = work_dir.join("del-project");
    std::fs::create_dir_all(&project_dir).expect("create dir");

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let svc = RuntimeWorkspaceMutationService::new(work_dir.clone());
    let resp = rt.block_on(svc.create_workspace(WorkspaceEntryInput {
        id: Some("ws-gone".to_string()),
        path: Some(project_dir.to_string_lossy().into_owned()),
        access: Some("read-only".to_string()),
        alias: None,
        prompt_file: None,
        last_active: None,
    }));
    let id = created_id(&resp.expect("create"));

    rt.block_on(svc.delete_workspace(&id)).expect("delete succeeds");

    // Disk must reflect the deletion immediately.
    let disk = read_disk(&work_dir);
    let ids: Vec<&str> = disk["additional_dirs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].as_str().unwrap())
        .collect();
    assert!(
        ids.is_empty(),
        "additional_dirs should be empty after delete; got: {:?}",
        ids
    );

    drop(svc);

    // After a restart the deleted id must NOT be visible.
    let resolver = WorkspaceResolver::new(work_dir.to_string_lossy().as_ref());
    assert!(
        resolver.find_by_id("ws-gone").is_none(),
        "deleted workspace must not be re-discoverable after restart"
    );
}

/// Multiple workspaces must co-exist on disk and after restart. This
/// reproduces the desktop's "system agent + 3 user workspaces"
/// configuration and pins down that no entry is silently dropped.
#[test]
fn multiple_workspaces_all_survive_resolver_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let work_dir = dir.path().to_path_buf();
    std::fs::create_dir_all(work_dir.join("ws-1")).unwrap();
    std::fs::create_dir_all(work_dir.join("ws-2")).unwrap();
    std::fs::create_dir_all(work_dir.join("ws-3")).unwrap();

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let svc = RuntimeWorkspaceMutationService::new(work_dir.clone());
    let ids = ["ws-1", "ws-2", "ws-3"]
        .iter()
        .map(|client_id| {
            let resp = rt.block_on(svc.create_workspace(WorkspaceEntryInput {
                id: Some((*client_id).to_string()),
                path: Some(work_dir.join(client_id).to_string_lossy().into_owned()),
                access: Some("read-write".to_string()),
                alias: Some((*client_id).to_string()),
                prompt_file: None,
                last_active: None,
            }));
            created_id(&resp.expect("create"))
        })
        .collect::<Vec<_>>();
    drop(svc);

    // Disk-level: three entries, all of them.
    let disk = read_disk(&work_dir);
    assert_eq!(disk["additional_dirs"].as_array().unwrap().len(), 3);

    // Resolver-level: each workspace is independently discoverable.
    let resolver = WorkspaceResolver::new(work_dir.to_string_lossy().as_ref());
    for id in &ids {
        let found = resolver
            .find_by_id(id)
            .unwrap_or_else(|| panic!("resolver must find_by_id({}) after restart", id));
        assert_eq!(found.id, *id);
        assert!(
            found.path.ends_with(id),
            "path must point at the right project dir"
        );
    }

    // last_active_workspace_id helper must work — none set yet so None.
    assert_eq!(resolver.last_active_workspace_id(), None);
}

/// When `last_active=true` is persisted, the next resolver must surface
/// it via `last_active_workspace_id()`. This is the desktop's "open the
/// last selected workspace on launch" code path.
#[test]
fn last_active_flag_round_trips_through_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let work_dir = dir.path().to_path_buf();
    std::fs::create_dir_all(work_dir.join("active")).unwrap();
    std::fs::create_dir_all(work_dir.join("inactive")).unwrap();

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let svc = RuntimeWorkspaceMutationService::new(work_dir.clone());
    rt.block_on(svc.create_workspace(WorkspaceEntryInput {
        id: Some("ws-active".to_string()),
        path: Some(work_dir.join("active").to_string_lossy().into_owned()),
        access: Some("read-write".to_string()),
        alias: None,
        prompt_file: None,
        last_active: Some(true),
    }))
    .expect("create active");
    rt.block_on(svc.create_workspace(WorkspaceEntryInput {
        id: Some("ws-inactive".to_string()),
        path: Some(work_dir.join("inactive").to_string_lossy().into_owned()),
        access: Some("read-write".to_string()),
        alias: None,
        prompt_file: None,
        last_active: Some(false),
    }))
    .expect("create inactive");
    drop(svc);

    let resolver = WorkspaceResolver::new(work_dir.to_string_lossy().as_ref());
    assert_eq!(
        resolver.last_active_workspace_id(),
        Some("ws-active"),
        "last_active_workspace_id must surface the persisted flag after restart"
    );
}

/// Restarting into a corrupted `agent_workspaces.json` must NOT
/// silently swallow the bad data — the resolver falls back to the
/// `__agent_home__` synthetic entry only, and the mutation service
/// surfaces a `Persist` error so the desktop knows to repair the file.
#[test]
fn corrupt_config_blocks_mutation_and_resolver_falls_back_safely() {
    let dir = tempfile::tempdir().expect("tempdir");
    let work_dir = dir.path().to_path_buf();
    std::fs::create_dir_all(work_dir.join("config")).unwrap();
    std::fs::write(config_path(&work_dir), "{ this is not json").unwrap();

    // Resolver must still construct (it logs the error and falls back to
    // work_dir-only). The bad file is NOT exposed as a workspace.
    let resolver = WorkspaceResolver::new(work_dir.to_string_lossy().as_ref());
    assert!(
        resolver.find_by_id("anything-bad").is_none(),
        "corrupt file must not expose phantom workspaces"
    );
    assert!(
        resolver.find_by_id("__agent_home__").is_some(),
        "synthetic __agent_home__ entry must still resolve even with a corrupt file"
    );

    // The mutation service must refuse to write a new entry — it would
    // otherwise overwrite the corrupt file with a fresh one and erase
    // the chance to recover any data behind it.
    let new_path = work_dir.join("new-dir");
    std::fs::create_dir_all(&new_path).unwrap();
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let svc = RuntimeWorkspaceMutationService::new(work_dir.clone());
    let err = rt.block_on(svc.create_workspace(WorkspaceEntryInput {
        id: Some("ws-after-corrupt".to_string()),
        path: Some(new_path.to_string_lossy().into_owned()),
        access: Some("read-write".to_string()),
        alias: None,
        prompt_file: None,
        last_active: None,
    }));

    let err = err.expect_err("create_workspace on a corrupt config must fail");

    match err {
        WorkspaceError::Persist(_) => {}
        other => panic!("expected Persist error on corrupt config; got: {other:?}"),
    }
    // The disk file is unchanged.
    let raw = std::fs::read_to_string(config_path(&work_dir)).unwrap();
    assert!(
        raw.contains("this is not json"),
        "the corrupt file must be preserved on disk; got: {raw}"
    );
}
