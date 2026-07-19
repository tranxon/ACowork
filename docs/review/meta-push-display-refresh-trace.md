# Review: Model / Reasoning-Effort display not refreshing on switch (streaming-only)

> **Symptom** (confirmed by user repro): switching model or reasoning effort in
> the UI **does not refresh the toolbar display** when the switch happens while
> the agent is **streaming** a response. The same switch works correctly when
> the agent is **idle**.
>
> The streaming-vs-idle asymmetry is the load-bearing clue. This report traces
> the chain end-to-end, names the exact race, and prescribes the fix.

---

## 1. Root cause (one bug, two layers contributing)

### 1.1  The race

**Layer A — runtime side (`core/acowork-runtime/src/agent/loop_session.rs:101-112`)**:

```rust
.try_send_chunk(super::loop_::ChunkEvent::SessionStateChanged {
    status: status.clone(),
    model: self.session.model().map(|s| s.to_string()),         // in-memory model
    provider: self.session.provider().map(|s| s.to_string()),   // in-memory provider
    workspace_id: workspace_id_str.clone(),
    ratio: self.session.model_ratio(),
    reasoning_effort: self.session.reasoning_effort().map(|e| e.to_string()),
    temperature: Some(effective_temperature),
    context_usage: context_usage_json.clone(),
})
```

`emit_session_state()` is invoked on **every status transition** —
`transition_status()` in the same file (`loop_session.rs:26-33`) calls it. So
every `streaming → waiting_approval`, `streaming → idle`, `streaming → paused`,
`waiting_approval → streaming`, … fires `SessionStateChanged` with the
**current in-memory `SessionState` snapshot**.

**Layer B — frontend side (`apps/acowork-desktop/src-tauri/src/commands/chat_mqtt.rs:817-821`)**:

```rust
m.insert("model".into(), serde_json::Value::String(p.model.clone()));
m.insert("provider".into(), serde_json::Value::String(p.provider.clone()));
m.insert("workspace_id".into(), serde_json::Value::String(p.workspace_id.clone()));
m.insert("ratio".into(), serde_json::json!(p.ratio));
m.insert("reasoning_effort".into(), serde_json::Value::String(p.reasoning_effort.clone()));
m.insert("temperature".into(), serde_json::json!(p.temperature));
```

These five fields are **always inserted** into the flat-JSON, even when the
proto-side `String` is empty (prost serializes `Option::None` as `""`).

**Layer C — chatStore (`apps/acowork-desktop/src/stores/chatStore.ts:2596-2614`)**:

```ts
const sessionPatch: Partial<SessionChatState> = { sessionStatus: status };

// ADR-012: Backend includes per-session model/provider (from JSONL metadata).
if (typeof data.model === "string") sessionPatch.model = data.model as string;
if (typeof data.provider === "string") sessionPatch.provider = data.provider as string;
if (typeof data.ratio === "number") sessionPatch.ratio = data.ratio as number;
if (typeof data.reasoning_effort === "string") sessionPatch.reasoningEffort = data.reasoning_effort as string;
if (typeof data.temperature === "number") sessionPatch.temperature = data.temperature as number;
```

The `typeof data.model === "string"` predicate **always passes** because
`chat_mqtt.rs:817` always inserts a string. The patch **always fires**, and
because `data.model === "A"` reflects the **lagging in-memory** state, it
clobbers the user's just-set optimistic value.

### 1.2  Step-by-step timeline (streaming scenario, user's actual repro)

| t | Actor | Action | State |
|---|---|---|---|
| T0 | LLM | Streaming response from model A | runtime: `session.model = "A"`; chatStore: `model = "A"` |
| T1 | User | Clicks ModelMenu → "switch to B" | chatStore optimistically: `model = "B"` |
| T1+ε | Tauri | `mqtt_publish_control` fires | — |
| T1+2ε | Runtime | `control_rx` → `route_model_switch` → `send_to_session(ModelSwitch)` | runtime: `session.model = "A"` (unchanged — message **queued** in session task's inbox because the LLM call is in flight) |
| T2 | LLM | Finishes current iteration → tool call | runtime: status transitions `streaming → waiting_approval` |
| T2+ε | Runtime | `transition_status` → **`emit_session_state`** | runtime reads `self.session.model() = "A"` and publishes `SessionStateChanged{model:"A"}` |
| T2+2ε | Desktop | Receives `session_state_changed` → chatStore handler | chatStore patches `sessionState.model = "A"` ❌ **reverts optimistic "B"** |
| T3 | Display | `useChatStore` selector sees `sessionState.model = "A"` | Toolbar re-renders, **shows "A"** — user sees the switch didn't take |
| T3+T | SessionTask | LLM call yields, mailbox drained, processes `ModelSwitch` | runtime: `session.model = "B"`, `conv.update_model_provider(B)` → publish `session_meta{model_id:"B"}` (retained) |
| T3+T+ε | Desktop | Receives `session_meta` → chatStore handler | chatStore patches `model = "B"` ✓ |
| T3+2T | Display | Toolbar finally shows "B" | — |

**Idle scenario**:

| t | Actor | Action |
|---|---|---|
| T0 | — | session is idle, runtime: `session.model = "A"`; chatStore: `model = "A"` |
| T1 | User | Optimistic `model = "B"` |
| T1+ε | Tauri | `mqtt_publish_control` |
| T1+2ε | Runtime | `route_model_switch` → `send_to_session(ModelSwitch)`; session task is **idle** in its `recv()` loop → processes ModelSwitch **immediately** → `self.session.model = "B"` → publish `session_meta{model_id:"B"}` (retained) |
| T2 | Desktop | Receives `session_meta` → patches `model = "B"` ✓ |

In idle, no status transition fires between the optimistic update and the
`session_meta` confirmation, so no clobbering happens.

---

## 2. Why earlier candidate fixes don't address the real bug

- **Empty-string filter on `case "session_meta":`** (`chatStore.ts:2828-2838`) is a real bug but is **not the bug the user is hitting right now**. The `session_meta` event lands later and the model/provider/reasoningEffort values are non-empty in the actual MQTT payload (verified at log lines 3414-3420 — three publishes, all non-empty values). The empty-string filter would only matter if the runtime ever publishes `""` for those fields; the proto contract documents `""` as valid for "no override", but the runtime currently never sets it that way.
- **HTTP `fetchSessionState`** (line 1740-1783) is not involved here — the user's bug is live-push-driven, not pull-driven.
- **MQTT retention race on session switch** — not involved; the user is not switching sessions, only the model on the active session.

---

## 3. The fix

Two layers, both small. Apply in order:

### Fix A — runtime (`loop_session.rs:101-112`)

**Stop carrying `model` / `provider` / `reasoning_effort` / `workspace_id` on
per-iteration `SessionStateChanged` emissions.** They are **persistent** state
(authoritative path: `session_meta` retained MQTT, plus `session_opened` on
activate) — not real-time state. `emit_session_state`'s real-time job is
`status` + `context_usage` + `ratio`. The persistent fields belong in events
that fire when persistent state actually changes.

Concrete change:

```rust
// Before:
if !self.session_core.try_send_chunk(super::loop_::ChunkEvent::SessionStateChanged {
    status: status.clone(),
    model: self.session.model().map(|s| s.to_string()),
    provider: self.session.provider().map(|s| s.to_string()),
    workspace_id: workspace_id_str.clone(),
    ratio: self.session.model_ratio(),
    reasoning_effort: self.session.reasoning_effort().map(|e| e.to_string()),
    temperature: Some(effective_temperature),
    context_usage: context_usage_json.clone(),
}) { … }

// After: split the two concerns.
// 1. Persistent fields → only the FIRST emit per session should include them.
//    Wire that via a one-shot flag set at session creation time.
let include_persistent = !self.persistent_meta_emitted;
let (model_opt, provider_opt, effort_opt, ws_opt) = if include_persistent {
    self.persistent_meta_emitted = true;
    (
        self.session.model().map(|s| s.to_string()),
        self.session.provider().map(|s| s.to_string()),
        self.session.reasoning_effort().map(|e| e.to_string()),
        workspace_id_str.clone(),
    )
} else {
    (None, None, None, None)
};

if !self.session_core.try_send_chunk(super::loop_::ChunkEvent::SessionStateChanged {
    status: status.clone(),
    model: model_opt,
    provider: provider_opt,
    workspace_id: ws_opt,
    ratio: self.session.model_ratio(),
    reasoning_effort: effort_opt,
    temperature: Some(effective_temperature),
    context_usage: context_usage_json.clone(),
}) { … }
```

Add `persistent_meta_emitted: bool` to the `AgentLoop` struct (or whichever
owns `emit_session_state`) and initialize to `false` in the constructor /
`build_initial_session_state`. Set to `true` after the first emit.

This is **the proper architectural fix** — it separates persistent state from
runtime state at the source.

### Fix B — frontend defensive (`chatStore.ts:2596-2614`)

Even with Fix A, the `SessionStateChanged` event still carries
`ratio` / `temperature` correctly, and we want to keep the chatStore code
intuitive. But the `model` / `provider` / `reasoning_effort` / `workspace_id`
patching path is now wrong-by-construction. Apply the **key-presence** rule
from the prior report:

```ts
// session_state_changed is a STATUS event, not a meta event.
// Persistent fields (model, provider, reasoningEffort, workspaceId)
// are owned by `session_meta` (retained MQTT) and the optimistic
// update path. Don't clobber them here — apply ONLY status, ratio,
// temperature, and context_usage.
const hasOwn = (k: string) => Object.prototype.hasOwnProperty.call(data, k);
const sessionPatch: Partial<SessionChatState> = { sessionStatus: status };

// (keep)
if (typeof data.ratio === "number") sessionPatch.ratio = data.ratio;
if (typeof data.temperature === "number") sessionPatch.temperature = data.temperature;
if (data.context_usage && typeof data.context_usage === "object") {
    sessionPatch.contextUsage = data.context_usage as ContextUsageInfo;
}
// (removed) model / provider / reasoningEffort / workspace_id
```

This is a **defense-in-depth** measure: even if Fix A regresses in the
future, the chatStore won't clobber an optimistic update.

### Fix C — workspace_id (separate concern, but same shape)

`session_state_changed` currently carries `workspace_id` too. This one is
also redundant — `workspace_id` is persisted on the same code path as
`model` and `provider`, so it goes through `session_meta` retained. Apply
the same treatment: drop from `session_state_changed`, only include in the
first emit (Fix A) and on the post-`UpdateWorkspaceContext` emit at
`session_task.rs:1391`.

The chatStore handler at line 2842-2845 already handles `workspace_id`
correctly from `session_meta` — no frontend change needed there.

### Fix D — regression test

Add a regression test in `chatStore.test.ts` (or equivalent) that simulates:

```ts
it("does NOT clobber optimistic model when session_state_changed arrives mid-stream", () => {
  // 1. user optimistically sets model = "B"
  useChatStore.getState().setCurrentModel("B", "provider-b", "agent1");
  // 2. simulate a session_state_changed arriving from runtime with model = "A"
  //    (because the LLM is still on the previous iteration)
  handler({ type: "session_state_changed", session_id: "sess1",
            status: { status: "waiting_approval" }, model: "A", provider: "provider-a" },
          setState, getState, "agent1");
  // 3. expect: chatStore still shows model = "B" (optimistic preserved)
  expect(useChatStore.getState().agentStates.agent1.sessionStates.sess1.model).toBe("B");
});
```

---

## 4. What's NOT the fix

- **Don't** try to delay `session_state_changed` on the runtime side (e.g.
  "wait for queued ModelSwitch before emitting state"). It would entangle
  status emission with control message processing — a layering violation.
- **Don't** try to detect "user has an optimistic update in flight" on the
  runtime side — the runtime shouldn't know about the client's optimistic
  state. The boundary should be: "runtime publishes what is currently true
  in persistent state; runtime publishes what is currently true in
  runtime state; never confuse the two."

---

## 5. Side benefits of Fix A

- **Lower MQTT traffic**: every iteration currently re-sends
  `model/provider/reasoningEffort` even when nothing changed. After Fix A,
  only the actual `session_meta` retained publish carries those fields —
  the broker dedupes by topic.
- **Cleaner wire format**: `session_state_changed` payload becomes purely
  about **runtime state** (`status`, `context_usage`, `ratio`,
  `temperature`). `session_meta` becomes purely about **persistent state**.
  No more overlap, no more "which event is the source of truth" confusion.
- **Closes the same race for `reasoningEffort`**: the same bug exists for
  the reasoning-effort selector. Fix A closes both with one change.

---

## 6. Verification plan

1. Apply Fix A + Fix B + Fix D.
2. `cargo check -p acowork-runtime` — confirm no regressions in the
   `loop_session` / `subsystems` / `session_task` callers (8 emit sites
   total, all benign — they just won't carry persistent fields anymore).
3. `cargo test -p acowork-runtime` — confirm existing tests pass.
4. Manual test: trigger model switch during streaming, observe toolbar
   stays at "B" for the duration of the LLM call, then briefly stays at
   "B" after the call finishes (until `session_meta` confirms). No flicker.
5. Manual test: idle-time model switch (regression check) — should still
   work and now travel purely through `session_meta`.
6. Manual test: reasoning-effort switch during streaming — same fix path.