import os

f = os.path.join('core', 'rollball-runtime', 'src', 'grpc', 'client.rs')
with open(f, encoding='utf-8') as fh:
    c = fh.read()

# ============================================
# Fix 3: AgentHelloConfig construction (lines 426-455)
# Replace old proto fields with new JSON deserialization
# ============================================
old_config = """                    let config = AgentHelloConfig {
                        provider: if result.provider.is_empty() {
                            None
                        } else {
                            Some(result.provider)
                        },
                        model: if result.model.is_empty() {
                            None
                        } else {
                            Some(result.model)
                        },
                        api_key: if result.api_key.is_empty() {
                            None
                        } else {
                            Some(result.api_key)
                        },
                        base_url: if result.base_url.is_empty() {
                            None
                        } else {
                            Some(result.base_url)
                        },
                        models: result.models,
                        model_capabilities: result.model_capabilities.map(|c| c.into()),
                        max_output_tokens_limit: result.max_output_tokens_limit,
                        protocol_type: match result.protocol_type.as_str() {
                            "anthropic" => ProtocolType::Anthropic,
                            "ollama" => ProtocolType::Ollama,
                            _ => ProtocolType::OpenAI,
                        },
                    };"""

new_config = """                    let config = AgentHelloConfig {
                        provider_list: if result.provider_list_json.is_empty() {
                            None
                        } else {
                            serde_json::from_str(&result.provider_list_json).ok()
                        },
                        provider_list_version: result.provider_list_version,
                        mcp_list: if result.mcp_list_json.is_empty() {
                            None
                        } else {
                            serde_json::from_str(&result.mcp_list_json).ok()
                        },
                        mcp_list_version: result.mcp_list_version,
                        provider_key_vault: if result.provider_key_vault_json.is_empty() {
                            vec![]
                        } else {
                            serde_json::from_str(&result.provider_key_vault_json).unwrap_or_default()
                        },
                        mcp_key_vault: if result.mcp_key_vault_json.is_empty() {
                            vec![]
                        } else {
                            serde_json::from_str(&result.mcp_key_vault_json).unwrap_or_default()
                        },
                    };"""

if old_config in c:
    c = c.replace(old_config, new_config)
    print('Fix 3: AgentHelloConfig construction updated')
else:
    print('Fix 3: Pattern NOT found')

# ============================================
# Fix 4: proto_to_gateway_response AgentHelloResult branch (lines 1042-1063)
# ============================================
old_ahr = """        Some(ServerPayload::AgentHelloResult(r)) => GatewayResponse::AgentHelloResult {
            success: r.success,
            error: if r.error.is_empty() {
                None
            } else {
                Some(r.error)
            },
            provider: if r.provider.is_empty() { None } else { Some(r.provider) },
            model: if r.model.is_empty() { None } else { Some(r.model) },
            api_key: if r.api_key.is_empty() { None } else { Some(r.api_key) },
            base_url: if r.base_url.is_empty() { None } else { Some(r.base_url) },
            models: r.models,
            model_capabilities: r.model_capabilities.map(|c| c.into()),
            max_output_tokens_limit: r.max_output_tokens_limit,
            protocol_type: match r.protocol_type.as_str() {
                "anthropic" => ProtocolType::Anthropic,
                "ollama" => ProtocolType::Ollama,
                _ => ProtocolType::OpenAI,
            },
            // ADR-009: identity_entries not available via gRPC bridge (consumed at IPC level)
            identity_entries: vec![],
        },"""

new_ahr = """        Some(ServerPayload::AgentHelloResult(r)) => {
            let provider_list: Option<Vec<ProviderListItem>> = if r.provider_list_json.is_empty() {
                None
            } else {
                serde_json::from_str(&r.provider_list_json).ok()
            };
            let mcp_list: Option<Vec<McpListItem>> = if r.mcp_list_json.is_empty() {
                None
            } else {
                serde_json::from_str(&r.mcp_list_json).ok()
            };
            let provider_key_vault: Vec<ProviderKeyEntry> = if r.provider_key_vault_json.is_empty() {
                vec![]
            } else {
                serde_json::from_str(&r.provider_key_vault_json).unwrap_or_default()
            };
            let mcp_key_vault: Vec<McpKeyEntry> = if r.mcp_key_vault_json.is_empty() {
                vec![]
            } else {
                serde_json::from_str(&r.mcp_key_vault_json).unwrap_or_default()
            };
            GatewayResponse::AgentHelloResult {
                success: r.success,
                error: if r.error.is_empty() { None } else { Some(r.error) },
                provider_list,
                provider_list_version: r.provider_list_version,
                mcp_list,
                mcp_list_version: r.mcp_list_version,
                provider_key_vault,
                mcp_key_vault,
                identity_entries: vec![],
            }
        },"""

if old_ahr in c:
    c = c.replace(old_ahr, new_ahr)
    print('Fix 4: proto_to_gateway_response AgentHelloResult updated')
else:
    print('Fix 4: Pattern NOT found')

with open(f, 'w', encoding='utf-8') as fh:
    fh.write(c)

print('Done fixing grpc/client.rs')
