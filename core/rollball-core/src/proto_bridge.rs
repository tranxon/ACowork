//! Bridge conversions between domain types and generated proto types.
//!
//! Implements `From` traits so existing business logic (protocol.rs, budget.rs,
//! identity.rs) can seamlessly convert to/from the tonic-generated proto types.
//! This keeps the old JSON-based protocol intact while adding gRPC support.

use crate::budget;
use crate::identity;
use crate::proto;
use crate::protocol;

// ── UsageReport ↔ UsageReportRequest ─────────────────────────────────────

impl From<&budget::UsageReport> for proto::UsageReportRequest {
    fn from(r: &budget::UsageReport) -> Self {
        Self {
            agent_id: r.agent_id.clone(),
            provider: r.provider.clone(),
            tokens_used: r.tokens_used,
            cost_usd: r.cost_usd,
            timestamp: r.timestamp.to_rfc3339(),
            error: r.error.clone().unwrap_or_default(),
        }
    }
}

impl From<proto::UsageReportRequest> for budget::UsageReport {
    fn from(r: proto::UsageReportRequest) -> Self {
        Self {
            agent_id: r.agent_id,
            provider: r.provider,
            tokens_used: r.tokens_used,
            cost_usd: r.cost_usd,
            timestamp: chrono::DateTime::parse_from_rfc3339(&r.timestamp)
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .unwrap_or_default(),
            error: if r.error.is_empty() {
                None
            } else {
                Some(r.error)
            },
        }
    }
}

// ── ContextUsageInfo ↔ ContextUsageInfo ──────────────────────────────────

impl From<&protocol::ContextUsageInfo> for proto::ContextUsageInfo {
    fn from(c: &protocol::ContextUsageInfo) -> Self {
        Self {
            context_window: c.context_window,
            input_tokens: c.input_tokens,
            output_tokens: c.output_tokens,
            total_tokens: c.total_tokens,
            max_input_tokens: c.max_input_tokens.unwrap_or(0),
            usable_context: c.usable_context,
            usage_percent: c.usage_percent as u32,
        }
    }
}

impl From<proto::ContextUsageInfo> for protocol::ContextUsageInfo {
    fn from(c: proto::ContextUsageInfo) -> Self {
        Self {
            context_window: c.context_window,
            input_tokens: c.input_tokens,
            output_tokens: c.output_tokens,
            total_tokens: c.total_tokens,
            max_input_tokens: if c.max_input_tokens == 0 {
                None
            } else {
                Some(c.max_input_tokens)
            },
            usable_context: c.usable_context,
            usage_percent: c.usage_percent as u8,
        }
    }
}

// ── ModelCostInfo ↔ ModelCostInfo ────────────────────────────────────────

impl From<&protocol::ModelCostInfo> for proto::ModelCostInfo {
    fn from(c: &protocol::ModelCostInfo) -> Self {
        Self {
            input_per_million: c.input_per_million.unwrap_or(0.0),
            output_per_million: c.output_per_million.unwrap_or(0.0),
        }
    }
}

impl From<proto::ModelCostInfo> for protocol::ModelCostInfo {
    fn from(c: proto::ModelCostInfo) -> Self {
        Self {
            input_per_million: if c.input_per_million == 0.0 {
                None
            } else {
                Some(c.input_per_million)
            },
            output_per_million: if c.output_per_million == 0.0 {
                None
            } else {
                Some(c.output_per_million)
            },
        }
    }
}

// ── ModelModalities ↔ ModelModalities ────────────────────────────────────

impl From<&protocol::ModelModalities> for proto::ModelModalities {
    fn from(m: &protocol::ModelModalities) -> Self {
        Self {
            input: m.input.clone(),
            output: m.output.clone(),
        }
    }
}

impl From<proto::ModelModalities> for protocol::ModelModalities {
    fn from(m: proto::ModelModalities) -> Self {
        Self {
            input: m.input,
            output: m.output,
        }
    }
}

// ── ModelCapabilitiesInfo ↔ ModelCapabilitiesInfo ────────────────────────

impl From<&protocol::ModelCapabilitiesInfo> for proto::ModelCapabilitiesInfo {
    fn from(m: &protocol::ModelCapabilitiesInfo) -> Self {
        Self {
            context_window: m.context_window,
            max_output_tokens: m.max_output_tokens,
            max_input_tokens: m.max_input_tokens.unwrap_or(0),
            supports_tool_calling: m.supports_tool_calling,
            supports_reasoning: m.supports_reasoning.unwrap_or(false),
            supports_attachment: m.supports_attachment.unwrap_or(false),
            supports_temperature: m.supports_temperature.unwrap_or(true),
            cost: m.cost.as_ref().map(|c| c.into()),
            modalities: m.modalities.as_ref().map(|m| m.into()),
            name: m.name.clone().unwrap_or_default(),
            family: m.family.clone().unwrap_or_default(),
            knowledge_cutoff: m.knowledge_cutoff.clone().unwrap_or_default(),
        }
    }
}

impl From<proto::ModelCapabilitiesInfo> for protocol::ModelCapabilitiesInfo {
    fn from(m: proto::ModelCapabilitiesInfo) -> Self {
        Self {
            context_window: m.context_window,
            max_output_tokens: m.max_output_tokens,
            max_input_tokens: if m.max_input_tokens == 0 {
                None
            } else {
                Some(m.max_input_tokens)
            },
            supports_tool_calling: m.supports_tool_calling,
            supports_reasoning: if !m.supports_reasoning {
                None
            } else {
                Some(true)
            },
            supports_attachment: if !m.supports_attachment {
                None
            } else {
                Some(true)
            },
            supports_temperature: if m.supports_temperature {
                None // default true, so None preserves default behavior
            } else {
                Some(false)
            },
            cost: m.cost.map(|c| c.into()),
            modalities: m.modalities.map(|m| m.into()),
            name: if m.name.is_empty() { None } else { Some(m.name) },
            family: if m.family.is_empty() { None } else { Some(m.family) },
            knowledge_cutoff: if m.knowledge_cutoff.is_empty() { None } else { Some(m.knowledge_cutoff) },
        }
    }
}

// ── SessionInfoDto ↔ SessionInfoDto ──────────────────────────────────────

impl From<&protocol::SessionInfoDto> for proto::SessionInfoDto {
    fn from(s: &protocol::SessionInfoDto) -> Self {
        Self {
            session_id: s.session_id.clone(),
            created_at: s.created_at.clone(),
            message_count: s.message_count,
            title: s.title.clone().unwrap_or_default(),
            corrupted: s.corrupted,
        }
    }
}

impl From<proto::SessionInfoDto> for protocol::SessionInfoDto {
    fn from(s: proto::SessionInfoDto) -> Self {
        Self {
            session_id: s.session_id,
            created_at: s.created_at,
            message_count: s.message_count,
            title: if s.title.is_empty() { None } else { Some(s.title) },
            corrupted: s.corrupted,
        }
    }
}

// ── ConversationEntryDto ↔ ConversationEntryDto ──────────────────────────

impl From<&protocol::ConversationEntryDto> for proto::ConversationEntryDto {
    fn from(e: &protocol::ConversationEntryDto) -> Self {
        Self {
            id: e.id.clone(),
            ts: e.ts.clone(),
            role: e.role.clone(),
            content: e.content.clone(),
            metadata_json: e
                .metadata
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_default(),
        }
    }
}

impl From<proto::ConversationEntryDto> for protocol::ConversationEntryDto {
    fn from(e: proto::ConversationEntryDto) -> Self {
        Self {
            id: e.id,
            ts: e.ts,
            role: e.role,
            content: e.content,
            metadata: if e.metadata_json.is_empty() {
                None
            } else {
                serde_json::from_str(&e.metadata_json).ok()
            },
        }
    }
}

// ── CronEntryInfo ↔ CronEntryInfo ────────────────────────────────────────

impl From<&protocol::CronEntryInfo> for proto::CronEntryInfo {
    fn from(c: &protocol::CronEntryInfo) -> Self {
        Self {
            id: c.id.clone(),
            agent_id: c.agent_id.clone(),
            schedule: c.schedule.clone(),
            action: c.action.clone(),
            params_json: c.params.to_string(),
        }
    }
}

impl From<proto::CronEntryInfo> for protocol::CronEntryInfo {
    fn from(c: proto::CronEntryInfo) -> Self {
        Self {
            id: c.id,
            agent_id: c.agent_id,
            schedule: c.schedule,
            action: c.action,
            params: serde_json::from_str(&c.params_json).unwrap_or(serde_json::Value::Null),
            timezone: None,
            retry_count: 0,
            retry_interval_secs: 60,
            max_runs: None,
            run_count: 0,
            expires_at: None,
        }
    }
}

// ── IdentityEntry ↔ IdentityEntry ────────────────────────────────────────

impl From<&identity::IdentityEntry> for proto::IdentityEntry {
    fn from(e: &identity::IdentityEntry) -> Self {
        Self {
            field: e.field.clone(),
            value: e.value.clone(),
            confidence: e.confidence,
            category: e.category.as_str().to_string(),
            privacy: e.privacy.as_str().to_string(),
            source: e.source.clone(),
            updated_at: e.updated_at.clone(),
        }
    }
}

impl From<proto::IdentityEntry> for identity::IdentityEntry {
    fn from(e: proto::IdentityEntry) -> Self {
        Self {
            field: e.field,
            value: e.value,
            confidence: e.confidence,
            category: e.category.parse().unwrap_or(identity::IdentityCategory::Identity),
            privacy: e.privacy.parse().unwrap_or(identity::PrivacyLevel::Personal),
            source: e.source,
            updated_at: e.updated_at,
        }
    }
}

// ── IdentityQueryResult ↔ IdentityQueryResult ────────────────────────────

impl From<&identity::IdentityQueryResult> for proto::IdentityQueryResult {
    fn from(r: &identity::IdentityQueryResult) -> Self {
        Self {
            values: r.values.clone(),
            confidence: r.confidence.clone(),
        }
    }
}

impl From<proto::IdentityQueryResult> for identity::IdentityQueryResult {
    fn from(r: proto::IdentityQueryResult) -> Self {
        Self {
            values: r.values,
            confidence: r.confidence,
        }
    }
}

// ── BudgetInfo bridge (extracted from GatewayResponse::BudgetInfo) ───────

/// Intermediate struct for BudgetInfo conversion.
/// GatewayResponse::BudgetInfo is an enum variant, so we provide a helper
/// for the inner data.
pub struct BudgetInfoData {
    pub remaining_tokens: u64,
    pub remaining_cost_usd: f64,
}

impl From<BudgetInfoData> for proto::BudgetInfo {
    fn from(b: BudgetInfoData) -> Self {
        Self {
            remaining_tokens: b.remaining_tokens,
            remaining_cost_usd: b.remaining_cost_usd,
        }
    }
}

impl From<proto::BudgetInfo> for BudgetInfoData {
    fn from(b: proto::BudgetInfo) -> Self {
        Self {
            remaining_tokens: b.remaining_tokens,
            remaining_cost_usd: b.remaining_cost_usd,
        }
    }
}

// ── GatewayRequest → ClientMessage helpers ──────────────────────────────

/// Convert a domain GatewayRequest into a proto ClientMessage.
///
/// `request_id` must be assigned by the caller (correlation ID).
impl GatewayRequestToProto for protocol::GatewayRequest {
    fn to_proto(&self, request_id: u64) -> proto::ClientMessage {
        let payload = match self {
            protocol::GatewayRequest::KeyRelease { provider } => {
                Some(proto::client_message::Payload::KeyRelease(
                    proto::KeyReleaseRequest { provider: provider.clone() },
                ))
            }
            protocol::GatewayRequest::IntentSend { target, action, params, async_ } => {
                Some(proto::client_message::Payload::IntentSend(
                    proto::IntentSendRequest {
                        target: target.clone(),
                        action: action.clone(),
                        params_json: params.to_string(),
                        r#async: *async_,
                    },
                ))
            }
            protocol::GatewayRequest::BudgetQuery { provider } => {
                Some(proto::client_message::Payload::BudgetQuery(
                    proto::BudgetQueryRequest { provider: provider.clone() },
                ))
            }
            protocol::GatewayRequest::UsageReport(report) => {
                Some(proto::client_message::Payload::UsageReport(report.into()))
            }
            protocol::GatewayRequest::RateAcquire { provider } => {
                Some(proto::client_message::Payload::RateAcquire(
                    proto::RateAcquireRequest { provider: provider.clone() },
                ))
            }
            protocol::GatewayRequest::IdentityQuery { fields } => {
                Some(proto::client_message::Payload::IdentityQuery(
                    proto::IdentityQueryRequest { fields: fields.clone() },
                ))
            }
            protocol::GatewayRequest::CapabilityQuery { agent_id } => {
                Some(proto::client_message::Payload::CapabilityQuery(
                    proto::CapabilityQueryRequest {
                        agent_id: agent_id.clone().unwrap_or_default(),
                    },
                ))
            }
            protocol::GatewayRequest::CronRegister {
                agent_id,
                schedule,
                action,
                params,
                ..
            } => {
                Some(proto::client_message::Payload::CronRegister(
                    proto::CronRegisterRequest {
                        agent_id: agent_id.clone(),
                        schedule: schedule.clone(),
                        action: action.clone(),
                        params_json: params.to_string(),
                    },
                ))
            }
            protocol::GatewayRequest::CronUnregister { cron_id } => {
                Some(proto::client_message::Payload::CronUnregister(
                    proto::CronUnregisterRequest { cron_id: cron_id.clone() },
                ))
            }
            protocol::GatewayRequest::CronList {} => {
                Some(proto::client_message::Payload::CronList(
                    proto::CronListRequest {},
                ))
            }
            protocol::GatewayRequest::ContextUsageReport { agent_id, context } => {
                Some(proto::client_message::Payload::ContextUsageReport(
                    proto::ContextUsageReportRequest {
                        agent_id: agent_id.clone(),
                        context: Some(context.into()),
                    },
                ))
            }
            protocol::GatewayRequest::AgentHello {
                agent_id,
                version,
                connection_role,
            } => {
                Some(proto::client_message::Payload::AgentHello(
                    proto::AgentHelloRequest {
                        agent_id: agent_id.clone(),
                        version: version.clone(),
                        connection_role: connection_role.clone(),
                    },
                ))
            }
            protocol::GatewayRequest::ListSessions => {
                Some(proto::client_message::Payload::ListSessions(
                    proto::ListSessionsRequest {},
                ))
            }
            protocol::GatewayRequest::GetSessionMessages {
                session_id,
                cursor,
                limit,
                direction,
            } => {
                Some(proto::client_message::Payload::GetSessionMessages(
                    proto::GetSessionMessagesRequest {
                        session_id: session_id.clone(),
                        cursor: cursor.clone().unwrap_or_default(),
                        limit: *limit,
                        direction: direction.clone(),
                    },
                ))
            }
            protocol::GatewayRequest::CreateSession => {
                Some(proto::client_message::Payload::CreateSession(
                    proto::CreateSessionRequest {},
                ))
            }
            protocol::GatewayRequest::GetCurrentSessionId => {
                Some(proto::client_message::Payload::GetCurrentSessionId(
                    proto::GetCurrentSessionIdRequest {},
                ))
            }
            protocol::GatewayRequest::DeleteSession { session_id } => {
                Some(proto::client_message::Payload::DeleteSession(
                    proto::DeleteSessionRequest { session_id: session_id.clone() },
                ))
            }
        };

        proto::ClientMessage {
            request_id,
            payload,
        }
    }
}

/// Trait for converting GatewayRequest to proto ClientMessage.
pub trait GatewayRequestToProto {
    fn to_proto(&self, request_id: u64) -> proto::ClientMessage;
}

// ── GatewayResponse → ServerMessage helpers ─────────────────────────────

/// Convert a domain GatewayResponse into a proto ServerMessage.
///
/// `request_id` must be assigned by the caller (0 for unsolicited pushes).
impl GatewayResponseToProto for protocol::GatewayResponse {
    fn to_proto(&self, request_id: u64) -> proto::ServerMessage {
        let payload = match self {
            protocol::GatewayResponse::AgentHelloResult {
                success,
                error,
                provider,
                model,
                api_key,
                base_url,
                models,
                model_capabilities,
                max_output_tokens_limit,
                protocol_type,
                workspace_context_text,
                current_workspace_id,
                current_workspace_path,
                runtime_max_output_tokens,
                runtime_max_iterations,
                runtime_temperature,
                runtime_system_prompt_override,
                runtime_shell_approval_threshold,
            } => {
                Some(proto::server_message::Payload::AgentHelloResult(
                    proto::AgentHelloResult {
                        success: *success,
                        error: error.clone().unwrap_or_default(),
                        provider: provider.clone().unwrap_or_default(),
                        model: model.clone().unwrap_or_default(),
                        api_key: api_key.clone().unwrap_or_default(),
                        base_url: base_url.clone().unwrap_or_default(),
                        models: models.clone(),
                        model_capabilities: model_capabilities
                            .as_ref()
                            .map(|c| c.into()),
                        max_output_tokens_limit: *max_output_tokens_limit,
                        protocol_type: format!("{:?}", protocol_type).to_lowercase(),
                        workspace_context_text: workspace_context_text
                            .clone()
                            .unwrap_or_default(),
                        current_workspace_id: current_workspace_id
                            .clone()
                            .unwrap_or_default(),
                        current_workspace_path: current_workspace_path
                            .clone()
                            .unwrap_or_default(),
                        runtime_max_output_tokens: runtime_max_output_tokens.clone(),
                        runtime_max_iterations: runtime_max_iterations.clone(),
                        runtime_temperature: runtime_temperature.clone(),
                        runtime_system_prompt_override: runtime_system_prompt_override
                            .clone()
                            .unwrap_or_default(),
                        runtime_shell_approval_threshold: runtime_shell_approval_threshold
                            .clone()
                            .unwrap_or_default(),
                    },
                ))
            }
            protocol::GatewayResponse::KeyReleaseResult { api_key, error } => {
                Some(proto::server_message::Payload::KeyReleaseResult(
                    proto::KeyReleaseResult {
                        api_key: api_key.clone().unwrap_or_default(),
                        error: error.clone().unwrap_or_default(),
                    },
                ))
            }
            protocol::GatewayResponse::IntentDelivered { message_id } => {
                Some(proto::server_message::Payload::IntentDelivered(
                    proto::IntentDelivered { message_id: message_id.clone() },
                ))
            }
            protocol::GatewayResponse::IntentReceived { from, action, params, command } => {
                Some(proto::server_message::Payload::IntentReceived(
                    proto::IntentReceived {
                        from: from.clone(),
                        action: action.clone(),
                        params_json: params.to_string(),
                        command: command.clone().unwrap_or_default(),
                    },
                ))
            }
            protocol::GatewayResponse::BudgetInfo {
                remaining_tokens,
                remaining_cost_usd,
            } => {
                Some(proto::server_message::Payload::BudgetInfo(
                    proto::BudgetInfo {
                        remaining_tokens: *remaining_tokens,
                        remaining_cost_usd: *remaining_cost_usd,
                    },
                ))
            }
            protocol::GatewayResponse::UsageReportAck {} => {
                Some(proto::server_message::Payload::UsageReportAck(
                    proto::UsageReportAck {},
                ))
            }
            protocol::GatewayResponse::ContextUsageAck {} => {
                Some(proto::server_message::Payload::ContextUsageAck(
                    proto::ContextUsageAck {},
                ))
            }
            protocol::GatewayResponse::RateToken { granted, retry_after_ms } => {
                Some(proto::server_message::Payload::RateToken(
                    proto::RateToken {
                        granted: *granted,
                        retry_after_ms: retry_after_ms.unwrap_or(0),
                    },
                ))
            }
            protocol::GatewayResponse::IdentityDelivery { entries } => {
                Some(proto::server_message::Payload::IdentityDelivery(
                    proto::IdentityDelivery {
                        entries: entries.iter().map(|e| e.into()).collect(),
                    },
                ))
            }
            protocol::GatewayResponse::LLMConfigDelivery {
                provider,
                model,
                api_key,
                base_url,
                models,
                model_capabilities,
                max_output_tokens_limit,
                protocol_type,
            } => {
                Some(proto::server_message::Payload::LlmConfigDelivery(
                    proto::LlmConfigDelivery {
                        provider: provider.clone(),
                        model: model.clone().unwrap_or_default(),
                        api_key: api_key.clone(),
                        base_url: base_url.clone().unwrap_or_default(),
                        models: models.clone(),
                        model_capabilities: model_capabilities.as_ref().map(|m| m.into()),
                        max_output_tokens_limit: *max_output_tokens_limit,
                        protocol_type: format!("{:?}", protocol_type).to_lowercase(),
                    },
                ))
            }
            protocol::GatewayResponse::IdentityQueryResult { values, confidence } => {
                Some(proto::server_message::Payload::IdentityQueryResult(
                    proto::IdentityQueryResult {
                        values: values.clone(),
                        confidence: confidence.clone(),
                    },
                ))
            }
            protocol::GatewayResponse::CapabilityOverview { capabilities } => {
                Some(proto::server_message::Payload::CapabilityOverview(
                    proto::CapabilityOverview {
                        capabilities: capabilities
                            .iter()
                            .map(|(k, v)| (k.clone(), proto::StringList { items: v.clone() }))
                            .collect(),
                    },
                ))
            }
            protocol::GatewayResponse::CapabilityUpdate {
                agent_id,
                actions,
                removed,
            } => {
                Some(proto::server_message::Payload::CapabilityUpdate(
                    proto::CapabilityUpdate {
                        agent_id: agent_id.clone(),
                        actions: actions.clone(),
                        removed: *removed,
                    },
                ))
            }
            protocol::GatewayResponse::CronRegisterResult { cron_id, error } => {
                Some(proto::server_message::Payload::CronRegisterResult(
                    proto::CronRegisterResult {
                        cron_id: cron_id.clone().unwrap_or_default(),
                        error: error.clone().unwrap_or_default(),
                    },
                ))
            }
            protocol::GatewayResponse::CronUnregisterResult { removed } => {
                Some(proto::server_message::Payload::CronUnregisterResult(
                    proto::CronUnregisterResult { removed: *removed },
                ))
            }
            protocol::GatewayResponse::CronListResult { entries } => {
                Some(proto::server_message::Payload::CronListResult(
                    proto::CronListResult {
                        entries: entries.iter().map(|e| e.into()).collect(),
                    },
                ))
            }
            protocol::GatewayResponse::WorkspaceContextUpdate {
                context_text,
                current_workspace_id,
                current_workspace_path,
            } => {
                Some(proto::server_message::Payload::WorkspaceContextUpdate(
                    proto::WorkspaceContextUpdate {
                        context_text: context_text.clone(),
                        current_workspace_id: current_workspace_id.clone().unwrap_or_default(),
                        current_workspace_path: current_workspace_path.clone().unwrap_or_default(),
                    },
                ))
            }
            protocol::GatewayResponse::IterationLimitPaused {
                iteration,
                max_iterations,
                message,
            } => {
                Some(proto::server_message::Payload::IterationLimitPaused(
                    proto::IterationLimitPaused {
                        iteration: *iteration,
                        max_iterations: *max_iterations,
                        message: message.clone(),
                    },
                ))
            }
            protocol::GatewayResponse::SessionList { sessions } => {
                Some(proto::server_message::Payload::SessionList(
                    proto::SessionList {
                        sessions: sessions.iter().map(|s| s.into()).collect(),
                    },
                ))
            }
            protocol::GatewayResponse::SessionMessages {
                messages,
                cursor,
                has_more,
            } => {
                Some(proto::server_message::Payload::SessionMessages(
                    proto::SessionMessages {
                        messages: messages.iter().map(|m| m.into()).collect(),
                        cursor: cursor.clone().unwrap_or_default(),
                        has_more: *has_more,
                    },
                ))
            }
            protocol::GatewayResponse::SessionCreated { session_id } => {
                Some(proto::server_message::Payload::SessionCreated(
                    proto::SessionCreated { session_id: session_id.clone() },
                ))
            }
            protocol::GatewayResponse::CurrentSessionId { session_id } => {
                Some(proto::server_message::Payload::CurrentSessionId(
                    proto::CurrentSessionId { session_id: session_id.clone().unwrap_or_default() },
                ))
            }
            protocol::GatewayResponse::SessionDeleted { success, error } => {
                Some(proto::server_message::Payload::SessionDeleted(
                    proto::SessionDeleted { success: *success, error: error.clone().unwrap_or_default() },
                ))
            }
            protocol::GatewayResponse::LogLevelUpdate { log_level } => {
                Some(proto::server_message::Payload::LogLevelUpdate(
                    proto::LogLevelUpdate { log_level: log_level.clone() },
                ))
            }
            protocol::GatewayResponse::LogRotate => {
                Some(proto::server_message::Payload::LogRotate(
                    proto::LogRotate {},
                ))
            }
            protocol::GatewayResponse::RuntimeConfigUpdate {
                max_output_tokens,
                max_iterations,
                temperature,
                system_prompt_override,
                active_tools,
                shell_approval_threshold,
            } => {
                Some(proto::server_message::Payload::RuntimeConfigUpdate(
                    proto::RuntimeConfigUpdate {
                        max_output_tokens: max_output_tokens.clone(),
                        max_iterations: max_iterations.clone(),
                        temperature: temperature.clone(),
                        system_prompt_override: system_prompt_override.clone().unwrap_or_default(),
                        active_tools: active_tools.clone().unwrap_or_default(),
                        shell_approval_threshold: shell_approval_threshold.clone().unwrap_or_default(),
                    },
                ))
            }
            // Unknown messages have no proto representation — they are
            // generated on the Runtime side when incoming proto messages
            // are malformed or unrecognized. Mapping them to UsageReportAck
            // ensures the server can still construct a valid ServerMessage.
            protocol::GatewayResponse::Unknown {} => {
                Some(proto::server_message::Payload::UsageReportAck(
                    proto::UsageReportAck {},
                ))
            }
        };

        proto::ServerMessage {
            request_id,
            payload,
        }
    }
}

/// Trait for converting GatewayResponse to proto ServerMessage.
pub trait GatewayResponseToProto {
    fn to_proto(&self, request_id: u64) -> proto::ServerMessage;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_usage_report_roundtrip() {
        let original = budget::UsageReport {
            agent_id: "com.rollball.test".to_string(),
            provider: "openai".to_string(),
            tokens_used: 1000,
            cost_usd: 0.03,
            timestamp: chrono::Utc::now(),
            error: Some("rate_limit".to_string()),
        };

        let proto_msg: proto::UsageReportRequest = (&original).into();
        let restored: budget::UsageReport = proto_msg.into();

        assert_eq!(restored.agent_id, original.agent_id);
        assert_eq!(restored.provider, original.provider);
        assert_eq!(restored.tokens_used, original.tokens_used);
        assert!((restored.cost_usd - original.cost_usd).abs() < f64::EPSILON);
        assert_eq!(restored.error, original.error);
    }

    #[test]
    fn test_context_usage_info_roundtrip() {
        let original = protocol::ContextUsageInfo {
            context_window: 128000,
            input_tokens: 50000,
            output_tokens: 2000,
            total_tokens: 52000,
            max_input_tokens: Some(120000),
            usable_context: 96000,
            usage_percent: 42,
        };

        let proto_msg: proto::ContextUsageInfo = (&original).into();
        let restored: protocol::ContextUsageInfo = proto_msg.into();

        assert_eq!(restored.context_window, original.context_window);
        assert_eq!(restored.input_tokens, original.input_tokens);
        assert_eq!(restored.output_tokens, original.output_tokens);
        assert_eq!(restored.total_tokens, original.total_tokens);
        assert_eq!(restored.max_input_tokens, original.max_input_tokens);
        assert_eq!(restored.usable_context, original.usable_context);
        assert_eq!(restored.usage_percent, original.usage_percent);
    }

    #[test]
    fn test_identity_entry_roundtrip() {
        let original = identity::IdentityEntry {
            field: "city".to_string(),
            value: "Shanghai".to_string(),
            confidence: 0.9,
            category: identity::IdentityCategory::Identity,
            privacy: identity::PrivacyLevel::Personal,
            source: "user_input".to_string(),
            updated_at: "2026-04-24T00:00:00Z".to_string(),
        };

        let proto_msg: proto::IdentityEntry = (&original).into();
        let restored: identity::IdentityEntry = proto_msg.into();

        assert_eq!(restored.field, original.field);
        assert_eq!(restored.value, original.value);
        assert!((restored.confidence - original.confidence).abs() < f32::EPSILON);
        assert_eq!(restored.category, original.category);
        assert_eq!(restored.privacy, original.privacy);
    }

    #[test]
    fn test_model_capabilities_info_roundtrip() {
        let original = protocol::ModelCapabilitiesInfo {
            context_window: 128000,
            max_output_tokens: 16384,
            max_input_tokens: Some(120000),
            supports_tool_calling: true,
            supports_reasoning: Some(true),
            supports_attachment: Some(true),
            supports_temperature: None,
            cost: Some(protocol::ModelCostInfo {
                input_per_million: Some(2.5),
                output_per_million: Some(10.0),
            }),
            modalities: Some(protocol::ModelModalities {
                input: vec!["text".to_string(), "image".to_string()],
                output: vec!["text".to_string()],
            }),
            name: Some("GPT-4o".to_string()),
            family: Some("gpt".to_string()),
            knowledge_cutoff: Some("2025-04".to_string()),
        };

        let proto_msg: proto::ModelCapabilitiesInfo = (&original).into();
        let restored: protocol::ModelCapabilitiesInfo = proto_msg.into();

        assert_eq!(restored.context_window, original.context_window);
        assert_eq!(restored.max_output_tokens, original.max_output_tokens);
        assert_eq!(restored.max_input_tokens, original.max_input_tokens);
        assert_eq!(restored.supports_tool_calling, original.supports_tool_calling);
        assert_eq!(restored.name, original.name);
    }

    #[test]
    fn test_gateway_request_key_release_to_proto() {
        let req = protocol::GatewayRequest::KeyRelease {
            provider: "openai".to_string(),
        };
        let msg = req.to_proto(42);
        assert_eq!(msg.request_id, 42);
        assert!(matches!(
            msg.payload,
            Some(proto::client_message::Payload::KeyRelease(_))
        ));
    }

    #[test]
    fn test_gateway_response_budget_info_to_proto() {
        let resp = protocol::GatewayResponse::BudgetInfo {
            remaining_tokens: 50000,
            remaining_cost_usd: 1.5,
        };
        let msg = resp.to_proto(1);
        assert_eq!(msg.request_id, 1);
        assert!(matches!(
            msg.payload,
            Some(proto::server_message::Payload::BudgetInfo(_))
        ));
    }

    #[test]
    fn test_session_info_dto_roundtrip() {
        let original = protocol::SessionInfoDto {
            session_id: "20260503_143022_a1b2c3".to_string(),
            created_at: "2026-05-03T14:30:22Z".to_string(),
            message_count: 42,
            title: Some("Test Session".to_string()),
            corrupted: false,
        };

        let proto_msg: proto::SessionInfoDto = (&original).into();
        let restored: protocol::SessionInfoDto = proto_msg.into();

        assert_eq!(restored.session_id, original.session_id);
        assert_eq!(restored.created_at, original.created_at);
        assert_eq!(restored.message_count, original.message_count);
        assert_eq!(restored.title, original.title);
    }

    #[test]
    fn test_conversation_entry_dto_roundtrip() {
        let original = protocol::ConversationEntryDto {
            id: "msg-001".to_string(),
            ts: "2026-05-03T14:30:22.123Z".to_string(),
            role: "user".to_string(),
            content: "Hello".to_string(),
            metadata: Some(serde_json::json!({"tool_name": "bash"})),
        };

        let proto_msg: proto::ConversationEntryDto = (&original).into();
        let restored: protocol::ConversationEntryDto = proto_msg.into();

        assert_eq!(restored.id, original.id);
        assert_eq!(restored.role, original.role);
        assert_eq!(restored.content, original.content);
        assert!(restored.metadata.is_some());
    }
}
