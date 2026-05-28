import { useState, useEffect, useCallback, useMemo } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { VaultKeyEntry, ModelInfo, ModelCapabilitiesInfo, ModelCapabilitiesMap, ProviderListEntry, McpServerConfigDef, McpTransportDef, McpPresetDef } from "../../lib/types";
import { cn } from "../../lib/utils";
import { inputBase, selectBase } from "../../lib/ui-styles";
import { needsApiKey, keyPlaceholder } from "../../lib/providers";
import { fetchProviderModels } from "../../lib/gateway-api";
import { getGatewayUrl } from "../../lib/config";
import { Star } from "lucide-react";
import { useMcpStore } from "../../stores/mcpStore";
import { MCP_PRESETS, presetToServerConfig } from "../../lib/mcp-presets";
import { SearchTab } from "./SearchTab";

type HarnessTab = "providers" | "search" | "mcp";

export function HarnessPage() {
  const [activeTab, setActiveTab] = useState<HarnessTab>("providers");

  const tabs: { id: HarnessTab; label: string }[] = [
    { id: "providers", label: "Providers" },
    { id: "search", label: "Search" },
    { id: "mcp", label: "MCP" },
  ];

  return (
    <div className="flex flex-1 flex-col bg-zinc-50 dark:bg-zinc-900">
      {/* Tabs */}
      <div className="flex gap-1 border-b border-zinc-200 px-6 pt-2 dark:border-zinc-800">
        {tabs.map((tab) => (
          <button
            key={tab.id}
            onClick={() => setActiveTab(tab.id)}
            className={cn(
              "border-b-2 px-3 py-2 text-sm transition-colors",
              activeTab === tab.id
                ? "border-[var(--color-accent)] font-semibold text-zinc-700 dark:text-zinc-200"
                : "border-transparent font-normal text-zinc-500 hover:text-zinc-700 dark:text-zinc-400 dark:hover:text-zinc-300",
            )}
          >
            {tab.label}
          </button>
        ))}
      </div>

      {/* Tab content */}
      <div className="flex-1 overflow-y-auto p-6">
        {activeTab === "providers" && <ProvidersTab />}
        {activeTab === "search" && <SearchTab />}
        {activeTab === "mcp" && <McpTab />}
      </div>
    </div>
  );
}

/** Compare two provider arrays by id for equality check (avoid unnecessary re-renders) */
function providersEqual(a: ProviderListEntry[], b: ProviderListEntry[]): boolean {
  if (a.length !== b.length) return false;
  return a.every((item, i) => item.id === b[i].id && item.name === b[i].name && item.model_count === b[i].model_count);
}

/** Provider configuration */
function ProvidersTab() {
  const [keys, setKeys] = useState<VaultKeyEntry[]>([]);
  const [keysLoading, setKeysLoading] = useState(true);
  const [showAddDialog, setShowAddDialog] = useState(false);
  const [showEditDialog, setShowEditDialog] = useState<string | null>(null);
  const [newProvider, setNewProvider] = useState("openai");
  const [newKey, setNewKey] = useState("");
  const [newBaseUrl, setNewBaseUrl] = useState("");
  const [newModels, setNewModels] = useState<string[]>([]);
  const [availableModels, setAvailableModels] = useState<ModelInfo[]>([]);
  const [modelsLoading, _setModelsLoading] = useState(false);
  const [modelSearchTerm, setModelSearchTerm] = useState("");
  const [modelCapabilityFilter, setModelCapabilityFilter] = useState<string[]>([]);

  // Add dialog — model capabilities state
  const [newContextWindow, setNewContextWindow] = useState("");
  const [newMaxOutputTokens, setNewMaxOutputTokens] = useState("");
  const [newSupportsToolCalling, setNewSupportsToolCalling] = useState(true);
  const [newCompactModel, setNewCompactModel] = useState("");
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<{ success: boolean; message: string } | null>(null);

  // Edit dialog state
  const [editKey, setEditKey] = useState("");
  const [editBaseUrl, setEditBaseUrl] = useState("");
  const [editModels, setEditModels] = useState<string[]>([]);
  const [editAvailableModels, setEditAvailableModels] = useState<ModelInfo[]>([]);
  const [editModelsLoading, setEditModelsLoading] = useState(false);
  const [editModelSearchTerm, setEditModelSearchTerm] = useState("");
  const [editModelCapabilityFilter, setEditModelCapabilityFilter] = useState<string[]>([]);

  // Edit dialog — model capabilities state
  const [editContextWindow, setEditContextWindow] = useState("");
  const [editMaxOutputTokens, setEditMaxOutputTokens] = useState("");
  const [editSupportsToolCalling, setEditSupportsToolCalling] = useState(true);
  const [editCompactModel, setEditCompactModel] = useState("");
  // Gateway config for default provider indication
  const [config, setConfig] = useState<GatewayConfig | null>(null);

  // Dynamic provider list from Gateway API
  const [dynamicProviders, setDynamicProviders] = useState<ProviderListEntry[]>([]);
  const [dynamicProvidersLoading, setDynamicProvidersLoading] = useState(false);


  const fetchKeys = useCallback(async () => {
    try {
      const result = await invoke<VaultKeyEntry[]>("list_keys");
      setKeys(result);
    } catch {
      // Gateway may not be running
    } finally {
      setKeysLoading(false);
    }
  }, []);

  const fetchConfig = useCallback(async () => {
    try {
      const resp = await fetch(`${getGatewayUrl()}/api/config`);
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      const result = await resp.json() as GatewayConfig;
      setConfig(result);
    } catch {
      // Gateway may not be running
    }
  }, []);

  // Fetch dynamic provider list from Gateway API with stale-while-revalidate pattern
  const fetchDynamicProviders = useCallback(async (useCache = true) => {
    const CACHE_KEY = "rollball_models_cache";
    const CACHE_TIMESTAMP_KEY = "rollball_models_cache_timestamp";
    const CACHE_TTL = 30 * 60 * 1000; // 30 minutes

    let hasValidCache = false;
    let hasCachedData = false;

    if (useCache) {
      try {
        const cachedData = localStorage.getItem(CACHE_KEY);
        const cachedTimestamp = localStorage.getItem(CACHE_TIMESTAMP_KEY);

        if (cachedData && cachedTimestamp) {
          const timestamp = parseInt(cachedTimestamp, 10);
          const now = Date.now();

          // Use cache immediately regardless of freshness (stale-while-revalidate)
          const parsed = JSON.parse(cachedData);
          const cachedProviders = parsed.providers ?? [];
          setDynamicProviders(cachedProviders);
          hasCachedData = cachedProviders.length > 0;

          if (now - timestamp < CACHE_TTL) {
            // Cache is fresh — background refresh without loading indicator
            hasValidCache = true;
          }
          // Stale cache: show loading indicator while revalidating
        }
      } catch {
        // Ignore cache errors
      }
    }

    // Show loading indicator only when no cached data is displayed yet
    if (!hasValidCache && !hasCachedData) {
      setDynamicProvidersLoading(true);
    }

    // Fetch from Gateway API
    try {
      const response = await fetch(`${getGatewayUrl()}/api/models`);
      if (response.ok) {
        const data = await response.json();
        const newProviders = data.providers ?? [];

        // Only update state if data actually changed (avoid unnecessary re-renders)
        setDynamicProviders(prev => {
          if (providersEqual(prev, newProviders)) return prev;
          return newProviders;
        });

        // Update cache
        try {
          localStorage.setItem(CACHE_KEY, JSON.stringify(data));
          localStorage.setItem(CACHE_TIMESTAMP_KEY, Date.now().toString());
        } catch {
          // Ignore cache write errors
        }
      }
    } catch {
      // Gateway API failed, keep using cached data if available
      if (!hasValidCache && !hasCachedData) {
        // No cache and API failed, show empty state
        setDynamicProviders([]);
      }
    } finally {
      setDynamicProvidersLoading(false);
    }
  }, []);

  useEffect(() => {
    fetchKeys();
    fetchConfig();
    // First load: use cache for instant display, refresh in background
    fetchDynamicProviders(true);
  }, [fetchKeys, fetchConfig, fetchDynamicProviders]);

  // Fetch available models for a provider from Gateway API
  const fetchModels = useCallback(async (providerId: string): Promise<ModelInfo[]> => {
    // Check localStorage cache for this provider
    const CACHE_KEY = `rollball_models_${providerId}`;
    const CACHE_TIMESTAMP_KEY = `rollball_models_${providerId}_timestamp`;
    const CACHE_TTL = 30 * 60 * 1000; // 30 minutes

    try {
      const cachedData = localStorage.getItem(CACHE_KEY);
      const cachedTimestamp = localStorage.getItem(CACHE_TIMESTAMP_KEY);

      if (cachedData && cachedTimestamp) {
        const timestamp = parseInt(cachedTimestamp, 10);
        const now = Date.now();

        // Use cache if it's still fresh
        if (now - timestamp < CACHE_TTL) {
          const parsed = JSON.parse(cachedData);
          return parsed.models ?? [];
        } else {
          // Cache expired
          localStorage.removeItem(CACHE_KEY);
          localStorage.removeItem(CACHE_TIMESTAMP_KEY);
        }
      }
    } catch {
      // Ignore cache errors
    }

    // Fetch from Gateway API
    try {
      const data = await fetchProviderModels(providerId);
      const models = data.models ?? [];

      // Update cache
      try {
        localStorage.setItem(CACHE_KEY, JSON.stringify(data));
        localStorage.setItem(CACHE_TIMESTAMP_KEY, Date.now().toString());
      } catch {
        // Ignore cache write errors
      }

      return models;
    } catch {
      // No fallback — Gateway always returns model list
      return [];
    }
  }, []);

  const handleAdd = async () => {
    // First test the API key
    if (needsApiKey(newProvider) && !newKey.trim()) {
      setTestResult({ success: false, message: "Please enter an API Key first" });
      return;
    }

    setTesting(true);
    setTestResult(null);

    try {
      // Temporarily add the key
      await invoke("add_key", {
        provider: newProvider,
        key: newKey,
        baseUrl: newBaseUrl || undefined,
      });

      // Try to fetch models to verify the key works
      await fetchProviderModels(newProvider);

      setTestResult({ success: true, message: "API Key is valid!" });

      // Remove the temporary key
      await invoke("remove_key", { provider: newProvider });
    } catch (e: any) {
      const errorMsg = e?.message || e?.toString() || "Test failed";
      setTestResult({ success: false, message: errorMsg });
      setTesting(false);
      return;
    }

    setTesting(false);

    // Test passed, proceed with saving
    // Get effective values (prefer models.dev data if available)
    const primaryModel = newModels.length > 0 ? newModels[0] : "";
    const modelInfo = availableModels.find(m => m.id === primaryModel);
    const hasModelsDevData = !!(modelInfo && (modelInfo.context_window || modelInfo.max_tokens));
    const effectiveContextWindow = hasModelsDevData
      ? (modelInfo?.context_window?.toString() ?? newContextWindow)
      : newContextWindow;
    const effectiveMaxOutputTokens = hasModelsDevData
      ? (modelInfo?.max_tokens?.toString() ?? newMaxOutputTokens)
      : newMaxOutputTokens;
    const effectiveSupportsToolCalling = hasModelsDevData
      ? (modelInfo?.tool_call ?? newSupportsToolCalling)
      : newSupportsToolCalling;
    const effectiveReasoning = hasModelsDevData
      ? (modelInfo?.reasoning ?? undefined)
      : undefined;
    const effectiveModalities = hasModelsDevData && modelInfo?.input_modalities?.length
      ? { input: modelInfo.input_modalities }
      : undefined;

    // Rust requires context_window to be present (u64, not Option)
    // Default to 128000 if not specified (safe default for most models)
    const ctxWindow = effectiveContextWindow ? parseInt(effectiveContextWindow) : 128000;

    // Build per-model capabilities map
    const modelCapabilities: ModelCapabilitiesMap = {};
    if (newModels.length > 0) {
      const maxOutTokens = effectiveMaxOutputTokens ? parseInt(effectiveMaxOutputTokens) : 0;
      for (const modelId of newModels) {
        const mi = availableModels.find(m => m.id === modelId);
        modelCapabilities[modelId] = {
          context_window: mi?.context_window ?? ctxWindow,
          max_output_tokens: mi?.max_tokens ?? maxOutTokens,
          supports_tool_calling: mi?.tool_call ?? effectiveSupportsToolCalling,
          supports_reasoning: mi?.reasoning ?? undefined,
          modalities: mi?.input_modalities?.length ? { input: mi.input_modalities } : undefined,
        };
      }
    }
    try {
      await invoke("add_key", {
        provider: newProvider,
        key: newKey,
        baseUrl: newBaseUrl || undefined,
        defaultModel: undefined,
        models: newModels.length > 0 ? newModels : undefined,
        modelCapabilities,
        compactModel: newCompactModel || undefined,
      });
      setShowAddDialog(false);
      setNewKey("");
      setNewModels([]);
      setNewContextWindow("");
      setNewMaxOutputTokens("");
      setNewSupportsToolCalling(true);
      setTestResult(null);
      await fetchKeys();
      await fetchConfig();
      window.dispatchEvent(new CustomEvent('models-added'));
    } catch (e) {
      alert(`Failed to add key: ${e}`);
    }
  };

  const handleRemove = async (provider: string) => {
    if (!confirm(`Remove key for ${provider}?`)) return;
    try {
      await invoke("remove_key", { provider });
      await fetchKeys();
    } catch (e) {
      alert(`Failed to remove key: ${e}`);
    }
  };

  // Set a configured provider as the default for the Gateway
  const handleSetDefaultProvider = async (provider: string) => {
    try {
      const entry = keys.find((k) => k.provider === provider);
      await fetch(`${getGatewayUrl()}/api/config`, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          default_provider: provider,
          default_model: entry?.models?.[0] || entry?.default_model || undefined,
        }),
      });
      await fetchConfig();
    } catch (e) {
      alert(`Failed to set default provider: ${e}`);
    }
  };

  const handleEdit = async (provider: string) => {
    const keyEntry = keys.find((k) => k.provider === provider);
    const dynamicProvider = dynamicProviders.find((p) => p.id === provider);
    setEditKey(keyEntry?.key_preview ?? "");
    setEditBaseUrl(keyEntry?.base_url ?? dynamicProvider?.api ?? "");
    setEditModels(keyEntry?.models?.length ? keyEntry.models : keyEntry?.default_model ? [keyEntry.default_model] : []);
    setEditModelSearchTerm("");
    setEditModelCapabilityFilter([]);
    // Load existing capabilities from VaultKeyEntry
    setEditContextWindow(keyEntry?.model_capabilities?.context_window?.toString() ?? "");
    setEditMaxOutputTokens(keyEntry?.model_capabilities?.max_output_tokens?.toString() ?? "");
    setEditSupportsToolCalling(keyEntry?.model_capabilities?.supports_tool_calling ?? true);
    setEditCompactModel(keyEntry?.compact_model ?? "");
    setShowEditDialog(provider);
    // Fetch models
    setEditModelsLoading(true);
    const models = await fetchModels(provider);
    setEditAvailableModels(models);
    setEditModelsLoading(false);
  };

  const handleEditSave = async () => {
    if (!showEditDialog) return;
    console.log("[handleEditSave] provider:", showEditDialog, "models:", editModels, "key:", editKey ? "(present)" : "(empty)");
    try {
      // NOTE: editKey is initialized from key_preview (masked), NOT the real API key.
      // Do NOT send the key field unless the user explicitly entered a new key.
      // The Gateway update_key API preserves the existing key when key is omitted.
      const updatePayload: Record<string, unknown> = {
        provider: showEditDialog,
        baseUrl: editBaseUrl || undefined,
        defaultModel: undefined,
        models: editModels.length > 0 ? editModels : undefined,
      };
      // Only include key if user actually typed a new one (not the masked preview)
      const keyEntry = keys.find((k) => k.provider === showEditDialog);
      if (editKey && editKey !== keyEntry?.key_preview) {
        updatePayload.key = editKey;
      }
      // Build per-model capabilities map
      if (editContextWindow || editMaxOutputTokens) {
        const cw = Number(editContextWindow);
        const mot = Number(editMaxOutputTokens);
        if ((editContextWindow && (!Number.isFinite(cw) || cw <= 0)) ||
          (editMaxOutputTokens && (!Number.isFinite(mot) || mot <= 0))) {
          alert('Context Window and Max Output Tokens must be positive numbers');
          return;
        }
        const caps: ModelCapabilitiesMap = {};
        for (const modelId of editModels) {
          const modelInfo = editAvailableModels.find(m => m.id === modelId);
          caps[modelId] = {
            context_window: cw || 0,
            max_output_tokens: mot || 0,
            supports_tool_calling: editSupportsToolCalling,
            supports_reasoning: modelInfo?.reasoning ?? undefined,
            modalities: modelInfo?.input_modalities?.length ? { input: modelInfo.input_modalities } : undefined,
          };
        }
        updatePayload.modelCapabilities = caps;
      }
      // Include compact_model if set
      if (editCompactModel) {
        updatePayload.compactModel = editCompactModel;
      } else {
        updatePayload.compactModel = null;  // Explicitly clear if empty
      }
      console.log("[handleEditSave] payload:", JSON.stringify(updatePayload));
      await invoke("update_key", updatePayload);
      console.log("[handleEditSave] success");
      setShowEditDialog(null);
      await fetchKeys();
      await fetchConfig();
      window.dispatchEvent(new CustomEvent('models-added'));
    } catch (e) {
      console.error("[handleEditSave] error:", e);
      alert(`Failed to update key: ${e}`);
    }
  };

  // Toggle a model in the selection list
  const toggleModel = (model: string, currentList: string[], setList: (v: string[]) => void) => {
    if (currentList.includes(model)) {
      setList(currentList.filter((m) => m !== model));
    } else {
      setList([...currentList, model]);
    }
  };

  return (
    <div className="max-w-2xl space-y-4">
      <div className="rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <div className="flex items-center justify-between mb-3">
          <h2 className="text-xs font-medium">Provider Management</h2>
        </div>

        {/* Configured Providers (top section) — depends on fetchKeys */}
        {keysLoading ? (
          <div className="py-3 text-center text-xs text-zinc-400">Loading keys...</div>
        ) : keys.length > 0 && (
          <div>
            <h3 className="mb-2 text-xs font-medium text-zinc-500">Configured Providers</h3>
            <div className="space-y-1">
              {keys.map((keyEntry) => {
                const provider = dynamicProviders.find((p) => p.id === keyEntry.provider);
                const providerName = provider?.name || keyEntry.provider;

                return (
                  <div key={keyEntry.provider} className="rounded-lg border border-zinc-200 px-3 py-1.5 dark:border-zinc-700">
                    <div className="flex items-center justify-between gap-2">
                      <span className="shrink-0 text-xs font-medium">{providerName}</span>
                      <div className="flex items-center gap-2 shrink-0">
                        <button
                          onClick={() => handleSetDefaultProvider(keyEntry.provider)}
                          className={cn(
                            "rounded p-0.5",
                            config?.default_provider === keyEntry.provider
                              ? "text-amber-500"
                              : "text-zinc-400 hover:text-amber-500 dark:hover:text-amber-400",
                          )}
                          title={config?.default_provider === keyEntry.provider ? "Default provider" : "Set as default provider"}
                        >
                          <Star className="h-3.5 w-3.5" />
                        </button>
                        <span className="text-xs" style={{ color: "var(--color-accent)" }}>Active</span>
                        <span className="text-xs text-zinc-400">Key: {keyEntry.key_preview}</span>
                        <button
                          onClick={() => handleEdit(keyEntry.provider)}
                          className="text-xs hover:opacity-70" style={{ color: "var(--color-accent)" }}
                        >
                          Edit
                        </button>
                        <button
                          onClick={() => handleRemove(keyEntry.provider)}
                          className="text-xs text-red-500 hover:text-red-700"
                        >
                          Remove
                        </button>
                      </div>
                    </div>
                    <div className="mt-1 flex flex-wrap items-center gap-x-2 gap-y-1">
                      {keyEntry.models?.length ? (
                        <span className="text-xs" style={{ color: "var(--color-accent)" }}>{keyEntry.models.join(", ")}</span>
                      ) : keyEntry.default_model ? (
                        <span className="text-xs" style={{ color: "var(--color-accent)" }}>{keyEntry.default_model}</span>
                      ) : (
                        <span className="text-xs text-zinc-400">—</span>
                      )}
                      {keyEntry.compact_model && (
                        <span className="rounded bg-purple-100 px-1.5 py-0.5 text-xs text-purple-700 dark:bg-purple-900 dark:text-purple-300" title="Compact model for summarization">
                          compact: {keyEntry.compact_model}
                        </span>
                      )}
                    </div>
                  </div>
                );
              })}
            </div>
          </div>
        )}

      </div>

      <div className="rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">

        {/* Available Providers (bottom section) — renders independently */}
        <div>
          <div className="mb-2 flex items-center justify-between">
            <h3 className="text-xs font-medium text-zinc-500">
              Available Providers {dynamicProvidersLoading && <span className="text-zinc-400">(refreshing...)</span>}
            </h3>
            <div className="flex gap-1">
              <button
                onClick={() => {
                  // Clear localStorage cache only — keep current UI data
                  Object.keys(localStorage)
                    .filter(k => k.startsWith('rollball_models_'))
                    .forEach(k => localStorage.removeItem(k));
                  // Mark refreshing, do NOT clear dynamicProviders
                  setDynamicProvidersLoading(true);
                  // Re-fetch from API
                  fetchDynamicProviders(false);
                }}
                className="rounded px-2 py-0.5 text-xs text-zinc-500 hover:text-red-600 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:text-red-400 dark:hover:bg-zinc-800"
                title="Clear cache and refresh"
              >
                🗑 Clear Cache
              </button>
              <button
                onClick={() => fetchDynamicProviders(false)}
                disabled={dynamicProvidersLoading}
                className="rounded px-2 py-0.5 text-xs text-zinc-500 hover:text-zinc-700 hover:bg-zinc-100 disabled:opacity-50 dark:text-zinc-400 dark:hover:text-zinc-300 dark:hover:bg-zinc-800"
                title="Refresh provider list"
              >
                {dynamicProvidersLoading ? "Refreshing..." : "↻ Refresh"}
              </button>
            </div>
          </div>
          <div className="space-y-1">
            {/* Show loading indicator when no data and still loading */}
            {dynamicProvidersLoading && dynamicProviders.length === 0 ? (
              <div className="py-3 text-center text-xs text-zinc-400">Loading providers...</div>
            ) : dynamicProviders.length === 0 && !dynamicProvidersLoading ? (
              <div className="py-3 text-center text-xs text-zinc-400">No providers available</div>
            ) : (
              dynamicProviders.map((item) => {
                const providerId = item.id;
                const providerName = item.name || providerId;
                const keyEntry = keys.find((k) => k.provider === providerId);
                const modelCount = item.model_count;

                // Skip if already configured (shown in top section)
                if (keyEntry) return null;

                // Skip local providers that don't need API keys
                if (!needsApiKey(providerId)) return null;

                return (
                  <div key={providerId} className="rounded-lg border border-zinc-200 px-3 py-1.5 dark:border-zinc-700">
                    <div className="flex items-center justify-between">
                      <div className="min-w-0 flex-1">
                        <span className="text-xs font-medium">{providerName}</span>
                        {modelCount && (
                          <span className="ml-2 text-xs text-zinc-400">{modelCount} models available</span>
                        )}

                      </div>
                      <button
                        onClick={() => {
                          setNewProvider(providerId);
                          const dynamicProvider = dynamicProviders.find((p) => p.id === providerId);
                          setNewBaseUrl(dynamicProvider?.api ?? "");
                          fetchModels(providerId).then((models) => setAvailableModels(models));
                          // Reset capabilities state
                          setNewContextWindow("");
                          setNewMaxOutputTokens("");
                          setNewSupportsToolCalling(true);
                          setShowAddDialog(true);
                        }}
                        className="rounded-md bg-zinc-100 px-3 py-1 text-xs font-medium text-zinc-700 hover:bg-zinc-200 dark:bg-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-600"
                      >
                        Add Key
                      </button>
                    </div>
                  </div>
                );
              })
            )}
          </div>
        </div>
      </div>

      {/* Add key dialog */}
      {showAddDialog && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50">
          <div className="w-[440px] max-h-[85vh] overflow-y-auto rounded-lg bg-white p-6 shadow-xl dark:bg-zinc-800">
            <h3 className="mb-3 text-sm font-semibold">Add API Key: {dynamicProviders.find((p) => p.id === newProvider)?.name || newProvider}</h3>

            <div className="space-y-2">
              {/* Provider display (read-only) */}
              <div>
                <label className="mb-1 block text-xs text-zinc-500">Provider</label>
                <div className="w-full rounded-md border border-zinc-200 bg-zinc-50 px-3 py-2 text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200">
                  {dynamicProviders.find((p) => p.id === newProvider)?.name || newProvider}
                </div>
              </div>

              {needsApiKey(newProvider) && (
                <div>
                  <label className="mb-1 block text-xs text-zinc-500">API Key</label>
                  <input
                    type="password"
                    value={newKey}
                    onChange={(e) => setNewKey(e.target.value)}
                    placeholder={keyPlaceholder(newProvider)}
                    className="w-full rounded-md border border-zinc-200 px-3 py-2 text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                  />
                </div>
              )}

              {(() => {
                return (
                  <div>
                    <label className="mb-1 block text-xs text-zinc-500">Base URL</label>
                    <input
                      type="text"
                      value={newBaseUrl}
                      onChange={(e) => setNewBaseUrl(e.target.value)}
                      placeholder="https://..."
                      className="w-full rounded-md border border-zinc-200 px-3 py-2 font-mono text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                    />
                  </div>
                )
              })()}

              {/* Model selection (multi-select) */}
              <div>
                <label className="mb-1 block text-xs text-zinc-500">
                  Default Model {newModels.length > 0 && <span className="text-accent-green">({newModels.length} selected)</span>}
                </label>

                {/* Capability filters */}
                <div className="mb-2 flex gap-2">
                  <button
                    onClick={() => setModelCapabilityFilter(
                      modelCapabilityFilter.includes('tool_call')
                        ? modelCapabilityFilter.filter(f => f !== 'tool_call')
                        : [...modelCapabilityFilter, 'tool_call']
                    )}
                    className={cn(
                      "rounded px-2 py-0.5 text-xs font-medium",
                      modelCapabilityFilter.includes('tool_call')
                        ? "bg-accent-green/10 text-accent-green"
                        : "bg-zinc-100 text-zinc-600 hover:bg-zinc-200 dark:bg-zinc-700 dark:text-zinc-400"
                    )}
                  >
                    🔧 Tool Calling
                  </button>
                  <button
                    onClick={() => setModelCapabilityFilter(
                      modelCapabilityFilter.includes('reasoning')
                        ? modelCapabilityFilter.filter(f => f !== 'reasoning')
                        : [...modelCapabilityFilter, 'reasoning']
                    )}
                    className={cn(
                      "rounded px-2 py-0.5 text-xs font-medium",
                      modelCapabilityFilter.includes('reasoning')
                        ? "bg-purple-100 text-purple-700 dark:bg-purple-900 dark:text-purple-300"
                        : "bg-zinc-100 text-zinc-600 hover:bg-zinc-200 dark:bg-zinc-700 dark:text-zinc-400"
                    )}
                  >
                    🧠 Reasoning
                  </button>
                  <button
                    onClick={() => setModelCapabilityFilter(
                      modelCapabilityFilter.includes('image')
                        ? modelCapabilityFilter.filter(f => f !== 'image')
                        : [...modelCapabilityFilter, 'image']
                    )}
                    className={cn(
                      "rounded px-2 py-0.5 text-xs font-medium",
                      modelCapabilityFilter.includes('image')
                        ? "bg-sky-100 text-sky-700 dark:bg-sky-900 dark:text-sky-300"
                        : "bg-zinc-100 text-zinc-600 hover:bg-zinc-200 dark:bg-zinc-700 dark:text-zinc-400"
                    )}
                  >
                    🖼️ Image
                  </button>
                </div>

                {/* Selected models as tags */}
                {newModels.length > 0 && (
                  <div className="mb-1 flex flex-wrap gap-1">
                    {newModels.map((m) => (
                      <span key={m} className="inline-flex items-center gap-1 rounded bg-accent-green/10 px-2 py-0.5 text-xs text-accent-green">
                        {m}
                        <button onClick={() => setNewModels(newModels.filter((x) => x !== m))} className="text-accent-green/60 hover:text-accent-green">×</button>
                      </span>
                    ))}
                  </div>
                )}
                {/* Search and select models */}
                <input
                  type="text"
                  value={modelSearchTerm}
                  onChange={(e) => setModelSearchTerm(e.target.value)}
                  placeholder="Search models..."
                  className="w-full rounded-md border border-zinc-200 px-3 py-2 text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                />
                <div className="mt-1 max-h-40 overflow-y-auto rounded border border-zinc-200 dark:border-zinc-700">
                  {modelsLoading ? (
                    <div className="px-3 py-2 text-xs text-zinc-400">Loading models...</div>
                  ) : (
                    availableModels
                      .filter((m) => {
                        // Filter by search term
                        const matchesSearch = !modelSearchTerm ||
                          m.id.toLowerCase().includes(modelSearchTerm.toLowerCase()) ||
                          m.name.toLowerCase().includes(modelSearchTerm.toLowerCase());

                        // Filter by capabilities
                        const matchesCapabilities = modelCapabilityFilter.length === 0 ||
                          modelCapabilityFilter.every(filter => {
                            if (filter === 'tool_call') return m.tool_call === true;
                            if (filter === 'reasoning') return m.reasoning === true;
                            if (filter === 'image') return m.input_modalities?.includes('image') ?? false;
                            return true;
                          });

                        return matchesSearch && matchesCapabilities;
                      })
                      .map((m) => (
                        <label
                          key={m.id}
                          className="flex cursor-pointer items-center gap-2 px-3 py-1.5 text-xs hover:bg-zinc-50 dark:hover:bg-zinc-700"
                        >
                          <input
                            type="checkbox"
                            checked={newModels.includes(m.id)}
                            onChange={() => toggleModel(m.id, newModels, setNewModels)}
                            className="accent-[var(--color-accent)]"
                          />
                          <div className="flex flex-1 flex-col gap-0.5">
                            <span className="truncate">{m.name || m.id}</span>
                            <div className="flex gap-2 text-xs text-zinc-400">
                              {m.context_window && (
                                <span>{(m.context_window / 1000).toFixed(0)}K context</span>
                              )}
                              {m.max_tokens && (
                                <span>{(m.max_tokens / 1000).toFixed(1)}K max output</span>
                              )}
                              {m.reasoning && <span>🧠 reasoning</span>}
                              {m.tool_call && <span>🔧 tools</span>}
                              {m.input_modalities?.includes('image') && <span>🖼️ image</span>}
                            </div>
                          </div>
                        </label>
                      ))
                  )}
                  {!modelsLoading && availableModels.length === 0 && (
                    <div className="px-3 py-2 text-xs text-zinc-400">No models found. Select provider first.</div>
                  )}
                </div>
                {/* Manual model input */}
                <div className="mt-2 flex gap-1">
                  <input
                    type="text"
                    placeholder="Or type a custom model name..."
                    className="flex-1 rounded-md border border-zinc-200 px-3 py-2 text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                    onKeyDown={(e) => {
                      if (e.key === "Enter") {
                        const val = (e.target as HTMLInputElement).value.trim();
                        if (val && !newModels.includes(val)) {
                          setNewModels([...newModels, val]);
                          (e.target as HTMLInputElement).value = "";
                        }
                      }
                    }}
                  />
                </div>
              </div>

              {/* Model Capabilities */}
              {newModels.length > 0 && (() => {
                // Find the first selected model in availableModels to check if it has capabilities
                const primaryModel = newModels[0];
                const modelInfo = availableModels.find(m => m.id === primaryModel);
                const hasModelsDevData = !!(modelInfo && (modelInfo.context_window || modelInfo.max_tokens));
                // Auto-fill from models.dev data when available
                const autoContextWindow = modelInfo?.context_window?.toString() ?? "";
                const autoMaxOutputTokens = modelInfo?.max_tokens?.toString() ?? "";
                const autoSupportsToolCalling = modelInfo?.tool_call ?? true;
                // Use auto-filled values if available, otherwise use user input state
                const displayContextWindow = hasModelsDevData ? autoContextWindow : newContextWindow;
                const displayMaxOutputTokens = hasModelsDevData ? autoMaxOutputTokens : newMaxOutputTokens;
                const displaySupportsToolCalling = hasModelsDevData ? autoSupportsToolCalling : newSupportsToolCalling;
                return (
                  <div>
                    <label className="mb-1 block text-xs text-zinc-500">
                      Model Capabilities
                      {hasModelsDevData && <span className="ml-1 text-xs text-zinc-400">(from models.dev)</span>}
                      {!hasModelsDevData && <span className="ml-1 text-xs text-amber-500">(manual input required)</span>}
                    </label>
                    <div className="flex gap-2">
                      <div className="flex-1">
                        <label className="mb-0.5 block text-xs text-zinc-400">Context Window</label>
                        <input
                          type="number"
                          value={displayContextWindow}
                          onChange={(e) => setNewContextWindow(e.target.value)}
                          readOnly={hasModelsDevData}
                          placeholder="e.g. 128000"
                          className={cn(
                            "w-full rounded-md border border-zinc-200 px-3 py-2 text-xs",
                            hasModelsDevData
                              ? "bg-zinc-50 text-zinc-400 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-500"
                              : "dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200",
                          )}
                        />
                      </div>
                      <div className="flex-1">
                        <label className="mb-0.5 block text-xs text-zinc-400">Max Output Tokens</label>
                        <input
                          type="number"
                          value={displayMaxOutputTokens}
                          onChange={(e) => setNewMaxOutputTokens(e.target.value)}
                          readOnly={hasModelsDevData}
                          placeholder="e.g. 4096"
                          className={cn(
                            "w-full rounded-md border border-zinc-200 px-3 py-2 text-xs",
                            hasModelsDevData
                              ? "bg-zinc-50 text-zinc-400 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-500"
                              : "dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200",
                          )}
                        />
                      </div>
                    </div>
                    <div className="mt-1.5 flex items-center gap-2">
                      <label className="flex items-center gap-1.5 text-xs text-zinc-500">
                        <input
                          type="checkbox"
                          checked={displaySupportsToolCalling}
                          onChange={(e) => setNewSupportsToolCalling(e.target.checked)}
                          disabled={hasModelsDevData}
                          className="accent-[var(--color-accent)]"
                        />
                        Supports Tool Calling
                      </label>
                    </div>
                  </div>
                );
              })()}

              {/* Compact model for LLM summarization */}
              {newModels.length > 0 && (
                <div>
                  <label className="mb-1 block text-xs text-zinc-500">
                    Compact Model (Summarization)
                  </label>
                  <select
                    value={newCompactModel}
                    onChange={(e) => setNewCompactModel(e.target.value)}
                    className="w-full rounded-md border border-zinc-200 px-3 py-2 text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                  >
                    <option value="">Use current model (default)</option>
                    {newModels.map((m) => (
                      <option key={m} value={m}>{m}</option>
                    ))}
                  </select>
                </div>
              )}

              {/* Test result */}
              {testResult && (
                <div className={cn(
                  "rounded-md px-3 py-2 text-xs",
                  testResult.success
                    ? "bg-green-50 text-green-700 dark:bg-green-900/20 dark:text-green-400"
                    : "bg-red-50 text-red-700 dark:bg-red-900/20 dark:text-red-400"
                )}>
                  {testResult.message}
                </div>
              )}
            </div>

            <div className="mt-4 flex items-center justify-between gap-2">
              {/* Test result on the left */}
              <div className="flex-1 min-w-0">
                {testResult && (
                  <div className={cn(
                    "rounded-md px-3 py-1.5 text-xs truncate",
                    testResult.success
                      ? "bg-green-50 text-green-700 dark:bg-green-900/20 dark:text-green-400"
                      : "bg-red-50 text-red-700 dark:bg-red-900/20 dark:text-red-400"
                  )}>
                    {testResult.message}
                  </div>
                )}
                {testing && (
                  <div className="text-xs text-zinc-400">Testing...</div>
                )}
              </div>

              {/* Buttons on the right with equal width */}
              <div className="flex gap-2 shrink-0">
                <button
                  onClick={() => { setShowAddDialog(false); setNewModels([]); setTestResult(null); }}
                  className="rounded-md px-3 py-1.5 text-xs font-medium text-zinc-600 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-700"
                >
                  Cancel
                </button>
                <button
                  onClick={handleAdd}
                  disabled={(needsApiKey(newProvider) ? !newKey.trim() : false) || testing}
                  className="rounded-md bg-zinc-200 px-3 py-1.5 text-xs font-medium text-zinc-800 hover:bg-zinc-300 disabled:opacity-50 dark:bg-zinc-700 dark:hover:bg-zinc-600"
                >
                  {testing ? "Saving..." : "Save"}
                </button>
              </div>
            </div>
          </div>
        </div>
      )}

      {/* Edit key dialog */}
      {showEditDialog && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50">
          <div className="w-[440px] max-h-[85vh] overflow-y-auto rounded-lg bg-white p-6 shadow-xl dark:bg-zinc-800">
            <h3 className="mb-3 text-sm font-semibold">Edit: {showEditDialog}</h3>

            <div className="space-y-2">
              <div>
                <label className="mb-1 block text-xs text-zinc-500">API Key</label>
                <input
                  type="password"
                  value={editKey}
                  onChange={(e) => setEditKey(e.target.value)}
                  placeholder="Enter new API key..."
                  className="w-full rounded-md border border-zinc-200 px-3 py-2 text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                />
              </div>

              {(
                <div>
                  <label className="mb-1 block text-xs text-zinc-500">Base URL</label>
                  <input
                    type="text"
                    value={editBaseUrl}
                    onChange={(e) => setEditBaseUrl(e.target.value)}
                    placeholder="https://..."
                    className="w-full rounded-md border border-zinc-200 px-3 py-2 font-mono text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                  />
                </div>
              )}

              {/* Model selection */}
              <div>
                <label className="mb-1 block text-xs text-zinc-500">
                  Default Model {editModels.length > 0 && <span className="text-accent-green">({editModels.length} selected)</span>}
                </label>

                {/* Capability filters */}
                <div className="mb-2 flex gap-2">
                  <button
                    onClick={() => setEditModelCapabilityFilter(
                      editModelCapabilityFilter.includes('tool_call')
                        ? editModelCapabilityFilter.filter(f => f !== 'tool_call')
                        : [...editModelCapabilityFilter, 'tool_call']
                    )}
                    className={cn(
                      "rounded px-2 py-0.5 text-xs font-medium",
                      editModelCapabilityFilter.includes('tool_call')
                        ? "bg-accent-green/10 text-accent-green"
                        : "bg-zinc-100 text-zinc-600 hover:bg-zinc-200 dark:bg-zinc-700 dark:text-zinc-400"
                    )}
                  >
                    🔧 Tool Calling
                  </button>
                  <button
                    onClick={() => setEditModelCapabilityFilter(
                      editModelCapabilityFilter.includes('reasoning')
                        ? editModelCapabilityFilter.filter(f => f !== 'reasoning')
                        : [...editModelCapabilityFilter, 'reasoning']
                    )}
                    className={cn(
                      "rounded px-2 py-0.5 text-xs font-medium",
                      editModelCapabilityFilter.includes('reasoning')
                        ? "bg-purple-100 text-purple-700 dark:bg-purple-900 dark:text-purple-300"
                        : "bg-zinc-100 text-zinc-600 hover:bg-zinc-200 dark:bg-zinc-700 dark:text-zinc-400"
                    )}
                  >
                    🧠 Reasoning
                  </button>
                  <button
                    onClick={() => setEditModelCapabilityFilter(
                      editModelCapabilityFilter.includes('image')
                        ? editModelCapabilityFilter.filter(f => f !== 'image')
                        : [...editModelCapabilityFilter, 'image']
                    )}
                    className={cn(
                      "rounded px-2 py-0.5 text-xs font-medium",
                      editModelCapabilityFilter.includes('image')
                        ? "bg-sky-100 text-sky-700 dark:bg-sky-900 dark:text-sky-300"
                        : "bg-zinc-100 text-zinc-600 hover:bg-zinc-200 dark:bg-zinc-700 dark:text-zinc-400"
                    )}
                  >
                    🖼️ Image
                  </button>
                </div>

                {editModels.length > 0 && (
                  <div className="mb-1 flex flex-wrap gap-1">
                    {editModels.map((m) => (
                      <span key={m} className="inline-flex items-center gap-1 rounded bg-accent-green/10 px-2 py-0.5 text-xs text-accent-green">
                        {m}
                        <button onClick={() => setEditModels(editModels.filter((x) => x !== m))} className="text-accent-green/60 hover:text-accent-green">×</button>
                      </span>
                    ))}
                  </div>
                )}
                <input
                  type="text"
                  value={editModelSearchTerm}
                  onChange={(e) => setEditModelSearchTerm(e.target.value)}
                  placeholder="Search models..."
                  className="w-full rounded-md border border-zinc-200 px-3 py-2 text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                />
                <div className="mt-1 max-h-40 overflow-y-auto rounded border border-zinc-200 dark:border-zinc-700">
                  {editModelsLoading ? (
                    <div className="px-3 py-2 text-xs text-zinc-400">Loading models...</div>
                  ) : (
                    editAvailableModels
                      .filter((m) => {
                        // Filter by search term
                        const matchesSearch = !editModelSearchTerm ||
                          m.id.toLowerCase().includes(editModelSearchTerm.toLowerCase()) ||
                          m.name.toLowerCase().includes(editModelSearchTerm.toLowerCase());

                        // Filter by capabilities
                        const matchesCapabilities = editModelCapabilityFilter.length === 0 ||
                          editModelCapabilityFilter.every(filter => {
                            if (filter === 'tool_call') return m.tool_call === true;
                            if (filter === 'reasoning') return m.reasoning === true;
                            if (filter === 'image') return m.input_modalities?.includes('image') ?? false;
                            return true;
                          });

                        return matchesSearch && matchesCapabilities;
                      })
                      .map((m) => (
                        <label
                          key={m.id}
                          className="flex cursor-pointer items-center gap-2 px-3 py-1.5 text-xs hover:bg-zinc-50 dark:hover:bg-zinc-700"
                        >
                          <input
                            type="checkbox"
                            checked={editModels.includes(m.id)}
                            onChange={() => toggleModel(m.id, editModels, setEditModels)}
                            className="accent-[var(--color-accent)]"
                          />
                          <div className="flex flex-1 flex-col gap-0.5">
                            <span className="truncate">{m.name || m.id}</span>
                            <div className="flex gap-2 text-xs text-zinc-400">
                              {m.context_window && (
                                <span>{(m.context_window / 1000).toFixed(0)}K context</span>
                              )}
                              {m.max_tokens && (
                                <span>{(m.max_tokens / 1000).toFixed(1)}K max output</span>
                              )}
                              {m.reasoning && <span>🧠 reasoning</span>}
                              {m.tool_call && <span>🔧 tools</span>}
                              {m.input_modalities?.includes('image') && <span>🖼️ image</span>}
                            </div>
                          </div>
                        </label>
                      ))
                  )}
                </div>
                <div className="mt-2 flex gap-1">
                  <input
                    type="text"
                    placeholder="Or type a custom model name..."
                    className="flex-1 rounded-md border border-zinc-200 px-3 py-2 text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                    onKeyDown={(e) => {
                      if (e.key === "Enter") {
                        const val = (e.target as HTMLInputElement).value.trim();
                        if (val && !editModels.includes(val)) {
                          setEditModels([...editModels, val]);
                          (e.target as HTMLInputElement).value = "";
                        }
                      }
                    }}
                  />
                </div>
              </div>

              {/* Model Capabilities */}
              {editModels.length > 0 && (() => {
                const primaryModel = editModels[0];
                const modelInfo = editAvailableModels.find(m => m.id === primaryModel);
                const hasModelsDevData = !!(modelInfo && (modelInfo.context_window || modelInfo.max_tokens));
                const autoContextWindow = modelInfo?.context_window?.toString() ?? "";
                const autoMaxOutputTokens = modelInfo?.max_tokens?.toString() ?? "";
                const autoSupportsToolCalling = modelInfo?.tool_call ?? true;
                const displayContextWindow = hasModelsDevData ? autoContextWindow : editContextWindow;
                const displayMaxOutputTokens = hasModelsDevData ? autoMaxOutputTokens : editMaxOutputTokens;
                const displaySupportsToolCalling = hasModelsDevData ? autoSupportsToolCalling : editSupportsToolCalling;
                return (
                  <div>
                    <label className="mb-1 block text-xs text-zinc-500">
                      Model Capabilities
                      {hasModelsDevData && <span className="ml-1 text-xs text-zinc-400">(from models.dev)</span>}
                      {!hasModelsDevData && <span className="ml-1 text-xs text-amber-500">(manual input required)</span>}
                    </label>
                    <div className="flex gap-2">
                      <div className="flex-1">
                        <label className="mb-0.5 block text-xs text-zinc-400">Context Window</label>
                        <input
                          type="number"
                          value={displayContextWindow}
                          onChange={(e) => setEditContextWindow(e.target.value)}
                          readOnly={hasModelsDevData}
                          placeholder="e.g. 128000"
                          className={cn(
                            "w-full rounded-md border border-zinc-200 px-3 py-2 text-xs",
                            hasModelsDevData
                              ? "bg-zinc-50 text-zinc-400 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-500"
                              : "dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200",
                          )}
                        />
                      </div>
                      <div className="flex-1">
                        <label className="mb-0.5 block text-xs text-zinc-400">Max Output Tokens</label>
                        <input
                          type="number"
                          value={displayMaxOutputTokens}
                          onChange={(e) => setEditMaxOutputTokens(e.target.value)}
                          readOnly={hasModelsDevData}
                          placeholder="e.g. 4096"
                          className={cn(
                            "w-full rounded-md border border-zinc-200 px-3 py-2 text-xs",
                            hasModelsDevData
                              ? "bg-zinc-50 text-zinc-400 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-500"
                              : "dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200",
                          )}
                        />
                      </div>
                    </div>
                    <div className="mt-1.5 flex items-center gap-2">
                      <label className="flex items-center gap-1.5 text-xs text-zinc-500">
                        <input
                          type="checkbox"
                          checked={displaySupportsToolCalling}
                          onChange={(e) => setEditSupportsToolCalling(e.target.checked)}
                          disabled={hasModelsDevData}
                          className="accent-[var(--color-accent)]"
                        />
                        Supports Tool Calling
                      </label>
                    </div>
                  </div>
                );
              })()}

              {/* Compact model for LLM summarization */}
              {editModels.length > 0 && (
                <div>
                  <label className="mb-1 block text-xs text-zinc-500">
                    Compact Model (Summarization)
                  </label>
                  <select
                    value={editCompactModel}
                    onChange={(e) => setEditCompactModel(e.target.value)}
                    className="w-full rounded-md border border-zinc-200 px-3 py-2 text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                  >
                    <option value="">Use current model (default)</option>
                    {editModels.map((m) => (
                      <option key={m} value={m}>{m}</option>
                    ))}
                  </select>
                </div>
              )}
            </div>

            <div className="mt-4 flex items-center justify-end gap-2">
              {/* Buttons with equal width */}
              <button
                onClick={() => setShowEditDialog(null)}
                className="w-20 rounded-md px-3 py-1.5 text-xs font-medium text-center text-zinc-600 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-700"
              >
                Cancel
              </button>
              <button
                onClick={handleEditSave}
                className="w-20 rounded-md bg-zinc-200 px-3 py-1.5 text-xs font-medium text-center text-zinc-800 hover:bg-zinc-300 disabled:opacity-50 dark:bg-zinc-700 dark:hover:bg-zinc-600"
              >
                Save
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

/** MCP tab — placeholder, content TBD */
function McpTab() {
  const { catalog, loading, error, loadCatalog, addServer, removeServer } = useMcpStore();
  const [showAddForm, setShowAddForm] = useState(false);

  // New server form state
  const [newName, setNewName] = useState("");
  const [newTransport, setNewTransport] = useState<McpTransportDef>("stdio");
  const [newCommand, setNewCommand] = useState("");
  const [newArgs, setNewArgs] = useState("");
  const [newUrl, setNewUrl] = useState("");
  const [newEnv, setNewEnv] = useState("");

  // Preset env var form (for servers requiring API keys)
  const [presetEnvForm, setPresetEnvForm] = useState<Record<string, string>>({});
  const [activePreset, setActivePreset] = useState<McpPresetDef | null>(null);

  useEffect(() => {
    loadCatalog();
  }, [loadCatalog]);

  const catalogNames = useMemo(() => new Set(catalog.map((s) => s.name)), [catalog]);

  const handleAddFromPreset = (preset: McpPresetDef) => {
    if (preset.requiredEnv.length > 0) {
      // Show env form for API keys
      setActivePreset(preset);
      setPresetEnvForm(
        preset.requiredEnv.reduce((acc, key) => ({ ...acc, [key]: "" }), {})
      );
    } else {
      // No API key needed, add directly
      const config = presetToServerConfig(preset);
      addServer(config);
    }
  };

  const handlePresetEnvSubmit = () => {
    if (!activePreset) return;
    const config = presetToServerConfig(activePreset, presetEnvForm);
    addServer(config);
    setActivePreset(null);
    setPresetEnvForm({});
  };

  const handleAddManual = () => {
    if (!newName.trim()) return;
    const config: McpServerConfigDef = {
      name: newName.trim(),
      transport: newTransport,
      command: newCommand.trim(),
      args: newArgs.trim() ? newArgs.trim().split(/\s+/) : [],
      url: newUrl.trim() || undefined,
      env: newEnv.trim()
        ? Object.fromEntries(
          newEnv.split(",").map((pair) => {
            const [k, ...v] = pair.split("=");
            return [k.trim(), v.join("=").trim()];
          })
        )
        : {},
    };
    addServer(config);
    setShowAddForm(false);
    setNewName("");
    setNewCommand("");
    setNewArgs("");
    setNewUrl("");
    setNewEnv("");
  };

  return (
    <div className="max-w-2xl space-y-4">
      {/* Catalog servers */}
      <div className="rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <div className="flex items-center justify-between">
          <h2 className="text-xs font-medium">MCP Server Catalog</h2>
          <button
            onClick={() => setShowAddForm(true)}
            className="rounded-md bg-zinc-800 px-3 py-1 text-xs font-medium text-white hover:bg-zinc-700 dark:bg-zinc-700 dark:hover:bg-zinc-600"
          >
            Add Server
          </button>
        </div>

        {error && (
          <p className="mt-2 text-xs text-red-500">{error}</p>
        )}

        {loading && catalog.length === 0 && (
          <p className="mt-3 text-xs text-zinc-400">Loading catalog...</p>
        )}

        {!loading && catalog.length === 0 && (
          <p className="mt-3 text-xs text-zinc-400">
            No MCP servers configured yet.
          </p>
        )}

        {/* Server list */}
        {catalog.length > 0 && (
          <div className="mt-3 space-y-2">
            {catalog.map((server) => (
              <div
                key={server.name}
                className="flex items-center justify-between rounded border border-zinc-100 px-3 py-2 dark:border-zinc-600"
              >
                <div className="flex items-center gap-2">
                  <span className="rounded bg-zinc-100 px-1.5 py-0.5 text-[10px] font-mono text-zinc-500 dark:bg-zinc-700">
                    {server.transport}
                  </span>
                  <span className="text-xs font-medium">{server.name}</span>
                  {server.has_secrets && (
                    <span className="text-[10px] text-amber-500">has API key</span>
                  )}
                </div>
                <div className="flex items-center gap-2">
                  <span className="text-[10px] text-zinc-400">
                    {server.command || server.url || ""}
                  </span>
                  <button
                    onClick={() => removeServer(server.name)}
                    className="text-xs text-zinc-400 hover:text-red-500"
                  >
                    Remove
                  </button>
                </div>
              </div>
            ))}
          </div>
        )}
      </div>

      {/* Presets gallery — always visible */}
      <div className="rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <h2 className="text-xs font-medium mb-3">Recommended MCP Servers</h2>
        <div className="grid grid-cols-2 gap-2">
          {MCP_PRESETS.map((preset) => {
            const isInstalled = catalogNames.has(preset.id);
            return (
              <div
                key={preset.id}
                className="rounded border border-zinc-100 p-2 dark:border-zinc-600"
              >
                <div className="flex items-start justify-between">
                  <div>
                    <span className="text-xs font-medium">{preset.name}</span>
                    <span className="ml-1.5 rounded bg-zinc-100 px-1 py-0.5 text-[10px] text-zinc-400 dark:bg-zinc-700">
                      {preset.category}
                    </span>
                  </div>
                  {isInstalled ? (
                    <span className="text-[10px] text-green-500">Installed</span>
                  ) : (
                    <button
                      onClick={() => handleAddFromPreset(preset)}
                      className="rounded-md bg-zinc-800 px-2 py-0.5 text-[10px] font-medium text-white hover:bg-zinc-700 dark:bg-zinc-700 dark:hover:bg-zinc-600"
                    >
                      Add
                    </button>
                  )}
                </div>
                <p className="mt-1 text-[10px] text-zinc-400 line-clamp-2">
                  {preset.description}
                </p>
                {preset.requiredEnv.length > 0 && !isInstalled && (
                  <p className="mt-1 text-[10px] text-amber-500">
                    Requires: {preset.requiredEnv.join(", ")}
                  </p>
                )}
              </div>
            );
          })}
        </div>
      </div>

      {/* Add Server dialog */}
      {showAddForm && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50">
          <div className="w-[440px] max-h-[85vh] overflow-y-auto rounded-lg bg-white p-6 shadow-xl dark:bg-zinc-800">
            <h3 className="mb-3 text-sm font-semibold">Add Custom MCP Server</h3>
            <div className="space-y-2">
              <div>
                <label className="mb-1 block text-xs text-zinc-500">Name</label>
                <input
                  value={newName}
                  onChange={(e) => setNewName(e.target.value)}
                  className={inputBase}
                  placeholder="my-server"
                />
              </div>
              <div>
                <label className="mb-1 block text-xs text-zinc-500">Transport</label>
                <select
                  value={newTransport}
                  onChange={(e) => setNewTransport(e.target.value as McpTransportDef)}
                  className={selectBase}
                >
                  <option value="stdio">stdio</option>
                  <option value="http">http</option>
                  <option value="sse">sse</option>
                </select>
              </div>
              {newTransport === "stdio" ? (
                <>
                  <div>
                    <label className="mb-1 block text-xs text-zinc-500">Command</label>
                    <input
                      value={newCommand}
                      onChange={(e) => setNewCommand(e.target.value)}
                      className={inputBase}
                      placeholder="npx"
                    />
                  </div>
                  <div>
                    <label className="mb-1 block text-xs text-zinc-500">Arguments (space-separated)</label>
                    <input
                      value={newArgs}
                      onChange={(e) => setNewArgs(e.target.value)}
                      className={inputBase}
                      placeholder="-y @modelcontextprotocol/server-filesystem"
                    />
                  </div>
                </>
              ) : (
                <div>
                  <label className="mb-1 block text-xs text-zinc-500">URL</label>
                  <input
                    value={newUrl}
                    onChange={(e) => setNewUrl(e.target.value)}
                    className={inputBase}
                    placeholder="http://localhost:3000"
                  />
                </div>
              )}
              <div>
                <label className="mb-1 block text-xs text-zinc-500">Environment (KEY=VALUE, comma-separated)</label>
                <input
                  value={newEnv}
                  onChange={(e) => setNewEnv(e.target.value)}
                  className={inputBase}
                  placeholder="API_KEY=sk-xxx, DEBUG=true"
                />
              </div>
            </div>
            <div className="mt-4 flex justify-end gap-2">
              <button
                onClick={() => { setShowAddForm(false); }}
                className="rounded-md px-3 py-1.5 text-xs font-medium text-zinc-600 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-700"
              >
                Cancel
              </button>
              <button
                onClick={handleAddManual}
                disabled={!newName.trim()}
                className="rounded-md bg-zinc-800 px-3 py-1.5 text-xs font-medium text-white hover:bg-zinc-700 disabled:opacity-50 dark:bg-zinc-700 dark:hover:bg-zinc-600"
              >
                Add Server
              </button>
            </div>
          </div>
        </div>
      )}

      {/* Preset env form (for servers requiring API keys) */}
      {activePreset && (
        <div className="rounded-lg border border-amber-200 bg-white p-4 dark:border-amber-700 dark:bg-zinc-800">
          <h2 className="text-xs font-medium mb-1">Configure {activePreset.name}</h2>
          <p className="text-[10px] text-zinc-400 mb-3">{activePreset.installHint}</p>
          <div className="space-y-2">
            {activePreset.requiredEnv.map((envKey) => (
              <div key={envKey}>
                <label className="text-[10px] text-zinc-400">{envKey}</label>
                <input
                  type="password"
                  value={presetEnvForm[envKey] || ""}
                  onChange={(e) =>
                    setPresetEnvForm((prev) => ({ ...prev, [envKey]: e.target.value }))
                  }
                  className="w-full rounded border border-zinc-200 px-2 py-1 text-xs dark:border-zinc-600 dark:bg-zinc-700"
                  placeholder={`Enter ${envKey}`}
                />
              </div>
            ))}
            <div className="flex gap-2">
              <button
                onClick={handlePresetEnvSubmit}
                className="rounded px-3 py-1 text-xs bg-accent text-white hover:opacity-90"
              >
                Add Server
              </button>
              <button
                onClick={() => { setActivePreset(null); setPresetEnvForm({}); }}
                className="rounded px-3 py-1 text-xs text-zinc-500 hover:bg-zinc-100 dark:hover:bg-zinc-700"
              >
                Cancel
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

/** GatewayConfig type for local usage */
interface GatewayConfig {
  default_provider?: string;
  default_model?: string;
}