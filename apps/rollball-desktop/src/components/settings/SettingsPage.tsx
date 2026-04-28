import { useState, useEffect, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useGatewayStore } from "../../stores/gatewayStore";
import type { GatewayConfig, VaultKeyEntry, ModelInfo } from "../../lib/types";
import { cn } from "../../lib/utils";
import { ALL_PROVIDERS, PROVIDER_CATEGORIES, getProviderDef } from "../../lib/providers";
import { fetchProviderModels } from "../../lib/gateway-api";

type SettingsTab = "gateway" | "providers" | "vault" | "appearance" | "general";

export function SettingsPage() {
  const [activeTab, setActiveTab] = useState<SettingsTab>("gateway");

  const tabs: { id: SettingsTab; label: string }[] = [
    { id: "gateway", label: "Gateway" },
    { id: "providers", label: "Providers" },
    { id: "vault", label: "Vault" },
    { id: "appearance", label: "Appearance" },
    { id: "general", label: "General" },
  ];

  return (
    <div className="flex flex-1 flex-col bg-white dark:bg-zinc-900">
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
                ? "border-zinc-800 text-zinc-900 dark:border-zinc-200 dark:text-zinc-100"
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
        {activeTab === "vault" && <VaultTab />}
        {activeTab === "appearance" && <AppearanceTab />}
        {activeTab === "general" && <GeneralTab />}
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
            value="http://127.0.0.1:19876"
            readOnly
            className="w-full rounded-md border border-zinc-200 bg-zinc-50 px-3 py-2 text-sm dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-300"
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
          className="rounded-md bg-zinc-800 px-3 py-1.5 text-xs font-medium text-white hover:bg-zinc-700 disabled:opacity-50 dark:bg-zinc-700 dark:hover:bg-zinc-600"
        >
          {testing ? "Testing..." : "Test Connection"}
        </button>
      </div>
    </div>
  );
}

/** Provider configuration */
function ProvidersTab() {
  const [keys, setKeys] = useState<VaultKeyEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [showAddDialog, setShowAddDialog] = useState(false);
  const [showEditDialog, setShowEditDialog] = useState<string | null>(null);
  const [newProvider, setNewProvider] = useState("openai");
  const [newKey, setNewKey] = useState("");
  const [newBaseUrl, setNewBaseUrl] = useState("");
  const [newModels, setNewModels] = useState<string[]>([]);
  const [availableModels, setAvailableModels] = useState<ModelInfo[]>([]);
  const [modelsLoading, setModelsLoading] = useState(false);
  const [modelSearchTerm, setModelSearchTerm] = useState("");

  // Edit dialog state
  const [editKey, setEditKey] = useState("");
  const [editBaseUrl, setEditBaseUrl] = useState("");
  const [editModels, setEditModels] = useState<string[]>([]);
  const [editAvailableModels, setEditAvailableModels] = useState<ModelInfo[]>([]);
  const [editModelsLoading, setEditModelsLoading] = useState(false);
  const [editModelSearchTerm, setEditModelSearchTerm] = useState("");

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

  useEffect(() => { fetchKeys(); }, [fetchKeys]);

  // Fetch available models for a provider from Gateway API
  const fetchModels = useCallback(async (providerId: string): Promise<ModelInfo[]> => {
    try {
      const data = await fetchProviderModels(providerId);
      return data.models ?? [];
    } catch {
      // Fallback to exampleModels from provider definition
      const def = getProviderDef(providerId);
      return (def?.exampleModels ?? []).map((id) => ({ id, name: id }));
    }
  }, []);

  const handleAddProviderChange = async (id: string) => {
    setNewProvider(id);
    const def = getProviderDef(id);
    setNewBaseUrl(def?.baseUrl ?? "");
    setNewModels([]);
    setModelSearchTerm("");
    // Fetch available models for the selected provider
    setModelsLoading(true);
    const models = await fetchModels(id);
    setAvailableModels(models);
    setModelsLoading(false);
  };

  const handleAdd = async () => {
    try {
      await invoke("add_key", {
        provider: newProvider,
        key: newKey,
        baseUrl: newBaseUrl || undefined,
        defaultModel: newModels.length > 0 ? newModels[0] : undefined,
      });
      setShowAddDialog(false);
      setNewKey("");
      setNewModels([]);
      await fetchKeys();
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

  const handleEdit = async (provider: string) => {
    const keyEntry = keys.find((k) => k.provider === provider);
    const def = getProviderDef(provider);
    setEditKey(keyEntry?.key_preview ?? "");
    setEditBaseUrl(keyEntry?.base_url ?? def?.baseUrl ?? "");
    setEditModels(keyEntry?.default_model ? [keyEntry.default_model] : []);
    setEditModelSearchTerm("");
    setShowEditDialog(provider);
    // Fetch models
    setEditModelsLoading(true);
    const models = await fetchModels(provider);
    setEditAvailableModels(models);
    setEditModelsLoading(false);
  };

  const handleEditSave = async () => {
    if (!showEditDialog) return;
    try {
      await invoke("update_key", {
        provider: showEditDialog,
        key: editKey,
        baseUrl: editBaseUrl || undefined,
        defaultModel: editModels.length > 0 ? editModels[0] : undefined,
      });
      setShowEditDialog(null);
      await fetchKeys();
    } catch (e) {
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
    <div className="max-w-lg space-y-4">
      <div className="flex items-center justify-between">
        <h2 className="text-sm font-medium">Provider Management</h2>
        <button
          onClick={() => {
            setShowAddDialog(true);
            // Pre-fetch models for default provider
            fetchModels("openai").then((models) => setAvailableModels(models));
          }}
          className="rounded-md bg-zinc-800 px-3 py-1 text-xs font-medium text-white hover:bg-zinc-700 dark:bg-zinc-700 dark:hover:bg-zinc-600"
        >
          + Add Key
        </button>
      </div>

      {loading ? (
        <div className="py-8 text-center text-xs text-zinc-400">Loading...</div>
      ) : (
        <div className="space-y-3">
          {PROVIDER_CATEGORIES.map((cat) => {
            const catProviders = ALL_PROVIDERS.filter((p) => p.category === cat.id);
            if (catProviders.length === 0) return null;
            return (
              <div key={cat.id}>
                <h3 className="mb-2 text-xs font-medium text-zinc-500">{cat.label}</h3>
                <div className="space-y-2">
                  {catProviders.map((provider) => {
                    const keyEntry = keys.find((k) => k.provider === provider.id);
                    return (
                      <div key={provider.id} className="rounded-lg border border-zinc-200 p-3 dark:border-zinc-700">
                        <div className="flex items-center justify-between">
                          <div className="min-w-0 flex-1">
                            <span className="text-sm font-medium">{provider.name}</span>
                            {keyEntry?.default_model && (
                              <span className="ml-2 text-xs text-blue-500 dark:text-blue-400">{keyEntry.default_model}</span>
                            )}
                            {!keyEntry?.default_model && keyEntry && (
                              <span className="ml-2 text-xs text-zinc-400">{provider.exampleModels[0] ?? "—"}</span>
                            )}
                            {!keyEntry && provider.description && (
                              <span className="ml-2 text-xs text-zinc-400">— {provider.description}</span>
                            )}
                          </div>
                          {keyEntry ? (
                            <div className="flex items-center gap-2">
                              <span className="text-xs text-green-600 dark:text-green-400">Active</span>
                              <span className="text-xs text-zinc-400">Key: {keyEntry.key_preview}</span>
                              <button
                                onClick={() => handleEdit(provider.id)}
                                className="text-xs text-blue-500 hover:text-blue-700 dark:text-blue-400 dark:hover:text-blue-300"
                              >
                                Edit
                              </button>
                              <button
                                onClick={() => handleRemove(provider.id)}
                                className="text-xs text-red-500 hover:text-red-700"
                              >
                                Remove
                              </button>
                            </div>
                          ) : !provider.needsApiKey ? (
                            <span className="text-xs text-zinc-400">No API key needed</span>
                          ) : (
                            <span className="text-xs text-zinc-400">Not configured</span>
                          )}
                        </div>
                      </div>
                    );
                  })}
                </div>
              </div>
            );
          })}
        </div>
      )}

      {/* Add key dialog */}
      {showAddDialog && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50">
          <div className="w-[440px] max-h-[85vh] overflow-y-auto rounded-lg bg-white p-6 shadow-xl dark:bg-zinc-800">
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

              {newProviderDef?.needsApiKey && (
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

              {/* Model selection (multi-select) */}
              <div>
                <label className="mb-1 block text-xs text-zinc-500">
                  Default Model {newModels.length > 0 && <span className="text-blue-500">({newModels.length} selected)</span>}
                </label>
                {/* Selected models as tags */}
                {newModels.length > 0 && (
                  <div className="mb-1 flex flex-wrap gap-1">
                    {newModels.map((m) => (
                      <span key={m} className="inline-flex items-center gap-1 rounded bg-blue-100 px-2 py-0.5 text-xs text-blue-700 dark:bg-blue-900 dark:text-blue-300">
                        {m}
                        <button onClick={() => setNewModels(newModels.filter((x) => x !== m))} className="text-blue-400 hover:text-blue-600">×</button>
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
                  className="w-full rounded-md border border-zinc-200 px-3 py-1.5 text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                />
                <div className="mt-1 max-h-40 overflow-y-auto rounded border border-zinc-200 dark:border-zinc-700">
                  {modelsLoading ? (
                    <div className="px-3 py-2 text-xs text-zinc-400">Loading models...</div>
                  ) : (
                    availableModels
                      .filter((m) =>
                        !modelSearchTerm ||
                        m.id.toLowerCase().includes(modelSearchTerm.toLowerCase()) ||
                        m.name.toLowerCase().includes(modelSearchTerm.toLowerCase())
                      )
                      .map((m) => (
                        <label
                          key={m.id}
                          className="flex cursor-pointer items-center gap-2 px-3 py-1.5 text-xs hover:bg-zinc-50 dark:hover:bg-zinc-700"
                        >
                          <input
                            type="checkbox"
                            checked={newModels.includes(m.id)}
                            onChange={() => toggleModel(m.id, newModels, setNewModels)}
                            className="accent-blue-600"
                          />
                          <span className="flex-1 truncate">{m.name || m.id}</span>
                          {m.reasoning && <span className="text-[10px] text-zinc-400">reasoning</span>}
                          {m.tool_call && <span className="text-[10px] text-zinc-400">tools</span>}
                        </label>
                      ))
                  )}
                  {!modelsLoading && availableModels.length === 0 && (
                    <div className="px-3 py-2 text-xs text-zinc-400">No models found. Select provider first.</div>
                  )}
                </div>
                {/* Manual model input */}
                <div className="mt-1 flex gap-1">
                  <input
                    type="text"
                    placeholder="Or type a custom model name..."
                    className="flex-1 rounded-md border border-zinc-200 px-2 py-1 text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
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

              {newProviderDef?.description && (
                <p className="text-xs text-zinc-400">{newProviderDef.description}</p>
              )}
            </div>

            <div className="mt-4 flex justify-end gap-2">
              <button
                onClick={() => { setShowAddDialog(false); setNewModels([]); }}
                className="rounded-md px-3 py-1.5 text-xs font-medium text-zinc-600 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-700"
              >
                Cancel
              </button>
              <button
                onClick={handleAdd}
                disabled={newProviderDef?.needsApiKey ? !newKey.trim() : false}
                className="rounded-md bg-zinc-800 px-3 py-1.5 text-xs font-medium text-white hover:bg-zinc-700 disabled:opacity-50 dark:bg-zinc-700 dark:hover:bg-zinc-600"
              >
                Save
              </button>
            </div>
          </div>
        </div>
      )}

      {/* Edit key dialog */}
      {showEditDialog && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50">
          <div className="w-[440px] max-h-[85vh] overflow-y-auto rounded-lg bg-white p-6 shadow-xl dark:bg-zinc-800">
            <h3 className="mb-4 text-sm font-semibold">Edit: {showEditDialog}</h3>

            <div className="space-y-3">
              <div>
                <label className="mb-1 block text-xs text-zinc-500">API Key</label>
                <input
                  type="password"
                  value={editKey}
                  onChange={(e) => setEditKey(e.target.value)}
                  placeholder="Enter new API key..."
                  className="w-full rounded-md border border-zinc-200 px-3 py-2 text-sm dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                />
              </div>

              {(getProviderDef(showEditDialog)?.editableBaseUrl ?? true) && (
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
                  Default Model {editModels.length > 0 && <span className="text-blue-500">({editModels.length} selected)</span>}
                </label>
                {editModels.length > 0 && (
                  <div className="mb-1 flex flex-wrap gap-1">
                    {editModels.map((m) => (
                      <span key={m} className="inline-flex items-center gap-1 rounded bg-blue-100 px-2 py-0.5 text-xs text-blue-700 dark:bg-blue-900 dark:text-blue-300">
                        {m}
                        <button onClick={() => setEditModels(editModels.filter((x) => x !== m))} className="text-blue-400 hover:text-blue-600">×</button>
                      </span>
                    ))}
                  </div>
                )}
                <input
                  type="text"
                  value={editModelSearchTerm}
                  onChange={(e) => setEditModelSearchTerm(e.target.value)}
                  placeholder="Search models..."
                  className="w-full rounded-md border border-zinc-200 px-3 py-1.5 text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
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
                            className="accent-blue-600"
                          />
                          <span className="flex-1 truncate">{m.name || m.id}</span>
                          {m.reasoning && <span className="text-[10px] text-zinc-400">reasoning</span>}
                          {m.tool_call && <span className="text-[10px] text-zinc-400">tools</span>}
                        </label>
                      ))
                  )}
                </div>
                <div className="mt-1 flex gap-1">
                  <input
                    type="text"
                    placeholder="Or type a custom model name..."
                    className="flex-1 rounded-md border border-zinc-200 px-2 py-1 text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
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
            </div>

            <div className="mt-4 flex justify-end gap-2">
              <button
                onClick={() => setShowEditDialog(null)}
                className="rounded-md px-3 py-1.5 text-xs font-medium text-zinc-600 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-700"
              >
                Cancel
              </button>
              <button
                onClick={handleEditSave}
                disabled={!editKey.trim()}
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

/** Vault key management */
function VaultTab() {
  return <ProvidersTab />;
}

/** Appearance settings */
function AppearanceTab() {
  const [theme, setTheme] = useState<"light" | "dark" | "system">("system");
  const [fontSize, setFontSize] = useState(1.0);

  const fontSizes = [
    { label: "S", value: 0.875 },
    { label: "M", value: 1.0 },
    { label: "L", value: 1.125 },
    { label: "XL", value: 1.25 },
    { label: "XXL", value: 1.375 },
  ];

  useEffect(() => {
    // Apply dark class to html element
    if (theme === "dark") {
      document.documentElement.classList.add("dark");
    } else if (theme === "light") {
      document.documentElement.classList.remove("dark");
    } else {
      // System preference
      const prefersDark = window.matchMedia("(prefers-color-scheme: dark)").matches;
      document.documentElement.classList.toggle("dark", prefersDark);
    }
  }, [theme]);

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
                className="accent-zinc-800"
              />
              {t.charAt(0).toUpperCase() + t.slice(1)}
            </label>
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
                  ? "border-zinc-800 bg-zinc-800 text-white dark:border-zinc-200 dark:bg-zinc-200 dark:text-zinc-900"
                  : "border-zinc-200 text-zinc-600 hover:bg-zinc-50 dark:border-zinc-700 dark:text-zinc-400 dark:hover:bg-zinc-800",
              )}
            >
              {fs.label}
            </button>
          ))}
        </div>
      </div>

      <button
        onClick={() => { setTheme("system"); setFontSize(1.0); }}
        className="text-xs text-zinc-500 hover:text-zinc-700 dark:hover:text-zinc-300"
      >
        Reset to defaults
      </button>
    </div>
  );
}

/** General settings */
function GeneralTab() {
  const [config, setConfig] = useState<GatewayConfig | null>(null);

  useEffect(() => {
    invoke<GatewayConfig>("get_config").then(setConfig).catch(() => {});
  }, []);

  return (
    <div className="max-w-lg space-y-6">
      <div>
        <h2 className="mb-3 text-sm font-medium">Log Level</h2>
        <select
          defaultValue="info"
          onChange={async (e) => {
            try {
              await invoke("update_config", { logLevel: e.target.value });
            } catch { /* ignore */ }
          }}
          className="rounded-md border border-zinc-200 px-3 py-2 text-sm dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
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
          value={config?.data_dir ?? "—"}
          readOnly
          className="w-full rounded-md border border-zinc-200 bg-zinc-50 px-3 py-2 text-sm dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-400"
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
