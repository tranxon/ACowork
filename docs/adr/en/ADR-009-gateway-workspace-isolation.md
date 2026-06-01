# ADR-009: Gateway Workspace Isolation

## Status
Accepted

## Context

The Gateway has historically accessed agent workspace directories directly — reading and writing files under `{install_path}/workspace/` and `{install_path}/manifest.toml`. This violates the principle that the Gateway should only manage its own `{data_dir}`, while the Runtime owns its workspace.

Five violations were identified:

| ID | File | Operation | Path |
|----|------|-----------|------|
| V1 | `workspaces.rs` | READ + WRITE | `{install_path}/workspace/config/agent_workspaces.json` |
| V2 | `agents.rs` | READ + WRITE | `{install_path}/manifest.toml` |
| V3 | `agents.rs` | READ | `{install_path}/prompts/*.md` |
| V4 | `lifecycle/manager.rs` | WRITE | `{install_path}/workspace/.identity_delivery.json` |
| V5 | `config_api.rs` | DELETE | `{install_path}/workspace/logs/*.log` |

### Key observation: stopped agents have no UI

The desktop app only shows the **Status** tab for stopped agents. Setup, Memory, Chat, and Workspace UI are all hidden when the agent is not running. This means:

- The Gateway never needs to read workspace/config/manifest data for stopped agents (the UI doesn't consume it)
- The Gateway never needs to write workspace data for stopped agents (no user action can trigger it)
- Stopped agents have no Runtime process, so there is no IPC target

### Existing IPC infrastructure

Several IPC message types already exist that can carry the relevant data:

- `WorkspaceContextUpdate` — already used by 3 of 5 workspace handlers
- `RuntimeConfigUpdate.active_tools` — already pushed by `update_agent_config`
- `IdentityDelivery` — defined in protocol but never used
- `LogRotate` — already used for running agent log cleanup

## Decision

**Gateway never reads or writes agent workspace files.** The rule is absolute:

- **Running agents**: All reads and writes go through IPC. The Gateway sends data to the Runtime, which persists to its own workspace.
- **Stopped agents**: API endpoints return "not operable" or empty data. No fallback file access.

This eliminates the need for dual-path code (IPC + file fallback) entirely.

### Migration plan

| Violation | Strategy | Detail |
|-----------|----------|--------|
| V1 | IPC push + Runtime persist | Gateway sends `WorkspaceContextUpdate` with full config; Runtime writes `agent_workspaces.json` itself. Stopped → return empty list. Fix bug: `update_workspace` missing IPC push. |
| V2 | Remove `write_manifest_tools` | `active_tools` persistence already exists in per-agent config (`{data_dir}/agent_configs/{id}.json`). Delete `write_manifest_tools()`. `read_manifest_tools()` remains as install-time discovery fallback only (no write-back). Stopped → return empty tools. |
| V3 | Remove `read_system_prompt` | System prompt is Runtime-internal. Gateway has no business reading it. Stopped → return null. Running → system prompt comes from per-agent config override (`system_prompt_override`). |
| V4 | `AgentHelloResult` delivery | Delete `std::fs::write(.identity_delivery.json)` in `start_agent()`. Add `identity_entries: Vec<IdentityEntry>` to `AgentHelloResult`. Runtime receives identity after IPC handshake and injects into system prompt. |
| V5 | Runtime self-cleanup | Delete Phase 3 (stopped agent log deletion). Runtime cleans its own old logs on startup. Alternatively: accept this as a package-manager-like exception (pre-start cleanup). |

### Exceptions

The following Gateway operations on `install_path` are **explicitly allowed** because they manage the agent installation itself, not the runtime workspace:

- Package manager: install, uninstall, upgrade, clone, publish
- Agent listing: reading `agent.yaml` / `manifest.toml` for metadata (name, version, description) during `list_agents`

These are install-time operations, not runtime data access.

## Consequences

### What becomes easier
- Gateway code is simpler — no workspace path construction, no dual-path logic
- No risk of Gateway/Runtime file races
- Clean ownership model: Gateway owns `{data_dir}`, Runtime owns `{workspace}`
- Stopped agent API handlers become trivial (return empty/not-operable)

### What becomes harder
- V4 requires changing the Runtime initialization sequence (system prompt must be built after IPC handshake, not before)
- Workspace API behavior changes: stopped agents return empty workspace lists (frontend already handles this gracefully — no workspace UI is shown for stopped agents)
- Any future need to show stopped agent config would require persisting it in `{data_dir}` instead

### Compatibility
- No breaking changes for running agents (IPC path already exists for most operations)
- Stopped agent APIs change behavior: return empty/null instead of reading from workspace
- Frontend is already compatible — stopped agents don't show config/workspace/setup UI
