import { useState, useEffect, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useGatewayStore } from "../../stores/gatewayStore";
import { useSettingsStore } from "../../stores/settingsStore";
import type { GatewayConfig, VaultKeyEntry, ModelInfo, ModelCapabilitiesInfo, ProviderListEntry } from "../../lib/types";
import { cn } from "../../lib/utils";
import { needsApiKey, keyPlaceholder } from "../../lib/providers";
import { fetchProviderModels } from "../../lib/gateway-api";
import { DEFAULT_GATEWAY_URL, getGatewayUrl } from "../../lib/config";
import { Star } from "lucide-react";
import { inputReadonly, selectBase } from "../../lib/ui-styles";
import { ProfileTab } from "./ProfileTab";

type SettingsTab = "gateway" | "providers" | "appearance" | "general" | "profile";

export function SettingsPage({ initialTab = "gateway" }: { initialTab?: SettingsTab }) {
  const [activeTab, setActiveTab] = useState<SettingsTab>(initialTab);

  const tabs: { id: SettingsTab; label: string }[] = [
    { id: "gateway", label: "Gateway" },
    { id: "providers", label: "Providers" },
    { id: "appearance", label: "Appearance" },
    { id: "general", label: "General" },
    { id: "profile", label: "My Profile" },
  ];

  return (
    <div
      className="flex flex-1 flex-col bg-white dark:bg-zinc-900"
    >
      {/* Header */}
      <div className="border-b border-zinc-200 px-6 py-4 dark:border-zinc-800">
        <h1 className="text-xl font-semibold">Settings</h1>
      </div>

      {/* Tabs */}
      <div className="flex gap-1 border-b border-zinc-200 px-6 dark:border-zinc-800">
        {tabs.map((tab) => (
          <button
            key={tab.id}
            onClick={() => setActiveTab(tab.id)}
            className={cn(
              "border-b-2 px-3 py-2 text-sm font-medium transition-colors",
              activeTab === tab.id
                ? "border-accent-green text-accent-green dark:border-accent-green dark:text-accent-green"
                : "border-transparent text-zinc-500 hover:text-zinc-700 dark:text-zinc-400 dark:hover:text-zinc-300",
            )}
          >
            {tab.label}
          </button>
        ))}
      </div>

      {/* Tab content */}
      <div className="flex-1 overflow-y-auto p-6">
        {activeTab === "gateway" && <GatewayTab />}
        {activeTab === "providers" && <ProvidersTab />}
        {activeTab === "appearance" && <AppearanceTab />}
        {activeTab === "general" && <GeneralTab />}
        {activeTab === "profile" && <ProfileTab />}
      </div>
    </div>
  );
}

/** Gateway connection settings */
function GatewayTab() {
  const { status, health, checkHealth } = useGatewayStore();
  const [testing, setTesting] = useState(false);

  const handleTest = useCallback(async () => {
    setTesting(true);
    await checkHealth();
    setTesting(false);
  }, [checkHealth]);

  return (
    <div className="max-w-lg space-y-4">
      <h2 className="text-sm font-medium">Gateway Connection</h2>

      <div className="space-y-3">
        <div>
          <label className="mb-1 block text-xs text-zinc-500">Address</label>
          <input
            type="text"
            value={DEFAULT_GATEWAY_URL}
            readOnly
            className={`w-full ${inputReadonly}`}
          />
        </div>

        <div className="flex items-center gap-2 text-sm">
          <span className="text-zinc-500">Status</span>
          <span
            className={cn(
              "h-2 w-2 rounded-full",
              status === "connected" ? "bg-green-500" : status === "error" ? "bg-red-500" : "bg-zinc-400",
            )}
          />
          <span className={cn(
            status === "connected" ? "text-green-600 dark:text-green-400" :
            status === "error" ? "text-red-600 dark:text-red-400" :
            "text-zinc-500"
          )}>
            {status === "connected" ? "Connected" : status === "error" ? "Error" : "Disconnected"}
          </span>
        </div>

        {health && (
          <>
            <div className="flex items-center gap-2 text-sm">
              <span className="text-zinc-500">Version</span>
              <span>{health.version}</span>
            </div>
          </>
        )}

        <button
          onClick={handleTest}
          disabled={testing}
          className="rounded-md btn-solid px-3 py-1.5 text-xs font-medium disabled:opacity-50"
        >
          {testing ? "Testing..." : "Test Connection"}
        </button>
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
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<{ success: boolean; message: string } | null>(null);

  // Edit dialog state
  const [editKey, setEditKey] = useState("");
  const [editBaseUrl, setEditBaseUrl] = useState("");
  const [editModels, setEditModels] = useState<string[]>([]);
  const [editAvailableModels, setEditAvailableModels] = useState<ModelInfo[]>([]);
  const [editModelsLoading, setEditModelsLoading] = useState(false);
  const [editModelSearchTerm, setEditModelSearchTerm] = useState("");

  // Edit dialog — model capabilities state
  const [editContextWindow, setEditContextWindow] = useState("");
  const [editMaxOutputTokens, setEditMaxOutputTokens] = useState("");
  const [editSupportsToolCalling, setEditSupportsToolCalling] = useState(true);

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
      const result = await invoke<GatewayConfig>("get_config");
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
    
    // Rust requires context_window to be present (u64, not Option)
    // Default to 128000 if not specified (safe default for most models)
    const ctxWindow = effectiveContextWindow ? parseInt(effectiveContextWindow) : 128000;
    
    // Build model_capabilities if user selected models
    let modelCapabilities: ModelCapabilitiesInfo | undefined;
    if (newModels.length > 0) {
      const maxOutTokens = effectiveMaxOutputTokens ? parseInt(effectiveMaxOutputTokens) : 0;
      modelCapabilities = {
        context_window: ctxWindow,
        max_output_tokens: maxOutTokens,
        supports_tool_calling: effectiveSupportsToolCalling,
      };
    }
    try {
      await invoke("add_key", {
        provider: newProvider,
        key: newKey,
        baseUrl: newBaseUrl || undefined,
        defaultModel: undefined,
        models: newModels.length > 0 ? newModels : undefined,
        modelCapabilities,
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
      await invoke("update_config", {
        defaultProvider: provider,
        defaultModel: entry?.models?.[0] || entry?.default_model || undefined,
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
    // Load existing capabilities from VaultKeyEntry
    setEditContextWindow(keyEntry?.model_capabilities?.context_window?.toString() ?? "");
    setEditMaxOutputTokens(keyEntry?.model_capabilities?.max_output_tokens?.toString() ?? "");
    setEditSupportsToolCalling(keyEntry?.model_capabilities?.supports_tool_calling ?? true);
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
      // Build model_capabilities if user provided values
      if (editContextWindow || editMaxOutputTokens) {
        const cw = Number(editContextWindow);
        const mot = Number(editMaxOutputTokens);
        if ((editContextWindow && (!Number.isFinite(cw) || cw <= 0)) ||
            (editMaxOutputTokens && (!Number.isFinite(mot) || mot <= 0))) {
          alert('Context Window and Max Output Tokens must be positive numbers');
          return;
        }
        updatePayload.modelCapabilities = {
          context_window: cw || 0,
          max_output_tokens: mot || 0,
          supports_tool_calling: editSupportsToolCalling,
        };
      }
      console.log("[handleEditSave] payload:", JSON.stringify(updatePayload));
      await invoke("update_key", updatePayload);
      console.log("[handleEditSave] success");
      setShowEditDialog(null);
      await fetchKeys();
      await fetchConfig();
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
      <div className="flex items-center justify-between">
        <h2 className="text-sm font-medium">Provider Management</h2>
      </div>

        <div className="space-y-3">
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
                    <div key={keyEntry.provider} className="rounded-lg border border-zinc-200 p-1.5 dark:border-zinc-700">
                      <div className="flex items-center justify-between gap-2">
                        <div className="flex items-center flex-nowrap gap-2">
                          <span className="shrink-0 text-sm font-medium">{providerName}</span>
                          {keyEntry.models?.length ? (
                            <span className="shrink-0 text-xs text-accent-green">{keyEntry.models.join(", ")}</span>
                          ) : keyEntry.default_model ? (
                            <span className="shrink-0 text-xs text-accent-green">{keyEntry.default_model}</span>
                          ) : (
                            <span className="shrink-0 text-xs text-zinc-400">—</span>
                          )}
                        </div>
                        <div className="flex items-center gap-2">
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
                          <span className="text-xs text-green-600 dark:text-green-400">Active</span>
                          <span className="text-xs text-zinc-400">Key: {keyEntry.key_preview}</span>
                          <button
                            onClick={() => handleEdit(keyEntry.provider)}
                            className="text-xs text-accent-green hover:text-accent-green/70"
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
                    </div>
                  );
                })}
              </div>
            </div>
          )}

          {/* Divider */}
          {keys.length > 0 && (
            <div className="border-t border-zinc-200 dark:border-zinc-700" />
          )}

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
                  className="rounded px-2 py-0.5 text-[10px] text-zinc-500 hover:text-red-600 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:text-red-400 dark:hover:bg-zinc-800"
                  title="Clear cache and refresh"
                >
                  🗑 Clear Cache
                </button>
                <button
                  onClick={() => fetchDynamicProviders(false)}
                  disabled={dynamicProvidersLoading}
                  className="rounded px-2 py-0.5 text-[10px] text-zinc-500 hover:text-zinc-700 hover:bg-zinc-100 disabled:opacity-50 dark:text-zinc-400 dark:hover:text-zinc-300 dark:hover:bg-zinc-800"
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
                    <div key={providerId} className="rounded-lg border border-zinc-200 p-1.5 dark:border-zinc-700">
                      <div className="flex items-center justify-between">
                        <div className="min-w-0 flex-1">
                          <span className="text-sm font-medium">{providerName}</span>
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
                          className="rounded-md bg-zinc-100 px-2 py-1 text-xs font-medium text-zinc-700 hover:bg-zinc-200 dark:bg-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-600"
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
                      "rounded px-2 py-0.5 text-[10px] font-medium",
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
                      "rounded px-2 py-0.5 text-[10px] font-medium",
                      modelCapabilityFilter.includes('reasoning')
                        ? "bg-purple-100 text-purple-700 dark:bg-purple-900 dark:text-purple-300"
                        : "bg-zinc-100 text-zinc-600 hover:bg-zinc-200 dark:bg-zinc-700 dark:text-zinc-400"
                    )}
                  >
                    🧠 Reasoning
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
                            className="accent-accent-green"
                          />
                          <div className="flex flex-1 flex-col gap-0.5">
                            <span className="truncate">{m.name || m.id}</span>
                            <div className="flex gap-2 text-[10px] text-zinc-400">
                              {m.context_window && (
                                <span>{(m.context_window / 1000).toFixed(0)}K context</span>
                              )}
                              {m.max_tokens && (
                                <span>{(m.max_tokens / 1000).toFixed(1)}K max output</span>
                              )}
                              {m.reasoning && <span>🧠 reasoning</span>}
                              {m.tool_call && <span>🔧 tools</span>}
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
                      {hasModelsDevData && <span className="ml-1 text-[10px] text-zinc-400">(from models.dev)</span>}
                      {!hasModelsDevData && <span className="ml-1 text-[10px] text-amber-500">(manual input required)</span>}
                    </label>
                    <div className="flex gap-2">
                      <div className="flex-1">
                        <label className="mb-0.5 block text-[10px] text-zinc-400">Context Window</label>
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
                        <label className="mb-0.5 block text-[10px] text-zinc-400">Max Output Tokens</label>
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
                          className="accent-accent-green"
                        />
                        Supports Tool Calling
                      </label>
                    </div>
                  </div>
                );
              })()}



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
                  className="w-20 rounded-md px-3 py-1.5 text-xs font-medium text-center text-zinc-600 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-700"
                >
                  Cancel
                </button>
                <button
                  onClick={handleAdd}
                  disabled={(needsApiKey(newProvider) ? !newKey.trim() : false) || testing}
                  className="w-20 rounded-md bg-zinc-200 px-3 py-1.5 text-xs font-medium text-center text-zinc-800 hover:bg-zinc-300 disabled:opacity-50 dark:bg-zinc-700 dark:hover:bg-zinc-600"
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
                      .filter((m) =>
                        !editModelSearchTerm ||
                        m.id.toLowerCase().includes(editModelSearchTerm.toLowerCase()) ||
                        m.name.toLowerCase().includes(editModelSearchTerm.toLowerCase())
                      )
                      .map((m) => (
                        <label
                          key={m.id}
                          className="flex cursor-pointer items-center gap-2 px-3 py-1.5 text-xs hover:bg-zinc-50 dark:hover:bg-zinc-700"
                        >
                          <input
                            type="checkbox"
                            checked={editModels.includes(m.id)}
                            onChange={() => toggleModel(m.id, editModels, setEditModels)}
                            className="accent-accent-green"
                          />
                          <div className="flex flex-1 flex-col gap-0.5">
                            <span className="truncate">{m.name || m.id}</span>
                            <div className="flex gap-2 text-[10px] text-zinc-400">
                              {m.context_window && (
                                <span>{(m.context_window / 1000).toFixed(0)}K context</span>
                              )}
                              {m.max_tokens && (
                                <span>{(m.max_tokens / 1000).toFixed(1)}K max output</span>
                              )}
                              {m.reasoning && <span>🧠 reasoning</span>}
                              {m.tool_call && <span>🔧 tools</span>}
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
                      {hasModelsDevData && <span className="ml-1 text-[10px] text-zinc-400">(from models.dev)</span>}
                      {!hasModelsDevData && <span className="ml-1 text-[10px] text-amber-500">(manual input required)</span>}
                    </label>
                    <div className="flex gap-2">
                      <div className="flex-1">
                        <label className="mb-0.5 block text-[10px] text-zinc-400">Context Window</label>
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
                        <label className="mb-0.5 block text-[10px] text-zinc-400">Max Output Tokens</label>
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
                          className="accent-accent-green"
                        />
                        Supports Tool Calling
                      </label>
                    </div>
                  </div>
                );
              })()}
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

/** Appearance settings */
function AppearanceTab() {
  const { theme, setTheme, fontSize, setFontSize, contentWidth, setContentWidth, opacity, setOpacity, accentColor, setAccentColor } = useSettingsStore();

  // Content width options: 40-100%, step 10
  const contentWidths = [
    { label: "40%", value: 40 },
    { label: "50%", value: 50 },
    { label: "60%", value: 60 },
    { label: "70%", value: 70 },
    { label: "80%", value: 80 },
    { label: "90%", value: 90 },
    { label: "100%", value: 100 },
  ];

  // Font size options: M = previous default (text-sm = 0.875rem)
  const fontSizes = [
    { label: "S", value: 0.75 },
    { label: "M", value: 0.875 },
    { label: "L", value: 1.0 },
    { label: "XL", value: 1.125 },
    { label: "XXL", value: 1.25 },
  ];

  return (
    <div className="max-w-lg space-y-6">
      <div>
        <h2 className="mb-3 text-sm font-medium">Theme</h2>
        <div className="flex gap-4">
          {(["light", "dark", "system"] as const).map((t) => (
            <label key={t} className="flex items-center gap-2 text-sm">
              <input
                type="radio"
                name="theme"
                value={t}
                checked={theme === t}
                onChange={() => setTheme(t)}
                className="accent-accent-green"
              />
              {t.charAt(0).toUpperCase() + t.slice(1)}
            </label>
          ))}
        </div>
      </div>

      <div>
        <h2 className="mb-3 text-sm font-medium">Accent Color</h2>
        <p className="mb-2 text-xs text-zinc-500">全局高亮色</p>
        <div className="flex gap-2">
          {[
            { label: "Green", value: "#00C375" },
            { label: "Blue", value: "#3b82f6" },
            { label: "Violet", value: "#8b5cf6" },
            { label: "Cyan", value: "#06b6d4" },
            { label: "Rose", value: "#f43f5e" },
          ].map((c) => (
            <button
              key={c.value}
              onClick={() => setAccentColor(c.value)}
              className={cn(
                "flex h-9 w-9 items-center justify-center rounded-full transition-transform",
                accentColor === c.value
                  ? "scale-110 ring-2 ring-offset-2 ring-offset-white dark:ring-offset-zinc-900"
                  : "hover:scale-105",
              )}
              style={{
                backgroundColor: c.value,
                "--tw-ring-color": c.value,
              } as React.CSSProperties}
              title={c.label}
            />
          ))}
        </div>
      </div>

      <div>
        <h2 className="mb-3 text-sm font-medium">Content Width</h2>
        <p className="mb-2 text-xs text-zinc-500">聊天消息、工具调用、Thinking 的最大显示宽度</p>
        <div className="flex gap-1">
          {contentWidths.map((cw) => (
            <button
              key={cw.label}
              onClick={() => setContentWidth(cw.value)}
              className={cn(
                "rounded-md border px-3 py-1.5 text-xs font-medium transition-colors",
                contentWidth === cw.value
                  ? "border-accent-green bg-accent-green/10 text-accent-green dark:border-accent-green dark:bg-accent-green/20 dark:text-accent-green"
                  : "border-zinc-200 text-zinc-600 hover:bg-zinc-50 dark:border-zinc-700 dark:text-zinc-400 dark:hover:bg-zinc-800",
              )}
            >
              {cw.label}
            </button>
          ))}
        </div>
      </div>

      <div>
        <h2 className="mb-3 text-sm font-medium">Font Size</h2>
        <div className="flex gap-1">
          {fontSizes.map((fs) => (
            <button
              key={fs.label}
              onClick={() => setFontSize(fs.value)}
              className={cn(
                "rounded-md border px-3 py-1.5 text-xs font-medium transition-colors",
                fontSize === fs.value
                  ? "border-accent-green bg-accent-green/10 text-accent-green dark:border-accent-green dark:bg-accent-green/20 dark:text-accent-green"
                  : "border-zinc-200 text-zinc-600 hover:bg-zinc-50 dark:border-zinc-700 dark:text-zinc-400 dark:hover:bg-zinc-800",
              )}
            >
              {fs.label}
            </button>
          ))}
        </div>
      </div>

      <div>
        <h2 className="mb-3 text-sm font-medium">Opacity</h2>
        <p className="mb-2 text-xs text-zinc-500">窗口透明度（需配合窗口毛玻璃效果使用）</p>
        <div className="flex items-center gap-3">
          <input
            type="range"
            min="0"
            max="1.0"
            step="0.01"
            value={opacity}
            onChange={(e) => setOpacity(parseFloat(e.target.value))}
            className="flex-1"
            style={{ "--progress": `${opacity * 100}%` } as React.CSSProperties}
          />
          <span className="w-10 text-right text-xs text-zinc-600 dark:text-zinc-400">
            {Math.round(opacity * 100)}%
          </span>
        </div>
      </div>

      <button
        onClick={() => { setTheme("system"); setFontSize(0.875); setContentWidth(90); setOpacity(1.0); setAccentColor("#00C375"); }}
        className="rounded-lg btn-solid px-3 py-1.5 text-xs"
      >
        Reset to defaults
      </button>
    </div>
  );
}

/** General settings */
function GeneralTab() {
  const [config, setConfig] = useState<GatewayConfig | null>(null);
  const { logLevel, setLogLevel } = useSettingsStore();

  useEffect(() => {
    invoke<GatewayConfig>("get_config")
      .then((cfg) => {
        setConfig(cfg);
        // Gateway value takes precedence over localStorage
        setLogLevel(cfg.log_level);
      })
      .catch(() => {});
  }, [setLogLevel]);

  const currentLogLevel = config?.log_level || logLevel || "info";

  return (
    <div className="max-w-lg space-y-6">
      <div>
        <h2 className="mb-3 text-sm font-medium">Log Level</h2>
        <select
          value={currentLogLevel}
          onChange={async (e) => {
            const val = e.target.value;
            try {
              await invoke("update_config", { logLevel: val });
              setConfig((prev) => (prev ? { ...prev, log_level: val } : prev));
              setLogLevel(val);
            } catch { /* ignore */ }
          }}
          className={selectBase}
        >
          <option value="trace">trace</option>
          <option value="debug">debug</option>
          <option value="info">info</option>
          <option value="warn">warn</option>
          <option value="error">error</option>
        </select>
      </div>

      <div>
        <h2 className="mb-3 text-sm font-medium">Data Directory</h2>
        <input
          type="text"
          value={config?.data_dir ?? "\u2014"}
          readOnly
          className={`w-full ${inputReadonly}`}
        />
      </div>

      <div>
        <h2 className="mb-3 text-sm font-medium">About</h2>
        <div className="text-xs text-zinc-500 dark:text-zinc-400">
          <p>Rollball Desktop v0.1.0</p>
          <p className="mt-1">Built with Tauri v2 + React 19</p>
        </div>
      </div>
    </div>
  );
}
