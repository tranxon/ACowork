import { useState, useEffect, useCallback, useMemo } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { VaultKeyEntry, ModelInfo, ModelCapabilitiesInfo, ProviderListEntry } from "../../lib/types";
import { cn } from "../../lib/utils";
import { StyledInput } from "../common/StyledInput";
import { needsApiKey, keyPlaceholder, isLocalProvider } from "../../lib/providers";
import { fetchProviderModels, discoverModels, fetchProviders } from "../../lib/gateway-api";
import { ModelCapEditor } from "./ModelCapEditor";
import { ProviderPicker } from "./ProviderPicker";
import { useTranslation } from "../../i18n/useTranslation";
import { ChevronLeft } from "lucide-react";
import { ErrorBox } from "../common/ErrorBox";

interface AddProviderFlowProps {
  open: boolean;
  onClose: () => void;
  onSuccess: () => void;
  /** Skip picker and go directly to add/custom step. */
  initialStep?: "picker" | "add" | "custom";
  /** Provider ID when initialStep="add". */
  initialProvider?: string;
  /** Provider list entry for baseUrl default when initialStep="add". */
  initialProviderEntry?: ProviderListEntry;
}

type Step = "picker" | "add" | "custom";

/** Self-contained dialog that encapsulates the entire provider-add flow:
 *  picker → add dialog (local/remote) or custom-provider dialog. */
export function AddProviderFlow({
  open,
  onClose,
  onSuccess,
  initialStep = "picker",
  initialProvider,
  initialProviderEntry,
}: AddProviderFlowProps) {
  const { t } = useTranslation();

  // ── Dialog-level state ──
  const [step, setStep] = useState<Step>(initialStep);
  const [dynamicProviders, setDynamicProviders] = useState<ProviderListEntry[]>([]);
  const [keys, setKeys] = useState<VaultKeyEntry[]>([]);

  // ── Add-dialog state ──
  const [selectedProvider, setSelectedProvider] = useState<string>(initialProvider ?? "");
  const [newKey, setNewKey] = useState("");
  const [newBaseUrl, setNewBaseUrl] = useState(initialProviderEntry?.api ?? "");
  const [newModels, setNewModels] = useState<string[]>([]);
  const [availableModels, setAvailableModels] = useState<ModelInfo[]>([]);
  const [modelsLoading, setModelsLoading] = useState(false);
  const [modelSearchTerm, setModelSearchTerm] = useState("");
  const [modelCapabilityFilter, setModelCapabilityFilter] = useState<string[]>([]);
  const [newModelCaps, setNewModelCaps] = useState<Record<string, ModelCapabilitiesInfo>>({});
  const [newExpandedModels, setNewExpandedModels] = useState<Set<string>>(new Set());
  const [newCompactModel, setNewCompactModel] = useState("");
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<{ success: boolean; message: string } | null>(null);

  // ── Custom-provider dialog state ──
  const [customProviderName, setCustomProviderName] = useState("");
  const [customProviderId, setCustomProviderId] = useState("");
  const [customBaseUrl, setCustomBaseUrl] = useState("");
  const [customApiKey, setCustomApiKey] = useState("");
  const [customModels, setCustomModels] = useState<string[]>([]);
  const [customAvailableModels, setCustomAvailableModels] = useState<ModelInfo[]>([]);
  const [customModelsLoading, setCustomModelsLoading] = useState(false);
  const [customDiscoverError, setCustomDiscoverError] = useState<string | null>(null);
  const [customModelSearchTerm, setCustomModelSearchTerm] = useState("");
  const [customTesting, setCustomTesting] = useState(false);
  const [customModelCaps, setCustomModelCaps] = useState<Record<string, ModelCapabilitiesInfo>>({});
  const [customExpandedModels, setCustomExpandedModels] = useState<Set<string>>(new Set());

  // ── Derived ──
  const selectedProviderIsLocal = useMemo(
    () => isLocalProvider(selectedProvider),
    [selectedProvider],
  );

  // ── Data fetching ──
  const fetchKeys = useCallback(async () => {
    try {
      const result = await invoke<VaultKeyEntry[]>("list_keys");
      setKeys(result);
    } catch { /* Gateway may not be running */ }
  }, []);

  const loadProviders = useCallback(async () => {
    try {
      const providers = await fetchProviders();
      setDynamicProviders(providers);
    } catch { /* Gateway may not be running */ }
  }, []);

  const fetchModels = useCallback(async (providerId: string): Promise<ModelInfo[]> => {
    try {
      const data = await fetchProviderModels(providerId);
      return data.models ?? [];
    } catch {
      return [];
    }
  }, []);

  // ── Effects ──
  // On open: fetch providers + keys, apply initialStep
  useEffect(() => {
    if (!open) return;
    fetchKeys();
    loadProviders();
    setStep(initialStep);
    if (initialProvider) {
      setSelectedProvider(initialProvider);
      setNewBaseUrl(initialProviderEntry?.api ?? "");
      setNewKey("");
      setNewModels([]);
      setNewModelCaps({});
      setNewExpandedModels(new Set());
      setTestResult(null);
      setModelsLoading(true);
      fetchModels(initialProvider).then((models) => {
        setAvailableModels(models);
        setModelsLoading(false);
      });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  // ── Helpers ──
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

  const toggleNewModel = (model: string) => {
    if (newModels.includes(model)) {
      setNewModels(newModels.filter(m => m !== model));
      const next = { ...newModelCaps };
      delete next[model];
      setNewModelCaps(next);
    } else {
      setNewModels([...newModels, model]);
      const mi = availableModels.find(m => m.id === model);
      setNewModelCaps({ ...newModelCaps, [model]: makeDefaultCaps(mi) });
    }
  };

  const toggleCustomModel = (model: string) => {
    if (customModels.includes(model)) {
      setCustomModels(customModels.filter(m => m !== model));
      const next = { ...customModelCaps };
      delete next[model];
      setCustomModelCaps(next);
    } else {
      setCustomModels([...customModels, model]);
      const mi = customAvailableModels.find(m => m.id === model);
      setCustomModelCaps({ ...customModelCaps, [model]: makeDefaultCaps(mi) });
    }
  };

  const updateNewModelCap = (modelId: string, field: keyof ModelCapabilitiesInfo, value: unknown) => {
    setNewModelCaps(prev => ({ ...prev, [modelId]: { ...prev[modelId], [field]: value } }));
  };

  const updateCustomModelCap = (modelId: string, field: keyof ModelCapabilitiesInfo, value: unknown) => {
    setCustomModelCaps(prev => ({ ...prev, [modelId]: { ...prev[modelId], [field]: value } }));
  };

  const slugifyProviderId = (name: string): string => {
    return "custom-" + name.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "");
  };

  // ── Picker → add transition ──
  const handleConnect = (providerId: string, entry: ProviderListEntry) => {
    setSelectedProvider(providerId);
    setNewBaseUrl(entry.api ?? "");
    setNewKey("");
    setNewModels([]);
    setNewModelCaps({});
    setNewExpandedModels(new Set());
    setNewCompactModel("");
    setTestResult(null);
    setModelSearchTerm("");
    setModelCapabilityFilter([]);
    setStep("add");
    setModelsLoading(true);
    fetchModels(providerId).then((models) => {
      setAvailableModels(models);
      setModelsLoading(false);
    });
  };

  // ── Picker → custom transition ──
  const handleStartCustom = () => {
    setCustomProviderName("");
    setCustomProviderId("");
    setCustomBaseUrl("");
    setCustomApiKey("");
    setCustomModels([]);
    setCustomAvailableModels([]);
    setCustomDiscoverError(null);
    setCustomModelSearchTerm("");
    setCustomModelCaps({});
    setCustomExpandedModels(new Set());
    setStep("custom");
  };

  // ── Save handlers ──
  const handleAdd = async () => {
    if (!selectedProviderIsLocal && needsApiKey(selectedProvider) && !newKey.trim()) {
      setTestResult({ success: false, message: t("harness.pleaseEnterApiKey") });
      return;
    }

    // Local providers: skip key test, save directly
    if (selectedProviderIsLocal) {
      setTesting(true);
      try {
        await invoke("add_key", {
          provider: selectedProvider,
          key: "",
          baseUrl: newBaseUrl || undefined,
          defaultModel: undefined,
          models: newModels.length > 0 ? newModels : undefined,
          modelCapabilities: newModels.length > 0 ? newModelCaps : undefined,
          compactModel: newCompactModel || undefined,
        });
        window.dispatchEvent(new CustomEvent('models-added'));
        onSuccess();
        onClose();
      } catch (e) {
        alert(`${t("harness.failedConnectLocal")}: ${e}`);
      }
      setTesting(false);
      return;
    }

    // Remote providers: test key first
    setTesting(true);
    setTestResult(null);
    try {
      await invoke("add_key", {
        provider: selectedProvider,
        key: newKey,
        baseUrl: newBaseUrl || undefined,
      });
      await fetchProviderModels(selectedProvider);
      setTestResult({ success: true, message: t("harness.apiKeyValid") });
      await invoke("remove_key", { provider: selectedProvider });
    } catch (e: any) {
      const errorMsg = e?.message || e?.toString() || "Test failed";
      setTestResult({ success: false, message: errorMsg });
      setTesting(false);
      return;
    }
    setTesting(false);

    // Save
    try {
      await invoke("add_key", {
        provider: selectedProvider,
        key: newKey,
        baseUrl: newBaseUrl || undefined,
        defaultModel: undefined,
        models: newModels.length > 0 ? newModels : undefined,
        compactModel: newCompactModel || undefined,
      });
      window.dispatchEvent(new CustomEvent('models-added'));
      onSuccess();
      onClose();
    } catch (e) {
      alert(`${t("harness.failedAddKey")}: ${e}`);
    }
  };

  const handleDiscoverCustomModels = async () => {
    const url = customBaseUrl.trim();
    if (!url) return;
    setCustomModelsLoading(true);
    setCustomDiscoverError(null);
    setCustomAvailableModels([]);
    try {
      const models = await discoverModels(url, customApiKey.trim() || undefined);
      setCustomAvailableModels(models);
    } catch (e: any) {
      setCustomDiscoverError(e?.message || String(e));
    } finally {
      setCustomModelsLoading(false);
    }
  };

  const handleAddCustom = async () => {
    const name = customProviderName.trim();
    const id = customProviderId.trim();
    const url = customBaseUrl.trim();
    if (!name) { alert(t("harness.customProviderNameRequired")); return; }
    if (!id) { alert(t("harness.customProviderIdRequired")); return; }
    if (!url) { alert(t("harness.customBaseUrlRequired")); return; }
    if (dynamicProviders.some(p => p.id === id) || keys.some(k => k.provider === id)) {
      alert(t("harness.providerIdExists"));
      return;
    }
    setCustomTesting(true);
    try {
      await invoke("add_key", {
        provider: id,
        key: customApiKey.trim() || "",
        baseUrl: url,
        models: customModels.length > 0 ? customModels : undefined,
        modelCapabilities: customModels.length > 0 ? customModelCaps : undefined,
        custom: true,
      });
      window.dispatchEvent(new CustomEvent('models-added'));
      onSuccess();
      onClose();
    } catch (e) {
      alert(`${t("harness.failedAddKey")}: ${e}`);
    } finally {
      setCustomTesting(false);
    }
  };

  if (!open) return null;

  const selectedProviderName = dynamicProviders.find(p => p.id === selectedProvider)?.name || selectedProvider;

  // ── Render ──
  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50" onClick={onClose}>
      <div
        className="w-[440px] max-h-[85vh] overflow-hidden rounded-md bg-white shadow-xl dark:bg-zinc-800 flex flex-col"
        onClick={(e) => e.stopPropagation()}
      >
        {/* Header with optional back button */}
        <div className="shrink-0 flex items-center gap-2 px-6 pt-6 pb-3">
          {step !== "picker" && (
            <button
              onClick={() => setStep("picker")}
              className="text-zinc-400 hover:text-zinc-600 dark:hover:text-zinc-200"
            >
              <ChevronLeft className="h-4 w-4" />
            </button>
          )}
          <h3 className="text-sm font-semibold">
            {step === "picker" && t("harness.availableProviders")}
            {step === "add" && (selectedProviderIsLocal ? t("harness.connectLocalProvider") : t("harness.addApiKey")) + " " + selectedProviderName}
            {step === "custom" && t("harness.addCustomProvider")}
          </h3>
        </div>

        {/* Scrollable content */}
        <div className="flex-1 overflow-y-auto px-6 pb-2">

          {/* ── Step: Picker ── */}
          {step === "picker" && (
            <ProviderPicker
              providers={dynamicProviders}
              keys={keys}
              onConnect={handleConnect}
              onAddCustom={handleStartCustom}
            />
          )}

          {/* ── Step: Add ── */}
          {step === "add" && (
            <div className="space-y-2">
              {/* Provider display (read-only) */}
              <div>
                <label className="mb-1 block text-xs text-zinc-500">{t("harness.provider")}</label>
                <div className="w-full rounded-md border border-zinc-200 bg-zinc-50 px-3 py-2 text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200">
                  {selectedProviderName}
                </div>
              </div>

              {/* API Key */}
              {needsApiKey(selectedProvider) && (
                <div>
                  <label className="mb-1 block text-xs text-zinc-500">{t("harness.apiKey")}</label>
                  <StyledInput
                    type="password"
                    value={newKey}
                    onChange={(e) => setNewKey(e.target.value)}
                    placeholder={keyPlaceholder(selectedProvider)}
                  />
                </div>
              )}

              {/* Base URL */}
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

              {/* Model selection */}
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
                        <button onClick={() => toggleNewModel(m)} className="text-accent-green/60 hover:text-accent-green">×</button>
                      </span>
                    ))}
                  </div>
                )}

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
                        const matchesSearch = !modelSearchTerm ||
                          m.id.toLowerCase().includes(modelSearchTerm.toLowerCase()) ||
                          m.name.toLowerCase().includes(modelSearchTerm.toLowerCase());
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
                            onChange={() => toggleNewModel(m.id)}
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
                          setNewModelCaps(prev => ({ ...prev, [val]: makeDefaultCaps(undefined) }));
                          (e.target as HTMLInputElement).value = "";
                        }
                      }
                    }}
                  />
                </div>
              </div>

              {/* Per-model capabilities (local providers only) */}
              {selectedProviderIsLocal && newModels.length > 0 && (
                <div>
                  <label className="mb-1 block text-xs text-zinc-500">
                    {t("harness.modelCapabilities")}
                    <span className="ml-1 text-xs text-amber-500">({t("harness.manualInputRequired")})</span>
                  </label>
                  <div className="space-y-1">
                    {newModels.map(modelId => {
                      const caps = newModelCaps[modelId];
                      if (!caps) return null;
                      return (
                        <ModelCapEditor
                          key={modelId}
                          modelId={modelId}
                          caps={caps}
                          expanded={newExpandedModels.has(modelId)}
                          onToggle={() => setNewExpandedModels(prev => {
                            const next = new Set(prev);
                            if (next.has(modelId)) next.delete(modelId);
                            else next.add(modelId);
                            return next;
                          })}
                          onUpdate={(field, value) => updateNewModelCap(modelId, field, value)}
                        />
                      );
                    })}
                  </div>
                </div>
              )}

              {/* Compact model */}
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
              {testResult && testResult.success && (
                <div className="rounded-md bg-green-50 px-3 py-2 text-xs text-green-700 dark:bg-green-900/20 dark:text-green-400">
                  {testResult.message}
                </div>
              )}
              {testResult && !testResult.success && (
                <ErrorBox message={testResult.message} />
              )}
            </div>
          )}

          {/* ── Step: Custom ── */}
          {step === "custom" && (
            <div className="space-y-2">
              {/* Provider Name */}
              <div>
                <label className="mb-1 block text-xs text-zinc-500">{t("harness.customProviderName")}</label>
                <StyledInput
                  type="text"
                  value={customProviderName}
                  onChange={(e) => {
                    setCustomProviderName(e.target.value);
                    setCustomProviderId(slugifyProviderId(e.target.value));
                  }}
                  placeholder="e.g. My GPT Proxy"
                />
              </div>

              {/* Provider ID */}
              <div>
                <label className="mb-1 block text-xs text-zinc-500">{t("harness.customProviderId")}</label>
                <StyledInput
                  type="text"
                  value={customProviderId}
                  onChange={(e) => setCustomProviderId(e.target.value)}
                  placeholder="e.g. custom-my-gpt-proxy"
                  fontMono
                />
              </div>

              {/* Base URL */}
              <div>
                <label className="mb-1 block text-xs text-zinc-500">{t("harness.customBaseUrl")}</label>
                <StyledInput
                  type="text"
                  value={customBaseUrl}
                  onChange={(e) => setCustomBaseUrl(e.target.value)}
                  onBlur={() => { if (customBaseUrl.trim()) handleDiscoverCustomModels(); }}
                  onKeyDown={(e) => { if (e.key === "Enter" && customBaseUrl.trim()) { e.preventDefault(); handleDiscoverCustomModels(); } }}
                  placeholder="https://api.example.com/v1"
                  fontMono
                />
              </div>

              {/* API Key (optional) */}
              <div>
                <label className="mb-1 block text-xs text-zinc-500">{t("harness.apiKey")} <span className="text-zinc-400">({t("harness.optional")})</span></label>
                <StyledInput
                  type="password"
                  value={customApiKey}
                  onChange={(e) => setCustomApiKey(e.target.value)}
                  placeholder="sk-..."
                />
              </div>

              {/* Model discovery status */}
              {customModelsLoading && (
                <div className="rounded-md bg-zinc-50 px-3 py-2 text-xs text-zinc-500 dark:bg-zinc-900">
                  {t("harness.discoveringModels")}
                </div>
              )}
              {customDiscoverError && (
                <ErrorBox message={`${t("harness.discoverFailed")}: ${customDiscoverError}`} />
              )}

              {/* Model selection */}
              {customAvailableModels.length > 0 && (
                <div>
                  <label className="mb-1 block text-xs text-zinc-500">
                    {t("harness.defaultModel")} {customModels.length > 0 && <span className="text-accent-green">({customModels.length} {t("harness.selected")})</span>}
                  </label>
                  {customModels.length > 0 && (
                    <div className="mb-1 flex flex-wrap gap-1">
                      {customModels.map((m) => (
                        <span key={m} className="inline-flex items-center gap-1 rounded bg-accent-green/10 px-2 py-0.5 text-xs text-accent-green">
                          {m}
                          <button onClick={() => toggleCustomModel(m)} className="text-accent-green/60 hover:text-accent-green">×</button>
                        </span>
                      ))}
                    </div>
                  )}
                  <StyledInput
                    type="text"
                    value={customModelSearchTerm}
                    onChange={(e) => setCustomModelSearchTerm(e.target.value)}
                    placeholder={t("harness.searchModels")}
                  />
                  <div className="mt-1 max-h-40 overflow-y-auto rounded border border-zinc-200 dark:border-zinc-700">
                    {customAvailableModels
                      .filter((m) => {
                        if (!customModelSearchTerm) return true;
                        const term = customModelSearchTerm.toLowerCase();
                        return m.id.toLowerCase().includes(term) || m.name.toLowerCase().includes(term);
                      })
                      .map((m) => (
                        <label
                          key={m.id}
                          className="flex cursor-pointer items-center gap-2 px-3 py-1.5 text-xs hover:bg-zinc-50 dark:hover:bg-zinc-700"
                        >
                          <input
                            type="checkbox"
                            checked={customModels.includes(m.id)}
                            onChange={() => toggleCustomModel(m.id)}
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
                            </div>
                          </div>
                        </label>
                      ))}
                  </div>
                  <div className="mt-2 flex gap-1">
                    <StyledInput
                      type="text"
                      placeholder={t("harness.customModelPlaceholder")}
                      className="flex-1"
                      onKeyDown={(e) => {
                        if (e.key === "Enter") {
                          const val = (e.target as HTMLInputElement).value.trim();
                          if (val && !customModels.includes(val)) {
                            setCustomModels([...customModels, val]);
                            setCustomModelCaps(prev => ({ ...prev, [val]: makeDefaultCaps(undefined) }));
                            (e.target as HTMLInputElement).value = "";
                          }
                        }
                      }}
                    />
                  </div>
                </div>
              )}

              {/* Per-model capabilities */}
              {customModels.length > 0 && (
                <div>
                  <label className="mb-1 block text-xs text-zinc-500">
                    {t("harness.modelCapabilities")}
                    <span className="ml-1 text-xs text-amber-500">({t("harness.manualInputRequired")})</span>
                  </label>
                  <div className="space-y-1">
                    {customModels.map(modelId => {
                      const caps = customModelCaps[modelId];
                      if (!caps) return null;
                      return (
                        <ModelCapEditor
                          key={modelId}
                          modelId={modelId}
                          caps={caps}
                          expanded={customExpandedModels.has(modelId)}
                          onToggle={() => setCustomExpandedModels(prev => {
                            const next = new Set(prev);
                            if (next.has(modelId)) next.delete(modelId);
                            else next.add(modelId);
                            return next;
                          })}
                          onUpdate={(field, value) => updateCustomModelCap(modelId, field, value)}
                        />
                      );
                    })}
                  </div>
                </div>
              )}
            </div>
          )}
        </div>

        {/* Footer */}
        <div className="shrink-0 flex items-center justify-between gap-2 border-t border-zinc-100 dark:border-zinc-800 px-6 py-4">
          {/* Status on the left */}
          <div className="flex-1 min-w-0">
            {step === "add" && testResult && testResult.success && (
              <div className="truncate rounded-md bg-green-50 px-3 py-1.5 text-xs text-green-700 dark:bg-green-900/20 dark:text-green-400">
                {testResult.message}
              </div>
            )}
            {step === "add" && testResult && !testResult.success && (
              <div className="truncate text-xs text-red-600 dark:text-red-400" title={testResult.message}>
                {testResult.message}
              </div>
            )}
            {step === "add" && testing && (
              <div className="text-xs text-zinc-400">{t("harness.testing")}</div>
            )}
          </div>

          {/* Buttons on the right */}
          <div className="flex gap-2 shrink-0">
            <button
              onClick={onClose}
              className="rounded-md px-3 py-1.5 text-xs font-medium text-zinc-600 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-700"
            >
              {t("common.cancel")}
            </button>
            {step === "add" && (
              <button
                onClick={handleAdd}
                disabled={(needsApiKey(selectedProvider) ? !newKey.trim() : false) || testing}
                className="rounded-md bg-zinc-200 px-3 py-1.5 text-xs font-medium text-zinc-800 hover:bg-zinc-300 disabled:opacity-50 dark:bg-zinc-700 dark:hover:bg-zinc-600"
              >
                {testing ? t("harness.saving") : t("harness.save")}
              </button>
            )}
            {step === "custom" && (
              <button
                onClick={handleAddCustom}
                disabled={!customProviderName.trim() || !customProviderId.trim() || !customBaseUrl.trim() || customTesting}
                className="rounded-md bg-zinc-200 px-3 py-1.5 text-xs font-medium text-zinc-800 hover:bg-zinc-300 disabled:opacity-50 dark:bg-zinc-700 dark:hover:bg-zinc-600"
              >
                {customTesting ? t("harness.saving") : t("harness.save")}
              </button>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
