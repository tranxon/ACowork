p = 'core/rollball-runtime/src/cli.rs'
with open(p, encoding='utf-8') as f:
    c = f.read()

old = '''                        // Handle model/provider switch from Gateway (same pattern as model_switch action)
                        if model.is_some() || provider.is_some() {
                            if let Some(ref model_name) = model {
                                let provider_ref = provider.as_deref();
                                save_agent_model(&work_dir, model_name, provider_ref);
                                session_manager.update_model_override(model_name.clone());
                            }
                            tracing::info!(
                                model = ?model,
                                provider = ?provider,
                                "Model/provider override applied from RuntimeConfigUpdate"
                            );
                        }


                        // Hot-rebuild tool definitions when active_tools changes.'''

new = '''                        // Handle model/provider switch from Gateway (same pattern as model_switch action)
                        if model.is_some() || provider.is_some() {
                            if let Some(ref model_name) = model {
                                let provider_ref = provider.as_deref();
                                save_agent_model(&work_dir, model_name, provider_ref);
                                session_manager.update_model_override(model_name.clone());
                            }
                            tracing::info!(
                                model = ?model,
                                provider = ?provider,
                                "Model/provider override applied from RuntimeConfigUpdate"
                            );
                        }

                        // Persist per-agent config to workspace/config/agent_config.json.
                        // This consolidates all overrides into a single file owned by Runtime,
                        // replacing the former Gateway-side data/agent_configs/{agent_id}.json.
                        {
                            let overrides = &session_manager.runtime_overrides;
                            let agent_cfg = crate::agent_config::AgentConfig {
                                max_output_tokens: overrides.max_output_tokens,
                                max_iterations: overrides.max_iterations,
                                temperature: overrides.temperature,
                                system_prompt_override: overrides.system_prompt_override.clone(),
                                active_tools: overrides.active_tools.clone().unwrap_or_default(),
                                shell_approval_threshold: overrides.shell_approval_threshold.clone(),
                                mcp_servers: vec![],
                                available_models: vec![],
                            };
                            let _ = crate::agent_config::save_agent_config(
                                std::path::Path::new(&work_dir),
                                &agent_cfg,
                            );
                        }


                        // Hot-rebuild tool definitions when active_tools changes.'''

c = c.replace(old, new)
with open(p, 'w', encoding='utf-8') as f:
    f.write(c)
print('ok')
