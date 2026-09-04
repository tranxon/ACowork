//! Shared builders for the 5 global-resource snapshots.
//!
//! Both the MQTT retained publisher (`global_resources_publisher`) and the
//! HTTP projection (`http::global_resources_api`) consume the same
//! `build_available_*` functions. Centralising the builders here keeps the
//! two channels in lockstep: any new field added to the protobuf payload
//! automatically appears in the HTTP JSON projection.
//!
//! Authority split:
//! - **Vault decryption** lives here, not in the publisher. HTTP and MQTT
//!   both ship the decrypted `api_key` (loopback-only transport, see
//!   `mqtt.md §3.1.1`); doing the decrypt once avoids two divergent code
//!   paths that can drift in key-handling.
//! - **Provider configuration** (`base_url`, `models`, capabilities) comes
//!   from `resource_cache.provider_list`, which is the source of truth for
//!   *which providers exist* (Vault only carries the key).

use acowork_core::mqtt_proto::{
    AvailableEmbeddingModels, AvailableMcps, AvailableProviders, AvailableSearches, AvailableUsers,
    CompactModelRef, EmbeddingModelRef, McpRef, ProviderModelRef, ProviderRef, SearchRef,
    UserProfileRef,
};
use acowork_core::protocol::{McpTransportDef, ProtocolType};

use crate::gateway::state::GatewayState;
use crate::util::preview_key;

/// Build `AvailableProviders` from the GatewayState resource cache.
///
/// "Available" = all providers in the cache. Phase 2+ will filter to only
/// ready providers (the health-check loop is the gate). Vault decryption is
/// performed here so both MQTT and HTTP channels see the same key.
pub(crate) fn build_available_providers(gw: &GatewayState) -> AvailableProviders {
    let cache = &gw.resource_cache.provider_list;
    let providers: Vec<ProviderRef> = cache
        .providers
        .iter()
        .map(|p| {
            // Decrypt the provider's API key from the Gateway's Vault.
            // Empty string when no key is configured (e.g. local Ollama).
            // MQTT broker is localhost-only so it's safe to publish the
            // decrypted key in the retained payload — see mqtt.md §3.1.1.
            let api_key = gw
                .vault
                .get_provider(&p.id)
                .map(|entry| entry.api_key)
                .unwrap_or_default();
            // DIAG: log the byte range about to be published to MQTT so
            // we can correlate this preview with the `build_provider_for`
            // + `OpenAI streaming request prepared` lines on the runtime
            // side. If this preview matches the runtime's prefix, the
            // corruption is upstream of the publisher (vault layer).
            tracing::info!(
                provider_id = %p.id,
                api_key_len = api_key.len(),
                api_key_prefix = %preview_key(&api_key),
                "global_resources: building ProviderRef for AvailableProviders snapshot"
            );
            ProviderRef {
                id: p.id.clone(),
                base_url: p.base_url.clone(),
                protocol_type: map_protocol_type(&p.protocol_type).into(),
                models: p
                    .models
                    .iter()
                    .map(|m| {
                        let (input_modalities, output_modalities) = m
                            .capabilities
                            .modalities
                            .as_ref()
                            .map(|moda| (moda.input.clone(), moda.output.clone()))
                            .unwrap_or_default();
                        ProviderModelRef {
                            id: m.id.clone(),
                            capabilities: Some(acowork_core::mqtt_proto::ModelCapabilities {
                                context_window: m.capabilities.context_window,
                                max_output_tokens: m.capabilities.max_output_tokens,
                                input_modalities,
                                output_modalities,
                                supports_reasoning: m.capabilities.supports_reasoning,
                                default_reasoning_effort: m.capabilities.default_reasoning_effort.clone(),
                            }),
                            max_output_tokens_limit: m.max_output_tokens_limit,
                        }
                    })
                    .collect(),
                compact_model: p.compact_model.clone().unwrap_or_default(),
                custom: p.custom,
                api_key,
            }
        })
        .collect();

    AvailableProviders {
        version: cache.version,
        providers,
        // ADR-056: forward the global default compact model so Runtime can
        // resolve the distillation fallback chain without an extra round-trip.
        // `None` (i.e. not set in `provider_list.json`) means "no global
        // override" — Runtime falls back to provider.compact_model and chat.
        default_compact_model: cache.default_compact_model.clone().map(|r| {
            CompactModelRef {
                provider_id: r.provider_id,
                model_id: r.model_id,
            }
        }),
    }
}

/// Build `AvailableMcps` from the GatewayState resource cache.
///
/// The `auth_token` is extracted from env vars and headers via
/// `extract_api_key_from_mcp_config` — same logic that builds
/// `mcp_key_vault` for gRPC AgentHello. Empty when no auth required.
pub(crate) fn build_available_mcps(gw: &GatewayState) -> AvailableMcps {
    let cache = &gw.resource_cache.mcp_list;
    // MCP catalog is the source of truth for env/headers, not the
    // resource_cache (which only stores server lists). Load it here.
    let data_dir = gw
        .config
        .as_ref()
        .map(|c| std::path::PathBuf::from(&c.data_dir))
        .unwrap_or_else(|| std::path::PathBuf::from("./data"));
    let catalog: Vec<acowork_core::protocol::McpServerConfigDef> =
        crate::http::mcp_catalog_api::load_mcp_catalog(&data_dir)
            .ok()
            .unwrap_or_default();
    let servers: Vec<McpRef> = cache
        .servers
        .iter()
        .map(|s| {
            // Look up the catalog entry to extract env/headers for the token.
            let auth_token = catalog
                .iter()
                .find(|c| c.name == s.id)
                .and_then(crate::resource_cache::extract_api_key_from_mcp_config)
                .unwrap_or_default();
            McpRef {
                id: s.id.clone(),
                name: s.name.clone(),
                transport: map_mcp_transport(&s.transport).into(),
                url: s.url.clone().unwrap_or_default(),
                command: s.command.clone(),
                args: s.args.clone(),
                env: s.env.clone(),
                headers: s.headers.clone(),
                tool_timeout_secs: s.tool_timeout_secs.unwrap_or(0),
                auth_token,
            }
        })
        .collect();

    // T3-4: 自动注入 pm MCP（设计 §6.1 / §21）。
    //
    // `pm_mcp_url` 在 `Gateway::run` 启动时设置（`Some` ⇔ PM 服务已启动且
    // `pm.auto_inject_mcp = true`）。注入后，每个 Agent 的 catalog 都会出现
    // `name = "pm"` 的 HTTP MCP server，Agent 启动即可调用 `pm_*` 工具。
    //
    // 身份：pm MCP 通过 `X-MCP-Actor` header 识别调用者（= agent_id，设计
    // §9.2）。这里下发 `{agent_id}` 模板占位符，Runtime 连接时替换为实际
    // agent_id（见 Runtime `template_mcp_identity`）。
    let mut servers = servers;
    if let Some(pm_url) = &gw.pm_mcp_url {
        let mut headers = std::collections::HashMap::new();
        headers.insert("X-MCP-Actor".to_string(), "{agent_id}".to_string());
        servers.push(McpRef {
            id: "pm".to_string(),
            name: "pm".to_string(),
            transport: map_mcp_transport(&McpTransportDef::Http).into(),
            url: pm_url.clone(),
            command: String::new(),
            args: Vec::new(),
            env: std::collections::HashMap::new(),
            headers,
            tool_timeout_secs: 60,
            auth_token: String::new(),
        });
    }

    AvailableMcps {
        version: cache.version,
        servers,
    }
}

/// Build `AvailableSearches` from the GatewayState resource cache.
///
/// `api_key` is decrypted from the Gateway's Vault at snapshot time,
/// mirroring the logic in `build_search_key_vault` for gRPC AgentHello.
/// Empty when the search provider has no key configured.
pub(crate) fn build_available_searches(gw: &GatewayState) -> AvailableSearches {
    let cache = &gw.resource_cache.search_list;
    let providers: Vec<SearchRef> = cache
        .providers
        .iter()
        .map(|s| {
            let api_key = gw
                .vault
                .get_search_key(&s.id)
                .map(|entry| entry.api_key)
                .unwrap_or_default();
            SearchRef {
                id: s.id.clone(),
                name: s.name.clone(),
                description: s.description.clone(),
                requires_api_key: s.requires_api_key,
                base_url: s.base_url.clone(),
                api_key,
            }
        })
        .collect();

    AvailableSearches {
        version: cache.version,
        providers,
    }
}

/// Build `AvailableEmbeddingModels` from the GatewayState resource cache
/// + embed process state + active cloud embedding selection (Vault).
pub(crate) fn build_available_embedding_models(gw: &GatewayState) -> AvailableEmbeddingModels {
    let cache = &gw.resource_cache.embedding_models;
    let models: Vec<EmbeddingModelRef> = cache
        .models
        .iter()
        .map(|m| EmbeddingModelRef {
            id: m.id.clone(),
            name: m.name.clone(),
            description: m.description.clone().unwrap_or_default(),
            dimension: m.dimension as u32,
            max_tokens: m.max_tokens as u32,
            size_mb: m.size_mb,
            languages: m.languages.clone(),
            hf_repo: m.hf_repo.clone(),
            onnx_file: m.onnx_file.clone(),
            tokenizer_file: m.tokenizer_file.clone(),
            bundled: m.bundled,
            recommended: m.recommended,
        })
        .collect();

    // Active model info from embed process state.
    let (active_model_id, active_dimension, endpoint) = match &gw.embed_process {
        Some(eps) if eps.ready => (
            eps.active_model_id.clone().unwrap_or_default(),
            eps.active_dimension.unwrap_or(0) as u32,
            // ADR-055 D3: advertise host instead of hard-coded 127.0.0.1.
            format!("http://{}:{}/v1", gw.advertise_host, eps.port),
        ),
        _ => (String::new(), 0, String::new()),
    };

    // Cloud embedding selection (S1-5b): read active selection from disk +
    // decrypt the API key from the Vault. When the snapshot is empty, the
    // proto defaults to empty strings and Runtime continues with local
    // ONNX.
    let data_dir: Option<&std::path::Path> = gw
        .config
        .as_ref()
        .map(|c| std::path::Path::new(&c.data_dir));
    let cloud = data_dir
        .map(|dir| crate::embedding_providers::resolve_active_cloud_embedding(dir, &gw.vault))
        .unwrap_or_default();

    AvailableEmbeddingModels {
        version: cache.version,
        models,
        active_model_id,
        active_dimension,
        endpoint,
        active_provider_id: cloud.active_provider_id,
        active_api_key: cloud.active_api_key,
        active_base_url: cloud.active_base_url,
    }
}

/// ADR-042: Build `AvailableUsers` from the GatewayState user profile list.
///
/// Finds the user with `is_active == true` and serialises it into
/// `UserProfileRef`. UI-only fields (avatar / builtin_avatar /
/// created_at / updated_at / is_active) are omitted — Runtime never
/// renders user profile UI. `custom` HashMap is serialised to JSON.
pub(crate) fn build_available_users(gw: &GatewayState) -> AvailableUsers {
    let list = &gw.resource_cache.user_profile_list;
    let active = list.users.iter().find(|u| u.is_active).map(|u| {
        let custom_json = serde_json::to_string(&u.custom).unwrap_or_else(|e| {
            tracing::warn!(
                user_id = %u.user_id,
                error = %e,
                "Failed to serialise UserProfile.custom to JSON; sending empty"
            );
            "{}".to_string()
        });
        UserProfileRef {
            user_id: u.user_id.clone(),
            display_name: u.display_name.clone(),
            language: u.language.clone(),
            timezone: u.timezone.clone(),
            city: u.city.clone(),
            country: u.country.clone(),
            occupation: u.occupation.clone(),
            communication_style: u.communication_style.clone(),
            custom_json,
        }
    });

    AvailableUsers {
        version: list.version,
        active_user: active,
    }
}

// ── Enum mappers ─────────────────────────────────────────────────────

pub(crate) fn map_protocol_type(pt: &ProtocolType) -> acowork_core::mqtt_proto::LlmProtocol {
    match pt {
        ProtocolType::OpenAI => acowork_core::mqtt_proto::LlmProtocol::Openai,
        ProtocolType::Anthropic => acowork_core::mqtt_proto::LlmProtocol::Anthropic,
        ProtocolType::Google => acowork_core::mqtt_proto::LlmProtocol::Google,
        ProtocolType::Ollama => acowork_core::mqtt_proto::LlmProtocol::Ollama,
    }
}

pub(crate) fn map_mcp_transport(t: &McpTransportDef) -> acowork_core::mqtt_proto::McpTransport {
    match t {
        McpTransportDef::Stdio => acowork_core::mqtt_proto::McpTransport::Stdio,
        McpTransportDef::Http => acowork_core::mqtt_proto::McpTransport::Http,
        McpTransportDef::Sse => acowork_core::mqtt_proto::McpTransport::Sse,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_map_protocol_type() {
        assert_eq!(
            map_protocol_type(&ProtocolType::OpenAI),
            acowork_core::mqtt_proto::LlmProtocol::Openai
        );
        assert_eq!(
            map_protocol_type(&ProtocolType::Anthropic),
            acowork_core::mqtt_proto::LlmProtocol::Anthropic
        );
    }

    #[test]
    fn test_map_mcp_transport() {
        assert_eq!(
            map_mcp_transport(&McpTransportDef::Stdio),
            acowork_core::mqtt_proto::McpTransport::Stdio
        );
    }

    #[test]
    fn test_build_available_providers_empty() {
        let gw = GatewayState::new("/tmp/test-vault");
        let payload = build_available_providers(&gw);
        assert_eq!(payload.version, 0);
        assert!(payload.providers.is_empty());
    }

    /// T4-1（P4 远程）：`pm_mcp_url` 存在时，`build_available_mcps` 把 pm MCP
    /// 注入全局 `acowork/global/mcps` 资源，远程 Runtime 即可拿到 advertise
    /// endpoint（`http://{advertise_host}:{gw_http_port}{mcp_http_path}`）。
    #[test]
    fn test_build_available_mcps_injects_pm_mcp_when_url_set() {
        let mut gw = GatewayState::new("/tmp/test-vault");
        // 模拟 `Gateway::run` 在 PM 启动成功且 `pm.auto_inject_mcp` 时写入的
        // advertise endpoint（ADR-055 D3：用 advertise_host 而非 127.0.0.1）。
        gw.pm_mcp_url = Some("http://192.168.1.50:19876/api/pm/mcp".to_string());

        let payload = build_available_mcps(&gw);

        let pm = payload
            .servers
            .iter()
            .find(|s| s.id == "pm")
            .expect("pm MCP should be injected into global mcps when pm_mcp_url is set");
        assert_eq!(pm.name, "pm");
        assert_eq!(pm.url, "http://192.168.1.50:19876/api/pm/mcp");
        assert_eq!(
            pm.transport,
            map_mcp_transport(&McpTransportDef::Http) as i32,
            "pm MCP must use HTTP transport"
        );
        // 身份模板：Runtime 侧替换为实际 agent_id（X-MCP-Actor header）。
        assert_eq!(
            pm.headers.get("X-MCP-Actor").map(|s| s.as_str()),
            Some("{agent_id}"),
            "pm MCP must carry X-MCP-Actor identity template"
        );
        assert_eq!(pm.tool_timeout_secs, 60);
    }

    /// T4-1 反向：`pm_mcp_url` 为 None（PM 未启动 / auto_inject_mcp=false）时
    /// **不**注入 pm MCP，避免向 Agent 暴露不可达端点。
    #[test]
    fn test_build_available_mcps_skips_pm_when_url_none() {
        let gw = GatewayState::new("/tmp/test-vault");
        assert!(gw.pm_mcp_url.is_none());

        let payload = build_available_mcps(&gw);
        assert!(
            !payload.servers.iter().any(|s| s.id == "pm"),
            "pm MCP must NOT be injected when pm_mcp_url is None"
        );
    }
}
