import { useState, useEffect, useCallback, useMemo } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { VaultKeyEntry, ModelInfo, ModelCapabilitiesInfo, ProviderListEntry, McpServerConfigDef, McpTransportDef, McpPresetDef } from "../../lib/types";
import { cn } from "../../lib/utils";
import { inputBase, selectBase } from "../../lib/ui-styles";
import { StyledInput } from "../common/StyledInput";
import { isLocalProvider } from "../../lib/providers";
import { fetchProviderModels } from "../../lib/gateway-api";
import { getGatewayUrl } from "../../lib/config";
import { Monitor, Search, Globe, BookOpen, FileText, PenTool, Star } from "lucide-react";
import { useMcpStore } from "../../stores/mcpStore";
import { MCP_PRESETS, presetToServerConfig } from "../../lib/mcp-presets";
import { SearchTab } from "./SearchTab";
import { EmbeddingModelTab } from "./EmbeddingModelTab";
import { LspTab } from "./LspTab";
import { ModelCapEditor } from "./ModelCapEditor";
import { ProviderPicker } from "./ProviderPicker";
import { AddProviderFlow } from "./AddProviderFlow";
import { useTranslation } from "../../i18n/useTranslation";
import { Tooltip } from "../common/Tooltip";
import { ErrorBox } from "../common/ErrorBox";
import { TabButton } from "../common/tab";

type HarnessTab = "providers" | "search" | "mcp" | "embedding" | "lsp";

export function HarnessPage() {
  const { t } = useTranslation();
  const [activeTab, setActiveTab] = useState<HarnessTab>("providers");

  const tabs: { id: HarnessTab; label: string }[] = [
    { id: "providers", label: t("harness.tabProviders") },
    { id: "search", label: t("harness.tabSearch") },
    { id: "mcp", label: t("harness.tabMcp") },
    { id: "embedding", label: t("harness.tabEmbedding") },
    { id: "lsp", label: t("harnessLsp.tabLsp") },
  ];

  return (
    <div className="flex flex-1 flex-col bg-zinc-50 dark:bg-zinc-900">
      {/* Tabs */}
      <div className="flex gap-1 border-b border-zinc-200 px-6 pt-2 dark:border-zinc-800">
        {tabs.map((tab) => (
          <TabButton
            key={tab.id}
            onClick={() => setActiveTab(tab.id)}
            active={activeTab === tab.id}
          >
            {tab.label}
          </TabButton>
        ))}
      </div>

      {/* Tab content */}
      <div className="flex-1 overflow-y-auto p-6">
        {activeTab === "providers" && <ProvidersTab />}
        {activeTab === "search" && <SearchTab />}
        {activeTab === "mcp" && <McpTab />}
        {activeTab === "embedding" && <EmbeddingModelTab />}
        {activeTab === "lsp" && <LspTab />}
      </div>
    </div>
  );
}

/** Provider configuration */
function ProvidersTab() {
  const { t } = useTranslation();
  const [keys, setKeys] = useState<VaultKeyEntry[]>([]);
  const [keysLoading, setKeysLoading] = useState(true);
  const [showEditDialog, setShowEditDialog] = useState<string | null>(null);

  // AddProviderFlow state
  const [showAddFlow, setShowAddFlow] = useState(false);
  const [addFlowProvider, setAddFlowProvider] = useState<string | undefined>(undefined);
  const [addFlowEntry, setAddFlowEntry] = useState<ProviderListEntry | undefined>(undefined);

  // Edit dialog state
  const [editKey, setEditKey] = useState("");
  const [editBaseUrl, setEditBaseUrl] = useState("");
  const [editModels, setEditModels] = useState<string[]>([]);
  const [editAvailableModels, setEditAvailableModels] = useState<ModelInfo[]>([]);
  const [editModelsLoading, setEditModelsLoading] = useState(false);
  const [editModelSearchTerm, setEditModelSearchTerm] = useState("");
  const [editModelCapabilityFilter, setEditModelCapabilityFilter] = useState<string[]>([]);

  // Edit dialog — per-model capabilities state
  const [editModelCaps, setEditModelCaps] = useState<Record<string, ModelCapabilitiesInfo>>({});
  const [editExpandedModels, setEditExpandedModels] = useState<Set<string>>(new Set());
  const [editCompactModel, setEditCompactModel] = useState("");

  // Gateway config for default provider indication
  const [config, setConfig] = useState<GatewayConfig | null>(null);

  // Dynamic provider list from Gateway API
  const [dynamicProviders, setDynamicProviders] = useState<ProviderListEntry[]>([]);


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

  // Load provider list from Gateway API (offline_providers.json is the sole data source)
  const loadProviders = useCallback(async () => {
    try {
      const response = await fetch(`${getGatewayUrl()}/api/models`);
      if (response.ok) {
        const data = await response.json();
        setDynamicProviders(data.providers ?? []);
      }
    } catch {
      // Gateway may not be running
    }
  }, []);

  useEffect(() => {
    fetchKeys();
    fetchConfig();
    loadProviders();
  }, [fetchKeys, fetchConfig, loadProviders]);

  // Fetch available models for a provider from Gateway API
  const fetchModels = useCallback(async (providerId: string): Promise<ModelInfo[]> => {
    try {
      const data = await fetchProviderModels(providerId);
      return data.models ?? [];
    } catch {
      return [];
    }
  }, []);

  const handleRemove = async (provider: string) => {
    if (!confirm(t("harness.removeKeyConfirm", { provider }))) return;
    try {
      await invoke("remove_key", { provider });
      await fetchKeys();
    } catch (e) {
      alert(`${t("harness.failedRemoveKey")}: ${e}`);
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
      alert(`${t("harness.failedSetDefault")}: ${e}`);
    }
  };

  const handleEdit = async (provider: string) => {
    const keyEntry = keys.find((k) => k.provider === provider);
    const dynamicProvider = dynamicProviders.find((p) => p.id === provider);
    setEditKey(keyEntry?.key_preview ?? "");
    setEditBaseUrl(keyEntry?.base_url ?? dynamicProvider?.api ?? "");
    const configuredModels = keyEntry?.models?.length ? keyEntry.models : keyEntry?.default_model ? [keyEntry.default_model] : [];
    setEditModels(configuredModels);
    setEditModelSearchTerm("");
    setEditModelCapabilityFilter([]);
    setEditCompactModel(keyEntry?.compact_model ?? "");
    setEditModelCaps({});
    setEditExpandedModels(new Set());
    setShowEditDialog(provider);
    // Fetch models from Gateway API (includes input_modalities, context_window, etc.)
    setEditModelsLoading(true);
    const models = await fetchModels(provider);
    setEditAvailableModels(models);
    setEditModelsLoading(false);
    // Initialize per-model caps: prefer stored caps from vault (preserves
    // default_reasoning_effort, supports_reasoning, etc.), fall back to
    // live model data or sensible defaults.
    const storedCaps = keyEntry?.model_capabilities ?? {};
    const caps: Record<string, ModelCapabilitiesInfo> = {};
    for (const modelId of configuredModels) {
      const mi = models.find(m => m.id === modelId);
      const stored = storedCaps[modelId];
      caps[modelId] = stored
        ? { ...makeDefaultCaps(mi), ...stored }
        : makeDefaultCaps(mi);
    }
    setEditModelCaps(caps);
  };

  const handleEditSave = async () => {
    if (!showEditDialog) return;
    try {
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
      // For local/custom providers, send per-model capabilities
      const isLocal = isLocalProvider(showEditDialog);
      const isCustom = keyEntry?.custom ?? false;
      if ((isLocal || isCustom) && editModels.length > 0 && Object.keys(editModelCaps).length > 0) {
        updatePayload.modelCapabilities = editModelCaps;
      }
      // Include compact_model if set
      if (editCompactModel) {
        updatePayload.compactModel = editCompactModel;
      } else {
        updatePayload.compactModel = null;  // Explicitly clear if empty
      }
      await invoke("update_key", updatePayload);
      setShowEditDialog(null);
      await fetchKeys();
      await fetchConfig();
      window.dispatchEvent(new CustomEvent('models-added'));
    } catch (e) {
      alert(`${t("harness.failedUpdateKey")}: ${e}`);
    }
  };

  // Helper: create default capabilities for a model
  const makeDefaultCaps = (mi: ModelInfo | undefined): ModelCapabilitiesInfo => {
    if (mi && (mi.context_window || mi.max_tokens)) {
      return {
        context_window: mi.context_window ?? 128000,
        max_output_tokens: mi.max_tokens ?? 16384,
        supports_tool_calling: mi.tool_call ?? true,
        supports_reasoning: mi.reasoning ?? false,
        modalities: {
          input: mi.input_modalities ?? ["text"],
          output: mi.output_modalities ?? ["text"],
        },
      };
    }
    return {
      context_window: 128000,
      max_output_tokens: 16384,
      supports_tool_calling: true,
      supports_reasoning: false,
      modalities: { input: ["text"], output: ["text"] },
    };
  };

  // Toggle model in edit dialog (with caps management)
  const toggleEditModel = (model: string) => {
    if (editModels.includes(model)) {
      setEditModels(editModels.filter(m => m !== model));
      const next = { ...editModelCaps };
      delete next[model];
      setEditModelCaps(next);
    } else {
      setEditModels([...editModels, model]);
      const mi = editAvailableModels.find(m => m.id === model);
      // Preserve stored caps when re-selecting a previously configured model
      const keyEntry = keys.find(k => k.provider === showEditDialog);
      const stored = keyEntry?.model_capabilities?.[model];
      setEditModelCaps({
        ...editModelCaps,
        [model]: stored ? { ...makeDefaultCaps(mi), ...stored } : makeDefaultCaps(mi),
      });
    }
  };

  // Update a single field in a model's capabilities (edit dialog)
  const updateEditModelCap = (modelId: string, field: keyof ModelCapabilitiesInfo, value: unknown) => {
    setEditModelCaps(prev => ({
      ...prev,
      [modelId]: { ...prev[modelId], [field]: value },
    }));
  };

  return (
    <div className="max-w-2xl space-y-4">
      <div className="rounded-md border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <div className="flex items-center justify-between mb-3">
          <h2 className="text-xs font-medium">{t("harness.providerManagement")}</h2>
        </div>

        {/* Configured Providers (top section) — depends on fetchKeys */}
        {keysLoading ? (
          <div className="py-3 text-center text-xs text-zinc-400">{t("harness.loadingKeys")}</div>
        ) : keys.length > 0 && (
          <div>
            <h3 className="mb-2 text-xs font-medium text-zinc-500">{t("harness.configuredProviders")}</h3>
            <div className="space-y-1">
              {keys.map((keyEntry) => {
                const provider = dynamicProviders.find((p) => p.id === keyEntry.provider);
                const providerName = provider?.name || keyEntry.provider;
                const isLocal = keyEntry.local || isLocalProvider(keyEntry.provider);
                const isCustom = keyEntry.custom || provider?.custom;

                return (
                  <div key={keyEntry.provider} className="rounded-md border border-zinc-200 px-3 py-1.5 dark:border-zinc-700">
                    <div className="flex items-center justify-between gap-2">
                      <span className="shrink-0 text-xs font-medium">{providerName}</span>
                      <div className="flex items-center gap-2 shrink-0">
                        <Tooltip content={config?.default_provider === keyEntry.provider ? t("harness.defaultProvider") : t("harness.setDefaultProvider")} variant="plain">
                          <button
                            onClick={() => handleSetDefaultProvider(keyEntry.provider)}
                            className={cn(
                              "rounded p-0.5",
                              config?.default_provider === keyEntry.provider
                                ? "text-amber-500"
                                : "text-zinc-400 hover:text-amber-500 dark:hover:text-amber-400",
                            )}
                          >
                            <Star className="h-3.5 w-3.5" />
                          </button>
                        </Tooltip>
                        <span className="text-xs" style={{ color: "var(--color-accent)" }}>{t("harness.active")}</span>
                        {isCustom ? (
                          <Tooltip content={t("harness.customProviderNoKey")} variant="plain">
                            <span className="rounded bg-blue-100 px-1.5 py-0.5 text-xs text-blue-700 dark:bg-blue-900/30 dark:text-blue-400">
                              🔧 {t("harness.custom")}
                            </span>
                          </Tooltip>
                        ) : isLocal ? (
                          <Tooltip content={t("harness.localProviderNoKey")} variant="plain">
                            <span className="rounded bg-zinc-100 px-1.5 py-0.5 text-xs text-zinc-600 dark:bg-zinc-700 dark:text-zinc-400">
                              🏠 {t("harness.local")}
                            </span>
                          </Tooltip>
                        ) : (
                          <span className="text-xs text-zinc-400">{t("harness.key")}: {keyEntry.key_preview}</span>
                        )}
                        <button
                          onClick={() => handleEdit(keyEntry.provider)}
                          className="rounded btn-solid px-2 py-0.5 text-xs"
                        >
                          {t("harness.edit")}
                        </button>
                        <button
                          onClick={() => handleRemove(keyEntry.provider)}
                          className="rounded btn-solid px-2 py-0.5 text-xs"
                        >
                          {t("harness.remove")}
                        </button>
                      </div>
                    </div>
                    <div className="mt-1 flex flex-wrap items-center gap-x-2 gap-y-1">
                      {keyEntry.models?.length ? (
                        <span className="text-xs text-zinc-600 dark:text-zinc-400">{keyEntry.models.join(", ")}</span>
                      ) : keyEntry.default_model ? (
                        <span className="text-xs text-zinc-600 dark:text-zinc-400">{keyEntry.default_model}</span>
                      ) : (
                        <span className="text-xs text-zinc-400">—</span>
                      )}
                      {keyEntry.compact_model && (
                        <Tooltip content={t("harness.compactModelHint")} variant="plain">
                          <span className="rounded bg-zinc-100 px-1.5 py-0.5 text-xs text-zinc-600 dark:bg-zinc-700 dark:text-zinc-400">
                            {t("harness.compact")}: {keyEntry.compact_model}
                          </span>
                        </Tooltip>
                      )}
                    </div>
                  </div>
                );
              })}
            </div>
          </div>
        )}

      </div>

      <div className="rounded-md border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">

        {/* Available Providers — uses shared ProviderPicker component */}
        <div>
          <div className="mb-2 flex items-center">
            <h3 className="shrink-0 text-xs font-medium text-zinc-500">
              {t("harness.availableProviders")}
            </h3>
          </div>

          <ProviderPicker
            providers={dynamicProviders}
            keys={keys}
            onConnect={(providerId, entry) => {
              setAddFlowProvider(providerId);
              setAddFlowEntry(entry);
              setShowAddFlow(true);
            }}
            onAddCustom={() => {
              setAddFlowProvider(undefined);
              setAddFlowEntry(undefined);
              setShowAddFlow(true);
            }}
          />
        </div>
      </div>

      {/* Add Provider Flow dialog (picker → add / custom) */}
      <AddProviderFlow
        open={showAddFlow}
        initialStep={addFlowProvider ? "add" : "custom"}
        initialProvider={addFlowProvider}
        initialProviderEntry={addFlowEntry}
        onClose={() => {
          setShowAddFlow(false);
          setAddFlowProvider(undefined);
          setAddFlowEntry(undefined);
        }}
        onSuccess={async () => {
          await fetchKeys();
          await fetchConfig();
          await loadProviders();
        }}
      />

      {/* Edit key dialog */}
      {showEditDialog && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50">
          <div className="w-[440px] max-h-[85vh] overflow-y-auto rounded-md bg-white p-6 shadow-xl dark:bg-zinc-800">
            <h3 className="mb-3 text-sm font-semibold">{t("harness.editProvider")} {showEditDialog}</h3>

            <div className="space-y-2">
              {!isLocalProvider(showEditDialog) && (
                <div>
                  <label className="mb-1 block text-xs text-zinc-500">{t("harness.apiKey")}</label>
                  <StyledInput
                    type="password"
                    value={editKey}
                    onChange={(e) => setEditKey(e.target.value)}
                    placeholder={t("harness.enterNewApiKey")}
                  />
                </div>
              )}

              {(
                <div>
                  <label className="mb-1 block text-xs text-zinc-500">{t("harness.baseUrl")}</label>
                  <StyledInput
                    type="text"
                    value={editBaseUrl}
                    onChange={(e) => setEditBaseUrl(e.target.value)}
                    placeholder="https://..."
                    fontMono
                  />
                </div>
              )}

              {/* Model selection */}
              <div>
                <label className="mb-1 block text-xs text-zinc-500">
                  {t("harness.defaultModel")} {editModels.length > 0 && <span className="text-accent-green">({editModels.length} {t("harness.selected")})</span>}
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
                    🔧 {t("harness.toolCalling")}
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
                    🧠 {t("harness.reasoning")}
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
                    🖼️ {t("harness.image")}
                  </button>
                </div>

                {editModels.length > 0 && (
                  <div className="mb-1 flex flex-wrap gap-1">
                    {editModels.map((m) => (
                      <span key={m} className="inline-flex items-center gap-1 rounded bg-accent-green/10 px-2 py-0.5 text-xs text-accent-green">
                        {m}
                        <button onClick={() => toggleEditModel(m)} className="text-accent-green/60 hover:text-accent-green">×</button>
                      </span>
                    ))}
                  </div>
                )}
                <StyledInput
                  type="text"
                  value={editModelSearchTerm}
                  onChange={(e) => setEditModelSearchTerm(e.target.value)}
                  placeholder={t("harness.searchModels")}
                />
                <div className="mt-1 max-h-40 overflow-y-auto rounded border border-zinc-200 dark:border-zinc-700">
                  {editModelsLoading ? (
                    <div className="px-3 py-2 text-xs text-zinc-400">{t("harness.loadingModels")}</div>
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
                            onChange={() => toggleEditModel(m.id)}
                            className="accent-[var(--color-accent)]"
                          />
                          <div className="flex flex-1 flex-col gap-0.5">
                            <span className="truncate">{m.name || m.id}</span>
                            <div className="flex gap-2 text-xs text-zinc-400">
                              {m.context_window && (
                                <span>{(m.context_window / 1000).toFixed(0)}K {t("harness.context")}</span>
                              )}
                              {m.max_tokens && (
                                <span>{(m.max_tokens / 1000).toFixed(1)}K {t("harness.maxOutput")}</span>
                              )}
                              {m.reasoning && <span>🧠 {t("harness.reasoning")}</span>}
                              {m.tool_call && <span>🔧 {t("harness.tools")}</span>}
                              {m.input_modalities?.includes('image') && <span>🖼️ {t("harness.image")}</span>}
                            </div>
                          </div>
                        </label>
                      ))
                  )}
                </div>
                <div className="mt-2 flex gap-1">
                  <StyledInput
                    type="text"
                    placeholder={t("harness.customModelPlaceholder")}
                    className="flex-1"
                    onKeyDown={(e) => {
                      if (e.key === "Enter") {
                        const val = (e.target as HTMLInputElement).value.trim();
                        if (val && !editModels.includes(val)) {
                          setEditModels([...editModels, val]);
                          setEditModelCaps(prev => ({ ...prev, [val]: makeDefaultCaps(undefined) }));
                          (e.target as HTMLInputElement).value = "";
                        }
                      }
                    }}
                  />
                </div>
              </div>

              {/* Per-model capabilities (local/custom providers only — remote uses offline data) */}
              {(() => {
                const editKeyEntry = keys.find(k => k.provider === showEditDialog);
                const editIsLocal = showEditDialog ? isLocalProvider(showEditDialog) : false;
                const editIsCustom = editKeyEntry?.custom ?? false;
                if (!(editIsLocal || editIsCustom) || editModels.length === 0) return null;
                return (
                  <div>
                    <label className="mb-1 block text-xs text-zinc-500">
                      {t("harness.modelCapabilities")}
                      <span className="ml-1 text-xs text-amber-500">({t("harness.manualInputRequired")})</span>
                    </label>
                    <div className="space-y-1">
                      {editModels.map(modelId => {
                        const caps = editModelCaps[modelId];
                        if (!caps) return null;
                        return (
                          <ModelCapEditor
                            key={modelId}
                            modelId={modelId}
                            caps={caps}
                            expanded={editExpandedModels.has(modelId)}
                            onToggle={() => setEditExpandedModels(prev => {
                              const next = new Set(prev);
                              if (next.has(modelId)) next.delete(modelId);
                              else next.add(modelId);
                              return next;
                            })}
                            onUpdate={(field, value) => updateEditModelCap(modelId, field, value)}
                          />
                        );
                      })}
                    </div>
                  </div>
                );
              })()}

              {/* Compact model for LLM summarization */}
              {editModels.length > 0 && (
                <div>
                  <label className="mb-1 block text-xs text-zinc-500">
                    {t("harness.compactModel")}
                  </label>
                  <select
                    value={editCompactModel}
                    onChange={(e) => setEditCompactModel(e.target.value)}
                    className="w-full rounded-md border border-zinc-200 px-3 py-2 text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                  >
                    <option value="">{t("harness.useCurrentModel")}</option>
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
                {t("common.cancel")}
              </button>
              <button
                onClick={handleEditSave}
                className="w-20 rounded-md bg-zinc-200 px-3 py-1.5 text-xs font-medium text-center text-zinc-800 hover:bg-zinc-300 disabled:opacity-50 dark:bg-zinc-700 dark:hover:bg-zinc-600"
              >
                {t("harness.save")}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

/** MCP tab — placeholder, content TBD */
const MCP_ICON_MAP: Record<string, React.ComponentType<{ className?: string }>> = {
  Monitor, Search, Globe, BookOpen, FileText, PenTool,
};

function McpTab() {
  const { t } = useTranslation();
  const { catalog, loading, error, loadCatalog, addServer, removeServer, probeServer, probeByName,
    healthStatus, healthErrors, healthToolCounts } = useMcpStore();
  const [showAddForm, setShowAddForm] = useState(false);

  // Probe-before-add state
  const [pendingConfig, setPendingConfig] = useState<McpServerConfigDef | null>(null);
  const [probeResult, setProbeResult] = useState<{ success: boolean; tool_count: number; tools: string[]; error: string | null; duration_ms: number } | null>(null);

  const presetIconMap = useMemo(() => {
    const map: Record<string, string> = {};
    for (const p of MCP_PRESETS) {
      map[p.id] = p.icon ?? "";
    }
    return map;
  }, []);

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

  const handleAddFromPreset = async (preset: McpPresetDef) => {
    if (preset.requiredEnv.length > 0) {
      // Show env form for API keys
      setActivePreset(preset);
      setPresetEnvForm(
        preset.requiredEnv.reduce((acc, key) => ({ ...acc, [key]: "" }), {})
      );
    } else {
      // No API key needed, probe first then add
      const config = presetToServerConfig(preset);
      setPendingConfig(config);
      setProbeResult(null);
      const result = await probeServer(config);
      setProbeResult(result);
      if (result.success) {
        addServer(config);
        setPendingConfig(null);
      }
    }
  };

  const handlePresetEnvSubmit = async () => {
    if (!activePreset) return;
    const config = presetToServerConfig(activePreset, presetEnvForm);
    setPendingConfig(config);
    setProbeResult(null);
    setActivePreset(null);
    setPresetEnvForm({});
    const result = await probeServer(config);
    setProbeResult(result);
    if (result.success) {
      addServer(config);
      setPendingConfig(null);
    }
  };

  const handleAddManual = async () => {
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
    setPendingConfig(config);
    setProbeResult(null);
    setShowAddForm(false);
    const result = await probeServer(config);
    setProbeResult(result);
    if (result.success) {
      addServer(config);
      setPendingConfig(null);
      setNewName(""); setNewCommand(""); setNewArgs(""); setNewUrl(""); setNewEnv("");
    }
  };

  /** Add the pending config despite probe failure */
  const handleAddAnyway = () => {
    if (!pendingConfig) return;
    addServer(pendingConfig);
    setPendingConfig(null);
    setProbeResult(null);
  };

  /** Dismiss probe result without adding */
  const dismissProbe = () => {
    setPendingConfig(null);
    setProbeResult(null);
  };

  return (
    <div className="max-w-2xl space-y-4">
      {/* Catalog servers */}
      <div className="rounded-md border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <div className="flex items-center justify-between">
          <h2 className="text-xs font-medium">{t("harnessMcp.mcpServerCatalog")}</h2>
          <button
            onClick={() => setShowAddForm(true)}
            className="inline-flex items-center gap-1 rounded btn-solid px-2 py-1 text-[11px] font-medium"
          >
            {t("harnessMcp.addServer")}
          </button>
        </div>

        {error && (
          <div className="mt-2">
            <ErrorBox message={error} onClose={() => useMcpStore.setState({ error: null })} />
          </div>
        )}

        {loading && catalog.length === 0 && (
          <p className="mt-3 text-xs text-zinc-400">{t("harnessMcp.loadingCatalog")}</p>
        )}

        {!loading && catalog.length === 0 && (
          <p className="mt-3 text-xs text-zinc-400">
            {t("harnessMcp.noMcpServers")}
          </p>
        )}

        {/* Server list */}
        {catalog.length > 0 && (
          <div className="mt-3 space-y-2">
            {catalog.map((server) => {
              const status = healthStatus[server.name];
              const healthErr = healthErrors[server.name];
              const toolCount = healthToolCounts[server.name];
              return (
                <div
                  key={server.name}
                  className="rounded border border-zinc-100 px-3 py-2 dark:border-zinc-600"
                >
                  <div className="flex items-start justify-between">
                    <div className="flex items-center gap-2 min-w-0">
                      {/* Health indicator dot */}
                      {status === "probing" && (
                        <span className="h-2 w-2 shrink-0 rounded-full bg-amber-400 animate-pulse" title={t("harnessMcp.testing")} />
                      )}
                      {status === "healthy" && (
                        <span className="h-2 w-2 shrink-0 rounded-full bg-green-500" title={t("harnessMcp.connected", { count: toolCount })} />
                      )}
                      {status === "unhealthy" && (
                        <span className="h-2 w-2 shrink-0 rounded-full bg-red-500" title={healthErr || t("harnessMcp.connFailed")} />
                      )}
                      <span className="rounded bg-zinc-100 px-1.5 py-0.5 text-[10px] font-mono text-zinc-500 dark:bg-zinc-700 shrink-0">
                        {server.transport}
                      </span>
                      {(() => {
                        const iconName = presetIconMap[server.name];
                        const Icon = iconName ? MCP_ICON_MAP[iconName] : undefined;
                        return Icon ? <Icon className="h-3.5 w-3.5 shrink-0 text-zinc-500" /> : null;
                      })()}
                      <span className="text-xs font-medium truncate">{server.name}</span>
                      {server.has_secrets && (
                        <span className="text-[10px] text-amber-500 shrink-0">{t("harnessMcp.hasApiKey")}</span>
                      )}
                      {status === "healthy" && toolCount > 0 && (
                        <span className="text-[10px] text-green-500 shrink-0">{toolCount} tools</span>
                      )}
                    </div>
                    <div className="flex items-center gap-2 shrink-0">
                      <button
                        onClick={() => probeByName(server.name)}
                        disabled={status === "probing"}
                        className="inline-flex items-center gap-1 rounded btn-solid px-2 py-1 text-[11px] font-medium disabled:opacity-50"
                      >
                        {status === "probing" ? "..." : t("harnessMcp.testConn")}
                      </button>
                      <button
                        onClick={() => removeServer(server.name)}
                        className="inline-flex items-center gap-1 rounded btn-solid px-2 py-1 text-[11px] font-medium"
                      >
                        {t("harnessMcp.remove")}
                      </button>
                    </div>
                  </div>
                  {(server.command || server.url) && (
                    <p className="mt-1 text-[10px] text-zinc-400 break-all">
                      {server.command || server.url}
                    </p>
                  )}
                  {/* Show health error inline */}
                  {status === "unhealthy" && healthErr && (
                    <div className="mt-1">
                      <ErrorBox message={healthErr} className="!p-2 !text-[10px]" />
                    </div>
                  )}
                </div>
              );
            })}
          </div>
        )}
      </div>

      {/* Presets gallery — always visible */}
      <div className="rounded-md border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <h2 className="text-xs font-medium mb-3">{t("harnessMcp.recommendedMcpServers")}</h2>
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
                    <span className="flex items-center gap-1">
                      {(() => {
                        const Icon = MCP_ICON_MAP[preset.icon ?? ""];
                        return Icon ? <Icon className="h-3.5 w-3.5 shrink-0" /> : null;
                      })()}
                      <span className="text-xs font-medium">{preset.name}</span>
                    </span>
                    <span className="ml-1.5 rounded bg-zinc-100 px-1 py-0.5 text-[10px] text-zinc-400 dark:bg-zinc-700">
                      {preset.category}
                    </span>
                  </div>
                  {isInstalled ? (
                    <span className="text-[10px] text-green-500">{t("harnessMcp.installed")}</span>
                  ) : (
                    <button
                      onClick={() => handleAddFromPreset(preset)}
                      className="inline-flex items-center gap-1 rounded btn-solid px-2 py-0.5 text-[10px] font-medium"
                    >
                      {t("harnessMcp.add")}
                    </button>
                  )}
                </div>
                <p className="mt-1 text-[10px] text-zinc-400 line-clamp-2">
                  {preset.description}
                </p>
                {preset.requiredEnv.length > 0 && !isInstalled && (
                  <p className="mt-1 text-[10px] text-amber-500">
                    {t("harnessMcp.requires")}{preset.requiredEnv.join(", ")}
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
          <div className="w-[440px] max-h-[85vh] overflow-y-auto rounded-md bg-white p-6 shadow-xl dark:bg-zinc-800">
            <h3 className="mb-3 text-sm font-semibold">{t("harnessMcp.addCustomMcpServer")}</h3>
            <div className="space-y-2">
              <div>
                <label className="mb-1 block text-xs text-zinc-500">{t("harnessMcp.name")}</label>
                <input
                  value={newName}
                  onChange={(e) => setNewName(e.target.value)}
                  className={inputBase}
                  placeholder="my-server"
                />
              </div>
              <div>
                <label className="mb-1 block text-xs text-zinc-500">{t("harnessMcp.transport")}</label>
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
                    <label className="mb-1 block text-xs text-zinc-500">{t("harnessMcp.command")}</label>
                    <input
                      value={newCommand}
                      onChange={(e) => setNewCommand(e.target.value)}
                      className={inputBase}
                      placeholder="npx"
                    />
                  </div>
                  <div>
                    <label className="mb-1 block text-xs text-zinc-500">{t("harnessMcp.arguments")}</label>
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
                  <label className="mb-1 block text-xs text-zinc-500">{t("harnessMcp.url")}</label>
                  <input
                    value={newUrl}
                    onChange={(e) => setNewUrl(e.target.value)}
                    className={inputBase}
                    placeholder="http://localhost:3000"
                  />
                </div>
              )}
              <div>
                <label className="mb-1 block text-xs text-zinc-500">{t("harnessMcp.environment")}</label>
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
                className="inline-flex items-center gap-1 rounded-md border border-zinc-300 px-3 py-1.5 text-xs font-medium text-zinc-700 hover:bg-zinc-50 dark:border-zinc-600 dark:text-zinc-300 dark:hover:bg-zinc-700"
              >
                {t("common.cancel")}
              </button>
              <button
                onClick={handleAddManual}
                disabled={!newName.trim()}
                className="inline-flex items-center gap-1 rounded btn-accent px-3 py-1.5 text-xs font-medium disabled:opacity-50"
              >
                {t("harnessMcp.addServer")}
              </button>
            </div>
          </div>
        </div>
      )}

      {/* Preset env form (for servers requiring API keys) */}
      {activePreset && (
        <div className="rounded-md border border-[var(--color-accent)]/40 bg-white p-4 dark:bg-zinc-800">
          <h2 className="text-xs font-medium mb-1">{t("harnessMcp.configure")}{activePreset.name}</h2>
          <p className="text-[10px] text-zinc-400 mb-3">{activePreset.installHint}</p>
          <div className="space-y-2">
            {activePreset.requiredEnv.map((envKey) => (
              <div key={envKey}>
                <label className="mb-1 block text-[10px] text-zinc-400">{envKey}</label>
                <input
                  type="password"
                  value={presetEnvForm[envKey] || ""}
                  onChange={(e) =>
                    setPresetEnvForm((prev) => ({ ...prev, [envKey]: e.target.value }))
                  }
                  className={inputBase}
                  placeholder={`${t("harnessMcp.enter")}${envKey}`}
                />
              </div>
            ))}
            <div className="flex gap-2">
              <button
                onClick={handlePresetEnvSubmit}
                className="inline-flex items-center gap-1 rounded btn-accent px-3 py-1 text-xs font-medium"
              >
                {t("harnessMcp.addServer")}
              </button>
              <button
                onClick={() => { setActivePreset(null); setPresetEnvForm({}); }}
                className="inline-flex items-center gap-1 rounded-md border border-zinc-300 px-3 py-1 text-xs font-medium text-zinc-700 hover:bg-zinc-50 dark:border-zinc-600 dark:text-zinc-300 dark:hover:bg-zinc-700"
              >
                {t("common.cancel")}
              </button>
            </div>
          </div>
        </div>
      )}

      {/* Probe result dialog */}
      {probeResult && pendingConfig && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50">
          <div className="w-[400px] rounded-md bg-white p-6 shadow-xl dark:bg-zinc-800">
            {probeResult.success ? (
              <>
                <div className="flex items-center gap-2 mb-3">
                  <span className="h-3 w-3 rounded-full bg-green-500" />
                  <h3 className="text-sm font-semibold text-green-600 dark:text-green-400">
                    {t("harnessMcp.connected", { count: probeResult.tool_count })}
                  </h3>
                </div>
                {probeResult.tools.length > 0 && (
                  <div className="mb-3 max-h-32 overflow-y-auto rounded bg-zinc-50 p-2 dark:bg-zinc-700/50">
                    {probeResult.tools.map((tool) => (
                      <span key={tool} className="mr-1 mb-1 inline-block rounded bg-zinc-100 px-1.5 py-0.5 text-[10px] font-mono text-zinc-600 dark:bg-zinc-600 dark:text-zinc-300">
                        {tool}
                      </span>
                    ))}
                  </div>
                )}
                <p className="text-[10px] text-zinc-400 mb-3">{probeResult.duration_ms}ms</p>
                <button
                  onClick={dismissProbe}
                  className="inline-flex items-center gap-1 rounded btn-accent px-3 py-1.5 text-xs font-medium"
                >
                  OK
                </button>
              </>
            ) : (
              <>
                <div className="flex items-center gap-2 mb-3">
                  <span className="h-3 w-3 rounded-full bg-red-500" />
                  <h3 className="text-sm font-semibold text-red-600 dark:text-red-400">
                    {t("harnessMcp.connFailed")}
                  </h3>
                </div>
                <div className="mb-3">
                  <ErrorBox
                    message={t("harnessMcp.connFailed")}
                    details={probeResult.error ?? undefined}
                  />
                </div>
                <p className="text-[10px] text-zinc-400 mb-3">{probeResult.duration_ms}ms</p>
                <div className="flex gap-2">
                  <button
                    onClick={handleAddAnyway}
                    className="inline-flex items-center gap-1 rounded-md border border-amber-400 px-3 py-1.5 text-xs font-medium text-amber-600 hover:bg-amber-50 dark:border-amber-600 dark:text-amber-400 dark:hover:bg-amber-900/20"
                  >
                    {t("harnessMcp.addAnyway")}
                  </button>
                  <button
                    onClick={dismissProbe}
                    className="inline-flex items-center gap-1 rounded-md border border-zinc-300 px-3 py-1.5 text-xs font-medium text-zinc-700 hover:bg-zinc-50 dark:border-zinc-600 dark:text-zinc-300 dark:hover:bg-zinc-700"
                  >
                    {t("common.cancel")}
                  </button>
                </div>
              </>
            )}
          </div>
        </div>
      )}

      {/* Probing spinner overlay (shown while probe is in progress) */}
      {pendingConfig && !probeResult && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/30">
          <div className="w-[300px] rounded-md bg-white p-6 text-center shadow-xl dark:bg-zinc-800">
            <div className="mx-auto mb-3 h-8 w-8 animate-spin rounded-full border-2 border-zinc-300 border-t-[var(--color-accent)]" />
            <p className="text-xs text-zinc-500">{t("harnessMcp.testing")}</p>
            <p className="mt-1 text-[10px] text-zinc-400">{pendingConfig.name}</p>
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