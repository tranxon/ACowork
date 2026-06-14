import { useState, useEffect, useCallback, useMemo } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { VaultKeyEntry, ModelInfo, ModelCapabilitiesMap, ProviderListEntry, McpServerConfigDef, McpTransportDef, McpPresetDef } from "../../lib/types";
import { cn } from "../../lib/utils";
import { inputBase, selectBase } from "../../lib/ui-styles";
import { StyledInput } from "../common/StyledInput";
import { needsApiKey, keyPlaceholder, isLocalProvider } from "../../lib/providers";
import { fetchProviderModels } from "../../lib/gateway-api";
import { getGatewayUrl } from "../../lib/config";
import { Monitor, Search, Globe, BookOpen, FileText, PenTool, Star, ChevronsDown } from "lucide-react";
import { useMcpStore } from "../../stores/mcpStore";
import { MCP_PRESETS, presetToServerConfig } from "../../lib/mcp-presets";
import { SearchTab } from "./SearchTab";
import { EmbeddingModelTab } from "./EmbeddingModelTab";
import { useTranslation } from "../../i18n/useTranslation";
import { Tooltip } from "../common/Tooltip";
import { TabButton } from "../common/tab";

type HarnessTab = "providers" | "search" | "mcp" | "embedding";

export function HarnessPage() {
  const { t } = useTranslation();
  const [activeTab, setActiveTab] = useState<HarnessTab>("providers");

  const tabs: { id: HarnessTab; label: string }[] = [
    { id: "providers", label: t("harness.tabProviders") },
    { id: "search", label: t("harness.tabSearch") },
    { id: "mcp", label: t("harness.tabMcp") },
    { id: "embedding", label: t("harness.tabEmbedding") },
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
      </div>
    </div>
  );
}

/** Provider configuration */
function ProvidersTab() {
  const { t } = useTranslation();
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

  // Collapsible remote providers section (folded by default when any local provider is configured)
  const [showAllRemote, setShowAllRemote] = useState(false);

  // Search term for filtering available providers (both local and remote)
  const [providerSearchTerm, setProviderSearchTerm] = useState("");

  // Split providers into local / remote for UI grouping
  const { localProviders, remoteProviders } = useMemo(() => {
    const local: ProviderListEntry[] = [];
    const remote: ProviderListEntry[] = [];
    for (const p of dynamicProviders) {
      if (p.local || isLocalProvider(p.id)) {
        local.push(p);
      } else {
        remote.push(p);
      }
    }
    return { localProviders: local, remoteProviders: remote };
  }, [dynamicProviders]);

  // Filter remote providers by search term (match name or id)
  const filteredRemoteProviders = useMemo(() => {
    if (!providerSearchTerm.trim()) return remoteProviders;
    const term = providerSearchTerm.toLowerCase().trim();
    return remoteProviders.filter(p =>
      p.name?.toLowerCase().includes(term) ||
      p.id.toLowerCase().includes(term)
    );
  }, [remoteProviders, providerSearchTerm]);

  // Derived: is the current "add" target a local provider?
  const newProviderIsLocal = useMemo(
    () => isLocalProvider(newProvider),
    [newProvider]
  );


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

  const handleAdd = async () => {
    // First test the API key (skip for local providers which don't need keys)
    if (!newProviderIsLocal && needsApiKey(newProvider) && !newKey.trim()) {
      setTestResult({ success: false, message: t("harness.pleaseEnterApiKey") });
      return;
    }

    // For local providers, skip the key test and save directly
    if (newProviderIsLocal) {
      setTesting(true);
      try {
        // Build per-model capabilities map
        const modelCapabilities: ModelCapabilitiesMap = {};
        if (newModels.length > 0) {
          for (const modelId of newModels) {
            const mi = availableModels.find(m => m.id === modelId);
            modelCapabilities[modelId] = {
              context_window: mi?.context_window ?? 128000,
              max_output_tokens: mi?.max_tokens ?? 4096,
              supports_tool_calling: mi?.tool_call ?? newSupportsToolCalling,
              supports_reasoning: mi?.reasoning ?? undefined,
              modalities: mi?.input_modalities?.length ? { input: mi.input_modalities } : undefined,
            };
          }
        }
        await invoke("add_key", {
          provider: newProvider,
          key: "",  // local providers don't need a key; Gateway will fill placeholder
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
        alert(`${t("harness.failedConnectLocal")}: ${e}`);
      }
      setTesting(false);
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

      setTestResult({ success: true, message: t("harness.apiKeyValid") });

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

    // Build per-model capabilities map
    const modelCapabilities: ModelCapabilitiesMap = {};
    if (newModels.length > 0) {
      const maxOutTokens = effectiveMaxOutputTokens ? parseInt(effectiveMaxOutputTokens) : 4096;
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
      alert(`${t("harness.failedAddKey")}: ${e}`);
    }
  };

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
    setEditModels(keyEntry?.models?.length ? keyEntry.models : keyEntry?.default_model ? [keyEntry.default_model] : []);
    setEditModelSearchTerm("");
    setEditModelCapabilityFilter([]);
    // Load existing capabilities from VaultKeyEntry (per-model map, take first entry)
    const firstCaps = keyEntry?.model_capabilities ? Object.values(keyEntry.model_capabilities)[0] : undefined;
    // Use defaults (128000/4096) when capabilities are 0 or missing — this
    // handles old Vault entries that saved 0 before the fallback was fixed
    const vaultCtx = firstCaps?.context_window;
    const vaultMot = firstCaps?.max_output_tokens;
    setEditContextWindow(vaultCtx && vaultCtx > 0 ? vaultCtx.toString() : "128000");
    setEditMaxOutputTokens(vaultMot && vaultMot > 0 ? vaultMot.toString() : "4096");
    setEditSupportsToolCalling(firstCaps?.supports_tool_calling ?? true);
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
          alert(t("harness.contextWindowMaxOutputPositive"));
          return;
        }
        const caps: ModelCapabilitiesMap = {};
        for (const modelId of editModels) {
          const modelInfo = editAvailableModels.find(m => m.id === modelId);
          caps[modelId] = {
            context_window: cw || 128000,
            max_output_tokens: mot || 4096,
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
      alert(`${t("harness.failedUpdateKey")}: ${e}`);
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

                return (
                  <div key={keyEntry.provider} className="rounded-lg border border-zinc-200 px-3 py-1.5 dark:border-zinc-700">
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
                        {isLocal ? (
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
                          className="rounded-md btn-solid px-2 py-0.5 text-xs"
                        >
                          {t("harness.edit")}
                        </button>
                        <button
                          onClick={() => handleRemove(keyEntry.provider)}
                          className="rounded-md btn-solid px-2 py-0.5 text-xs"
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

      <div className="rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">

        {/* Available Providers — split into Local / Remote sections */}
        <div>
          <div className="mb-2 flex items-center">
            <h3 className="shrink-0 text-xs font-medium text-zinc-500">
              {t("harness.availableProviders")}
            </h3>
          </div>

          {dynamicProviders.length === 0 ? (
            <div className="py-3 text-center text-xs text-zinc-400">{t("harness.noProvidersAvailable")}</div>
          ) : (
            <div className="space-y-3">
              {/* ── Local Providers (always visible) ── */}
              {localProviders.length > 0 && (
                <div>
                  <h4 className="mb-1.5 text-xs font-medium text-zinc-500 dark:text-zinc-400">🏠 {t("harness.localProviders")}</h4>
                  <div className="space-y-1">
                    {localProviders.map((item) => {
                      const providerId = item.id;
                      const providerName = item.name || providerId;
                      const keyEntry = keys.find((k) => k.provider === providerId);
                      if (keyEntry) return null;
                      return (
                        <div key={providerId} className="rounded-lg border border-zinc-200 px-3 py-1.5 dark:border-zinc-700">
                          <div className="flex items-center justify-between">
                            <div className="min-w-0 flex-1">
                              <span className="text-xs font-medium">{providerName}</span>
                            </div>
                            <button
                              onClick={() => {
                                setNewProvider(providerId);
                                setNewBaseUrl(item.api ?? "");
                                setNewKey("");
                                fetchModels(providerId).then((models) => setAvailableModels(models));
                                setNewContextWindow("");
                                setNewMaxOutputTokens("");
                                setNewSupportsToolCalling(true);
                                setShowAddDialog(true);
                              }}
                              className="rounded-md bg-zinc-100 px-3 py-1 text-xs font-medium text-zinc-700 hover:bg-zinc-200 dark:bg-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-600"
                            >
                              {t("harness.connect")}
                            </button>
                          </div>
                        </div>
                      );
                    })}
                  </div>
                </div>
              )}

              {/* ── Remote Providers (expandable) ── */}
              {remoteProviders.length > 0 && (
                <div>
                  <div className="mb-1.5 flex items-center justify-between">
                    <span className="flex items-center gap-1 text-xs font-medium text-zinc-500 dark:text-zinc-400">
                      ☁️ {t("harness.remoteProviders")} (
                      {providerSearchTerm.trim()
                        ? `${filteredRemoteProviders.length}/${remoteProviders.filter(p => !keys.find(k => k.provider === p.id)).length}`
                        : remoteProviders.filter(p => !keys.find(k => k.provider === p.id)).length
                      }
                      {" "}{t("harness.available")})
                    </span>
                    <div className="flex items-center gap-2">
                      {/* Search filter for remote providers */}
                      <div className="relative">
                        <StyledInput
                          type="text"
                          value={providerSearchTerm}
                          onChange={(e) => setProviderSearchTerm(e.target.value)}
                          placeholder={t("harness.searchProviders")}
                          className="w-[180px] bg-white pl-7 pr-2 placeholder-zinc-400 dark:border-zinc-600 dark:bg-zinc-800 dark:placeholder-zinc-500"
                        />
                        <Search className="pointer-events-none absolute left-2 top-1/2 -translate-y-1/2 h-3.5 w-3.5 text-zinc-400" />
                      </div>
                    </div>
                  </div>
                  {filteredRemoteProviders.length > 0 && (
                    <>
                      <div className="space-y-1">
                        {(() => {
                          const hasMore = !providerSearchTerm.trim() && !showAllRemote && filteredRemoteProviders.length > 5;
                          const displayed = hasMore ? filteredRemoteProviders.slice(0, 5) : filteredRemoteProviders;
                          return displayed.map((item) => {
                            const providerId = item.id;
                            const providerName = item.name || providerId;
                            const keyEntry = keys.find((k) => k.provider === providerId);
                            const modelCount = item.model_count;
                            if (keyEntry) return null;
                            return (
                              <div key={providerId} className="rounded-lg border border-zinc-200 px-3 py-1.5 dark:border-zinc-700">
                                <div className="flex items-center justify-between">
                                  <div className="min-w-0 flex-1">
                                    <span className="text-xs font-medium">{providerName}</span>
                                    {modelCount != null ? (
                                      <span className="ml-2 text-xs text-zinc-400">{t("harness.modelsAvailable", { count: modelCount })}</span>
                                    ) : null}
                                  </div>
                                  <button
                                    onClick={() => {
                                      setNewProvider(providerId);
                                      const dynamicProvider = dynamicProviders.find((p) => p.id === providerId);
                                      setNewBaseUrl(dynamicProvider?.api ?? "");
                                      fetchModels(providerId).then((models) => setAvailableModels(models));
                                      setNewContextWindow("");
                                      setNewMaxOutputTokens("");
                                      setNewSupportsToolCalling(true);
                                      setShowAddDialog(true);
                                    }}
                                    className="rounded-md bg-zinc-100 px-3 py-1 text-xs font-medium text-zinc-700 hover:bg-zinc-200 dark:bg-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-600"
                                  >
                                    {t("harness.addKey")}
                                  </button>
                                </div>
                              </div>
                            );
                          });
                        })()}
                      </div>
                      {!providerSearchTerm.trim() && !showAllRemote && filteredRemoteProviders.length > 5 && (
                        <button
                          onClick={() => setShowAllRemote(true)}
                          className="mt-1 flex w-full items-center justify-center gap-1 rounded-lg border border-dashed border-zinc-300 py-2 text-xs text-zinc-500 transition-colors hover:border-zinc-400 hover:text-zinc-700 dark:border-zinc-600 dark:text-zinc-400 dark:hover:border-zinc-500 dark:hover:text-zinc-300"
                        >
                          <ChevronsDown className="h-4 w-4" />
                          <>Show all ({filteredRemoteProviders.length})</>
                        </button>
                      )}
                    </>
                  )}
                  {filteredRemoteProviders.length === 0 && providerSearchTerm.trim() && (
                    <div className="py-3 text-center text-xs text-zinc-400">
                      {t("harness.noProvidersMatch")}
                    </div>
                  )}
                </div>
              )}
            </div>
          )}
        </div>
      </div>

      {/* Add key dialog */}
      {showAddDialog && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50">
          <div className="w-[440px] max-h-[85vh] overflow-y-auto rounded-lg bg-white p-6 shadow-xl dark:bg-zinc-800">
            <h3 className="mb-3 text-sm font-semibold">
              {newProviderIsLocal ? t("harness.connectLocalProvider") + " " : t("harness.addApiKey") + " "}
              {dynamicProviders.find((p) => p.id === newProvider)?.name || newProvider}
            </h3>

            <div className="space-y-2">
              {/* Provider display (read-only) */}
              <div>
                <label className="mb-1 block text-xs text-zinc-500">{t("harness.provider")}</label>
                <div className="w-full rounded-md border border-zinc-200 bg-zinc-50 px-3 py-2 text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200">
                  {dynamicProviders.find((p) => p.id === newProvider)?.name || newProvider}
                </div>
              </div>

              {needsApiKey(newProvider) && (
                <div>
                  <label className="mb-1 block text-xs text-zinc-500">{t("harness.apiKey")}</label>
                  <StyledInput
                    type="password"
                    value={newKey}
                    onChange={(e) => setNewKey(e.target.value)}
                    placeholder={keyPlaceholder(newProvider)}
                  />
                </div>
              )}

              {(() => {
                return (
                  <div>
                    <label className="mb-1 block text-xs text-zinc-500">{t("harness.baseUrl")}</label>
                    <StyledInput
                      type="text"
                      value={newBaseUrl}
                      onChange={(e) => setNewBaseUrl(e.target.value)}
                      placeholder="https://..."
                      fontMono
                    />
                  </div>
                )
              })()}

              {/* Model selection (multi-select) */}
              <div>
                <label className="mb-1 block text-xs text-zinc-500">
                  {t("harness.defaultModel")} {newModels.length > 0 && <span className="text-accent-green">({newModels.length} {t("harness.selected")})</span>}
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
                    🔧 {t("harness.toolCalling")}
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
                    🧠 {t("harness.reasoning")}
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
                    🖼️ {t("harness.image")}
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
                <StyledInput
                  type="text"
                  value={modelSearchTerm}
                  onChange={(e) => setModelSearchTerm(e.target.value)}
                  placeholder={t("harness.searchModels")}
                />
                <div className="mt-1 max-h-40 overflow-y-auto rounded border border-zinc-200 dark:border-zinc-700">
                  {modelsLoading ? (
                    <div className="px-3 py-2 text-xs text-zinc-400">{t("harness.loadingModels")}</div>
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
                  {!modelsLoading && availableModels.length === 0 && (
                    <div className="px-3 py-2 text-xs text-zinc-400">{t("harness.noModelsFound")}</div>
                  )}
                </div>
                {/* Manual model input */}
                <div className="mt-2 flex gap-1">
                  <StyledInput
                    type="text"
                    placeholder={t("harness.customModelPlaceholder")}
                    className="flex-1"
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
                      {t("harness.modelCapabilities")}
                      {hasModelsDevData && <span className="ml-1 text-xs text-zinc-400">({t("harness.fromModelsDev")})</span>}
                      {!hasModelsDevData && <span className="ml-1 text-xs text-amber-500">({t("harness.manualInputRequired")})</span>}
                    </label>
                    <div className="flex gap-2">
                      <div className="flex-1">
                        <label className="mb-0.5 block text-xs text-zinc-400">{t("harness.contextWindow")}</label>
                        <StyledInput
                          type="number"
                          value={displayContextWindow}
                          onChange={(e) => setNewContextWindow(e.target.value)}
                          readOnly={hasModelsDevData}
                          placeholder="e.g. 128000"
                          className={cn(
                            hasModelsDevData
                              ? "bg-zinc-50 text-zinc-400 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-500"
                              : "dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200",
                          )}
                        />
                      </div>
                      <div className="flex-1">
                        <label className="mb-0.5 block text-xs text-zinc-400">{t("harness.maxOutputTokens")}</label>
                        <StyledInput
                          type="number"
                          value={displayMaxOutputTokens}
                          onChange={(e) => setNewMaxOutputTokens(e.target.value)}
                          readOnly={hasModelsDevData}
                          placeholder="e.g. 4096"
                          className={cn(
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
                        {t("harness.supportsToolCalling")}
                      </label>
                    </div>
                  </div>
                );
              })()}

              {/* Compact model for LLM summarization */}
              {newModels.length > 0 && (
                <div>
                  <label className="mb-1 block text-xs text-zinc-500">
                    {t("harness.compactModel")}
                  </label>
                  <select
                    value={newCompactModel}
                    onChange={(e) => setNewCompactModel(e.target.value)}
                    className="w-full rounded-md border border-zinc-200 px-3 py-2 text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                  >
                    <option value="">{t("harness.useCurrentModel")}</option>
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
                  <div className="text-xs text-zinc-400">{t("harness.testing")}</div>
                )}
              </div>

              {/* Buttons on the right with equal width */}
              <div className="flex gap-2 shrink-0">
                <button
                  onClick={() => { setShowAddDialog(false); setNewModels([]); setTestResult(null); }}
                  className="rounded-md px-3 py-1.5 text-xs font-medium text-zinc-600 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-700"
                >
                  {t("common.cancel")}
                </button>
                <button
                  onClick={handleAdd}
                  disabled={(needsApiKey(newProvider) ? !newKey.trim() : false) || testing}
                  className="rounded-md bg-zinc-200 px-3 py-1.5 text-xs font-medium text-zinc-800 hover:bg-zinc-300 disabled:opacity-50 dark:bg-zinc-700 dark:hover:bg-zinc-600"
                >
                  {testing ? t("harness.saving") : t("harness.save")}
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
                        <button onClick={() => setEditModels(editModels.filter((x) => x !== m))} className="text-accent-green/60 hover:text-accent-green">×</button>
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
                            onChange={() => toggleModel(m.id, editModels, setEditModels)}
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
                      {t("harness.modelCapabilities")}
                      {hasModelsDevData && <span className="ml-1 text-xs text-zinc-400">({t("harness.fromModelsDev")})</span>}
                      {!hasModelsDevData && <span className="ml-1 text-xs text-amber-500">({t("harness.manualInputRequired")})</span>}
                    </label>
                    <div className="flex gap-2">
                      <div className="flex-1">
                        <label className="mb-0.5 block text-xs text-zinc-400">{t("harness.contextWindow")}</label>
                        <StyledInput
                          type="number"
                          value={displayContextWindow}
                          onChange={(e) => setEditContextWindow(e.target.value)}
                          readOnly={hasModelsDevData}
                          placeholder="e.g. 128000"
                          className={cn(
                            hasModelsDevData
                              ? "bg-zinc-50 text-zinc-400 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-500"
                              : "dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200",
                          )}
                        />
                      </div>
                      <div className="flex-1">
                        <label className="mb-0.5 block text-xs text-zinc-400">{t("harness.maxOutputTokens")}</label>
                        <StyledInput
                          type="number"
                          value={displayMaxOutputTokens}
                          onChange={(e) => setEditMaxOutputTokens(e.target.value)}
                          readOnly={hasModelsDevData}
                          placeholder="e.g. 4096"
                          className={cn(
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
                        {t("harness.supportsToolCalling")}
                      </label>
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
  const { catalog, loading, error, loadCatalog, addServer, removeServer } = useMcpStore();
  const [showAddForm, setShowAddForm] = useState(false);

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
          <h2 className="text-xs font-medium">{t("harnessMcp.mcpServerCatalog")}</h2>
          <button
            onClick={() => setShowAddForm(true)}
            className="inline-flex items-center gap-1 rounded-md btn-solid px-2 py-1 text-[11px] font-medium"
          >
            {t("harnessMcp.addServer")}
          </button>
        </div>

        {error && (
          <p className="mt-2 text-xs text-red-500">{error}</p>
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
            {catalog.map((server) => (
              <div
                key={server.name}
                className="rounded border border-zinc-100 px-3 py-2 dark:border-zinc-600"
              >
                <div className="flex items-start justify-between">
                  <div className="flex items-center gap-2 min-w-0">
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
                  </div>
                  <button
                    onClick={() => removeServer(server.name)}
                    className="group/remove shrink-0 text-xs text-zinc-400 transition-colors hover:text-[var(--color-accent)] dark:text-zinc-500"
                  >
                    {t("harnessMcp.remove")}
                  </button>
                </div>
                {(server.command || server.url) && (
                  <p className="mt-1 text-[10px] text-zinc-400 break-all">
                    {server.command || server.url}
                  </p>
                )}
              </div>
            ))}
          </div>
        )}
      </div>

      {/* Presets gallery — always visible */}
      <div className="rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
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
                      className="inline-flex items-center gap-1 rounded-md btn-solid px-2 py-0.5 text-[10px] font-medium"
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
          <div className="w-[440px] max-h-[85vh] overflow-y-auto rounded-lg bg-white p-6 shadow-xl dark:bg-zinc-800">
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
                className="inline-flex items-center gap-1 rounded-md btn-accent px-3 py-1.5 text-xs font-medium disabled:opacity-50"
              >
                {t("harnessMcp.addServer")}
              </button>
            </div>
          </div>
        </div>
      )}

      {/* Preset env form (for servers requiring API keys) */}
      {activePreset && (
        <div className="rounded-lg border border-amber-200 bg-white p-4 dark:border-amber-700 dark:bg-zinc-800">
          <h2 className="text-xs font-medium mb-1">{t("harnessMcp.configure")}{activePreset.name}</h2>
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
                  className="rounded border border-zinc-200 px-2 py-1 dark:border-zinc-600 dark:bg-zinc-700"
                  placeholder={`${t("harnessMcp.enter")}${envKey}`}
                />
              </div>
            ))}
            <div className="flex gap-2">
              <button
                onClick={handlePresetEnvSubmit}
                className="inline-flex items-center gap-1 rounded-md btn-accent px-3 py-1 text-xs font-medium"
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
    </div>
  );
}

/** GatewayConfig type for local usage */
interface GatewayConfig {
  default_provider?: string;
  default_model?: string;
}