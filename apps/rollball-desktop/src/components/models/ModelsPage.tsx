import { useState, useEffect, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { VaultKeyEntry, GatewayConfig, ModelInfo } from "../../lib/types";
import { Key, Home, Plus, Trash2, Star, ChevronDown, Pencil, Loader2 } from "lucide-react";
import { ALL_PROVIDERS, PROVIDER_CATEGORIES, getProviderDef } from "../../lib/providers";
import { fetchProviderModels } from "../../lib/gateway-api";

type ProviderWithStatus = {
  id: string;
  name: string;
  models: string;
  local: boolean;
  baseUrl: string;
  category: string;
  description?: string;
};

export function ModelsPage() {
  const [keys, setKeys] = useState<VaultKeyEntry[]>([]);
  const [config, setConfig] = useState<GatewayConfig | null>(null);
  const [loading, setLoading] = useState(true);
  const [showAddDialog, setShowAddDialog] = useState(false);
  const [newProvider, setNewProvider] = useState("openai");
  const [newKey, setNewKey] = useState("");
  const [newBaseUrl, setNewBaseUrl] = useState("");
  const [newDefaultModel, setNewDefaultModel] = useState("");
  const [customModelInput, setCustomModelInput] = useState(false);
  const [dynamicModels, setDynamicModels] = useState<ModelInfo[]>([]);
  const [modelsLoading, setModelsLoading] = useState(false);

  const newProviderDef = getProviderDef(newProvider);

  const fetchKeys = useCallback(async () => {
    try {
      const result = await invoke<VaultKeyEntry[]>("list_keys");
      setKeys(result);
    } catch {
      // Gateway may not be running
    } finally {
      setLoading(false);
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

  useEffect(() => { fetchKeys(); fetchConfig(); }, [fetchKeys, fetchConfig]);

  // Load dynamic models for the default provider on mount
  useEffect(() => {
    setModelsLoading(true);
    fetchProviderModels(newProvider)
      .then((resp) => {
        setDynamicModels(resp.models);
        if (resp.models.length > 0) {
          setNewDefaultModel(resp.models[0].id);
        }
      })
      .catch((e) => { console.warn("Failed to fetch models on mount:", e); setDynamicModels([]); })
      .finally(() => setModelsLoading(false));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const handleAddProviderChange = (id: string) => {
    setNewProvider(id);
    const def = getProviderDef(id);
    setNewBaseUrl(def?.baseUrl ?? "");
    setNewDefaultModel(def?.exampleModels[0] ?? "");
    setCustomModelInput(false);
    // Fetch dynamic model list from Gateway (models.dev)
    setDynamicModels([]);
    setModelsLoading(true);
    fetchProviderModels(id)
      .then((resp) => {
        setDynamicModels(resp.models);
        if (resp.models.length > 0) {
          setNewDefaultModel(resp.models[0].id);
        }
      })
      .catch((e) => {
        // Fallback: use hardcoded exampleModels from provider def
        console.warn(`Failed to fetch models for ${id}:`, e);
        setDynamicModels([]);
      })
      .finally(() => setModelsLoading(false));
  };

  const handleAdd = async () => {
    try {
      await invoke("add_key", {
        provider: newProvider,
        key: newKey,
        baseUrl: newBaseUrl || undefined,
        defaultModel: newDefaultModel || undefined,
      });
      setShowAddDialog(false);
      setNewKey("");
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
      await fetchConfig();
    } catch (e) {
      alert(`Failed to remove key: ${e}`);
    }
  };

  const handleSetDefaultProvider = async (provider: string) => {
    try {
      const entry = keys.find((k) => k.provider === provider);
      await invoke("update_config", {
        defaultProvider: provider,
        defaultModel: entry?.default_model || undefined,
      });
      await fetchConfig();
    } catch (e) {
      alert(`Failed to set default provider: ${e}`);
    }
  };

  const providers: ProviderWithStatus[] = ALL_PROVIDERS.map((p) => ({
    id: p.id,
    name: p.name,
    models: p.exampleModels.join(", "),
    local: !p.needsApiKey,
    baseUrl: p.baseUrl,
    category: p.category,
    description: p.description,
  }));

  return (
    <div className="flex flex-1 flex-col bg-white dark:bg-zinc-900">
      {/* Header */}
      <div className="flex items-center justify-between border-b border-zinc-200 px-6 py-4 dark:border-zinc-800">
        <h1 className="text-xl font-semibold">Models</h1>
        <button
          onClick={() => setShowAddDialog(true)}
          className="inline-flex items-center gap-1.5 rounded-md bg-zinc-800 px-3 py-1.5 text-xs font-medium text-white hover:bg-zinc-700 dark:bg-zinc-700 dark:hover:bg-zinc-600"
        >
          <Plus className="h-3.5 w-3.5" /> Add Key
        </button>
      </div>

      {/* Content */}
      <div className="flex-1 overflow-y-auto p-6">
        <div className="mx-auto max-w-2xl space-y-4">
          {loading ? (
            <div className="py-8 text-center text-sm text-zinc-400">Loading providers...</div>
          ) : (
            providers.map((provider) => {
              const keyEntry = keys.find((k) => k.provider === provider.id);
              return (
                <div key={provider.id} className="rounded-lg border border-zinc-200 p-4 dark:border-zinc-700">
                  <div className="flex items-start justify-between">
                    <div className="flex items-center gap-3">
                      {provider.local ? (
                        <Home className="h-5 w-5 text-zinc-400" />
                      ) : (
                        <Key className="h-5 w-5 text-zinc-400" />
                      )}
                      <div>
                        <div className="flex items-center gap-2">
                          <span className="font-medium">{provider.name}</span>
                          {keyEntry ? (
                            <span className="rounded bg-green-100 px-1.5 py-0.5 text-xs text-green-700 dark:bg-green-900 dark:text-green-300">
                              Active
                            </span>
                          ) : !provider.local ? (
                            <span className="rounded bg-zinc-100 px-1.5 py-0.5 text-xs text-zinc-500 dark:bg-zinc-800 dark:text-zinc-400">
                              Not configured
                            </span>
                          ) : (
                            <span className="rounded bg-blue-100 px-1.5 py-0.5 text-xs text-blue-700 dark:bg-blue-900 dark:text-blue-300">
                              Available
                            </span>
                          )}
                        </div>
                        <p className="mt-1 text-xs text-zinc-400">{provider.models}</p>
                        {keyEntry && (
                          <p className="mt-1 text-xs text-zinc-400">Key: {keyEntry.key_preview}</p>
                        )}
                        {keyEntry?.base_url && (
                          <p className="mt-1 font-mono text-xs text-zinc-400">URL: {keyEntry.base_url}</p>
                        )}
                        {keyEntry?.default_model && (
                          <p className="mt-1 text-xs text-zinc-400">Model: {keyEntry.default_model}</p>
                        )}
                        {!keyEntry?.base_url && provider.baseUrl && (
                          <p className="mt-1 font-mono text-xs text-zinc-400">{provider.baseUrl}</p>
                        )}
                        {provider.description && (
                          <p className="mt-0.5 text-xs text-zinc-400">{provider.description}</p>
                        )}
                      </div>
                    </div>
                    <div className="flex items-center gap-1">
                      {keyEntry && (
                        <>
                          <button
                            onClick={() => handleSetDefaultProvider(provider.id)}
                            className={`rounded p-1.5 ${config?.default_provider === provider.id ? "text-amber-500" : "text-zinc-400 hover:bg-zinc-100 hover:text-amber-500 dark:hover:bg-zinc-800"}`}
                            title={config?.default_provider === provider.id ? "Default provider" : "Set as default provider"}
                          >
                            <Star className="h-4 w-4" />
                          </button>
                          <button
                            onClick={() => handleRemove(provider.id)}
                            className="rounded p-1.5 text-zinc-400 hover:bg-zinc-100 hover:text-red-500 dark:hover:bg-zinc-800"
                            title="Remove key"
                          >
                            <Trash2 className="h-4 w-4" />
                          </button>
                        </>
                      )}
                    </div>
                  </div>
                </div>
              );
            })
          )}
        </div>
      </div>

      {/* Add key dialog */}
      {showAddDialog && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50">
          <div className="w-96 rounded-lg bg-white p-6 shadow-xl dark:bg-zinc-800">
            <h3 className="mb-4 text-sm font-semibold">Add API Key</h3>
            <div className="space-y-3">
              <div>
                <label className="mb-1 block text-xs text-zinc-500">Provider</label>
                <select
                  value={newProvider}
                  onChange={(e) => handleAddProviderChange(e.target.value)}
                  className="w-full rounded-md border border-zinc-200 px-3 py-2 text-sm dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                >
                  {PROVIDER_CATEGORIES.map((cat) => (
                    <optgroup key={cat.id} label={cat.label}>
                      {ALL_PROVIDERS.filter((p) => p.category === cat.id).map((p) => (
                        <option key={p.id} value={p.id}>{p.name}</option>
                      ))}
                    </optgroup>
                  ))}
                </select>
              </div>
              {(newProviderDef?.needsApiKey ?? true) && (
                <div>
                  <label className="mb-1 block text-xs text-zinc-500">API Key</label>
                  <input
                    type="password"
                    value={newKey}
                    onChange={(e) => setNewKey(e.target.value)}
                    placeholder={newProviderDef?.keyPlaceholder ?? "API key..."}
                    className="w-full rounded-md border border-zinc-200 px-3 py-2 text-sm dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                  />
                </div>
              )}
              {newProviderDef?.editableBaseUrl && (
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
              )}
              <div>
                <label className="mb-1 block text-xs text-zinc-500">Default Model</label>
                {modelsLoading ? (
                  <div className="flex items-center gap-2 text-xs text-zinc-400">
                    <Loader2 className="h-3.5 w-3.5 animate-spin" /> Loading models...
                  </div>
                ) : dynamicModels.length > 0 && !customModelInput ? (
                  <div className="flex items-center gap-1">
                    <select
                      value={newDefaultModel}
                      onChange={(e) => setNewDefaultModel(e.target.value)}
                      className="flex-1 rounded-md border border-zinc-200 px-3 py-2 font-mono text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                    >
                      {dynamicModels.map((m) => (
                        <option key={m.id} value={m.id}>
                          {m.id}{m.reasoning ? " \u2b50" : ""}{m.tool_call ? " \ud83d\udd27" : ""}
                        </option>
                      ))}
                    </select>
                    <button
                      type="button"
                      onClick={() => setCustomModelInput(true)}
                      className="rounded p-1.5 text-zinc-400 hover:bg-zinc-100 hover:text-zinc-600 dark:hover:bg-zinc-800"
                      title="Enter custom model name"
                    >
                      <Pencil className="h-3.5 w-3.5" />
                    </button>
                  </div>
                ) : newProviderDef && newProviderDef.exampleModels.length > 0 && !customModelInput ? (
                  <div className="flex items-center gap-1">
                    <select
                      value={newDefaultModel}
                      onChange={(e) => setNewDefaultModel(e.target.value)}
                      className="flex-1 rounded-md border border-zinc-200 px-3 py-2 font-mono text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                    >
                      {newProviderDef.exampleModels.map((m) => (
                        <option key={m} value={m}>{m}</option>
                      ))}
                    </select>
                    <button
                      type="button"
                      onClick={() => setCustomModelInput(true)}
                      className="rounded p-1.5 text-zinc-400 hover:bg-zinc-100 hover:text-zinc-600 dark:hover:bg-zinc-800"
                      title="Enter custom model name"
                    >
                      <Pencil className="h-3.5 w-3.5" />
                    </button>
                  </div>
                ) : (
                  <div className="flex items-center gap-1">
                    <input
                      type="text"
                      value={newDefaultModel}
                      onChange={(e) => setNewDefaultModel(e.target.value)}
                      placeholder={newProviderDef?.exampleModels[0] ?? "model name..."}
                      className="flex-1 rounded-md border border-zinc-200 px-3 py-2 font-mono text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                    />
                    {newProviderDef && newProviderDef.exampleModels.length > 0 && (
                      <button
                        type="button"
                        onClick={() => { setCustomModelInput(false); setNewDefaultModel(newProviderDef.exampleModels[0]); }}
                        className="rounded p-1.5 text-zinc-400 hover:bg-zinc-100 hover:text-zinc-600 dark:hover:bg-zinc-800"
                        title="Back to model selector"
                      >
                        <ChevronDown className="h-3.5 w-3.5" />
                      </button>
                    )}
                  </div>
                )}
              </div>
              {newProviderDef?.description && (
                <p className="text-xs text-zinc-400">{newProviderDef.description}</p>
              )}
            </div>
            <div className="mt-4 flex justify-end gap-2">
              <button
                onClick={() => setShowAddDialog(false)}
                className="rounded-md px-3 py-1.5 text-xs font-medium text-zinc-600 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-700"
              >
                Cancel
              </button>
              <button
                onClick={handleAdd}
                disabled={(newProviderDef?.needsApiKey ?? true) ? !newKey.trim() : false}
                className="rounded-md bg-zinc-800 px-3 py-1.5 text-xs font-medium text-white hover:bg-zinc-700 disabled:opacity-50 dark:bg-zinc-700 dark:hover:bg-zinc-600"
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
