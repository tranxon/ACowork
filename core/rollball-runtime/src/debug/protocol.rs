//! JSON-RPC 2.0 protocol types for the Debug Protocol.
//!
//! Based on the debug protocol design in `docs/design/10-debug-protocol.md`.
//! All messages follow JSON-RPC 2.0 format over WebSocket.
//!
//! ## Message flow
//! - **Request**: Client → Server (`{ jsonrpc, id, method, params }`)
//! - **Response**: Server → Client (`{ jsonrpc, id, result/error }`)
//! - **Event (Notification)**: Server → Client (`{ jsonrpc, method, params }`, no `id`)

use serde::{Deserialize, Serialize};

// ── JSON-RPC 2.0 Core Types ──────────────────────────────────────────

/// A JSON-RPC 2.0 request from the debug client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: serde_json::Value,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

/// A JSON-RPC 2.0 response to the debug client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// A JSON-RPC 2.0 notification (server-pushed event, no `id`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

// ── Execution Control Methods ─────────────────────────────────────────

/// Parameters for `debugger.step`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepParams {
    /// Breakpoint granularity
    #[serde(default = "default_granularity")]
    pub granularity: StepGranularity,
}

fn default_granularity() -> StepGranularity {
    StepGranularity::Iteration
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepGranularity {
    Iteration,
    Phase,
}

// ── Breakpoint Types ──────────────────────────────────────────────────

/// A debug breakpoint definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Breakpoint {
    pub id: String,
    #[serde(default)]
    pub enabled: bool,
    pub condition: BreakpointCondition,
}

/// Breakpoint condition (one of four types).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum BreakpointCondition {
    #[serde(rename = "on_phase")]
    OnPhase { phase: String },
    #[serde(rename = "on_tool_call")]
    OnToolCall {
        #[serde(rename = "tool_name_pattern")]
        tool_name_pattern: String,
    },
    #[serde(rename = "on_iteration")]
    OnIteration { iteration: u32 },
    #[serde(rename = "on_tool_result")]
    OnToolResult {
        #[serde(rename = "is_error")]
        is_error: bool,
    },
}

/// Parameters for `debugger.setBreakpoint`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetBreakpointParams {
    pub condition: BreakpointCondition,
}

/// Result of `debugger.setBreakpoint`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetBreakpointResult {
    pub breakpoint_id: String,
}

/// Parameters for `debugger.removeBreakpoint`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoveBreakpointParams {
    pub breakpoint_id: String,
}

/// Result of `debugger.listBreakpoints`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListBreakpointsResult {
    pub breakpoints: Vec<Breakpoint>,
}

// ── State Query Types ─────────────────────────────────────────────────

/// Debug execution phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum DebugPhase {
    BudgetCheck,
    BuildContext,
    LlmCall,
    ParseResponse,
    ToolExecution,
    AppendHistory,
    Idle,
}

/// Result of `debugger.getState`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetStateResult {
    pub iteration: u32,
    pub phase: DebugPhase,
    pub messages: Vec<serde_json::Value>,
    pub snapshot_ids: Vec<String>,
    pub breakpoints: Vec<Breakpoint>,
    pub usage: DebugUsage,
    /// Whether the debug controller is currently paused
    pub paused: bool,
    /// Current execution state: "Running", "Paused", "Stepping", or "Stopped"
    pub state: String,
}

/// Token usage summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DebugUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

// ── Context Snapshot Types ────────────────────────────────────────────

/// Section metadata (same as in controller for serialization).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SectionMeta {
    pub size_bytes: usize,
    pub token_estimate: usize,
    pub hash: String,
}

/// Result of `debugger.getContextSnapshot`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetContextSnapshotResult {
    pub iteration: u32,
    pub built_at: String,
    pub sections: ContextSections,
    pub total_token_estimate: usize,
    pub phase: DebugPhase,
}

/// Seven control-plane sections of context (all content injected into the
/// LLM request that is NOT user input or LLM output).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextSections {
    pub system_prompt: SectionMeta,
    pub workspace_context: SectionMeta,
    pub environment: SectionMeta,
    pub tool_definitions: SectionMeta,
    pub skill_instructions: SectionMeta,
    pub retrieved_memory: SectionMeta,
    pub identity_context: SectionMeta,
}

/// Parameters for `debugger.getContextSnapshot` and `debugger.getSection`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetContextSnapshotParams {
    pub iteration: u32,
}

/// Parameters for `debugger.getSection`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetSectionParams {
    pub iteration: u32,
    /// One of: system_prompt, workspace_context, environment,
    /// tool_definitions, skill_instructions, retrieved_memory,
    /// identity_context
    pub section: String,
}

/// Result of `debugger.getSection`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetSectionResult {
    pub content: String,
    pub hash: String,
    pub token_count: usize,
}

// ── Context Editing Types ─────────────────────────────────────────────

/// Parameters for `debugger.rewind`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewindParams {
    pub to_iteration: u32,
}

/// Result of `debugger.rewind`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewindResult {
    pub rewound_to_iteration: u32,
    pub messages_trimmed_to: usize,
}

/// Parameters for `debugger.patchContext`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchContextParams {
    pub patches: PatchSet,
}

/// Set of patches to apply to context sections.
///
/// Multiple `patchContext` calls are merged incrementally: each call
/// overwrites only the sections it specifies, leaving previously patched
/// sections intact.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PatchSet {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_context: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_definitions: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill_instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retrieved_memory: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity_context: Option<serde_json::Value>,
}

impl PatchSet {
    /// Merge another PatchSet into this one, overwriting only the sections
    /// that the other PatchSet specifies. Sections that are None in the
    /// other set are left unchanged in this one.
    pub fn merge(&mut self, other: PatchSet) {
        if other.system_prompt.is_some() {
            self.system_prompt = other.system_prompt;
        }
        if other.workspace_context.is_some() {
            self.workspace_context = other.workspace_context;
        }
        if other.environment.is_some() {
            self.environment = other.environment;
        }
        if other.tool_definitions.is_some() {
            self.tool_definitions = other.tool_definitions;
        }
        if other.skill_instructions.is_some() {
            self.skill_instructions = other.skill_instructions;
        }
        if other.retrieved_memory.is_some() {
            self.retrieved_memory = other.retrieved_memory;
        }
        if other.identity_context.is_some() {
            self.identity_context = other.identity_context;
        }
    }
}

/// Parameters for `debugger.editMessage`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditMessageParams {
    pub index: usize,
    pub content: serde_json::Value,
}

/// Parameters for `debugger.rollback`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackParams {
    pub target_index: usize,
}

// ── Event Notification Types ──────────────────────────────────────────

/// Parameters for `debugger.onStep` event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnStepParams {
    pub iteration: u32,
    pub phase: DebugPhase,
    pub input: Option<serde_json::Value>,
    pub output: Option<serde_json::Value>,
    pub usage: Option<DebugUsage>,
}

/// Parameters for `debugger.onBreakpoint` event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnBreakpointParams {
    pub breakpoint_id: String,
    pub iteration: u32,
    pub phase: DebugPhase,
}

/// Parameters for `debugger.onStateChange` event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnStateChangeParams {
    pub old_phase: DebugPhase,
    pub new_phase: DebugPhase,
    pub iteration: u32,
}

/// Parameters for `debugger.onContextBuilt` event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnContextBuiltParams {
    pub iteration: u32,
    pub sections: ContextSections,
    pub total_token_estimate: usize,
}

// ── JSON-RPC Error Codes ──────────────────────────────────────────────

pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const INTERNAL_ERROR: i32 = -32603;
pub const SERVER_NOT_STARTED: i32 = -32000;
pub const NOT_IN_DEV_MODE: i32 = -32001;

impl JsonRpcResponse {
    /// Build a success response.
    pub fn success(id: serde_json::Value, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Build an error response.
    pub fn error(
        id: serde_json::Value,
        code: i32,
        message: impl Into<String>,
        data: Option<serde_json::Value>,
    ) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data,
            }),
        }
    }
}

impl JsonRpcNotification {
    pub fn new(method: impl Into<String>, params: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            method: method.into(),
            params,
        }
    }
}
