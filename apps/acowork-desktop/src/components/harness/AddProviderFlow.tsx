import { useState, useEffect, useCallback, useMemo } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { VaultKeyEntry, ModelInfo, ModelCapabilitiesInfo, ProviderListEntry } from "../../lib/types";
import { StyledInput } from "../common/StyledInput";
import { needsApiKey, keyPlaceholder, isLocalProvider } from "../../lib/providers";
import { fetchProviderModels, discoverModels, fetchProviders } from "../../lib/gateway-api";
import { ModelMultiSelect } from "./ModelMultiSelect";
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
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-modal-overlay" onClick={onClose}>
      <div
        className="w-[440px] max-h-[85vh] overflow-hidden rounded-md bg-modal-surface shadow-xl flex flex-col"
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

              {/* Model selection (shared multi-select component) */}
              <ModelMultiSelect
                models={availableModels}
                loading={modelsLoading}
                selected={newModels}
                onSelectedChange={setNewModels}
                caps={newModelCaps}
                onCapsChange={setNewModelCaps}
                expandedModels={newExpandedModels}
                onExpandedToggle={(modelId) =>
                  setNewExpandedModels((prev) => {
                    const next = new Set(prev);
                    if (next.has(modelId)) next.delete(modelId);
                    else next.add(modelId);
                    return next;
                  })
                }
                showModelCapEditor={selectedProviderIsLocal}
                compactModel={newCompactModel}
                onCompactModelChange={setNewCompactModel}
                showCompactModel={true}
              />

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

              {/* Model selection (shared multi-select component) — only after discover */}
              {customAvailableModels.length > 0 && (
                <ModelMultiSelect
                  models={customAvailableModels}
                  selected={customModels}
                  onSelectedChange={setCustomModels}
                  caps={customModelCaps}
                  onCapsChange={setCustomModelCaps}
                  expandedModels={customExpandedModels}
                  onExpandedToggle={(modelId) =>
                    setCustomExpandedModels((prev) => {
                      const next = new Set(prev);
                      if (next.has(modelId)) next.delete(modelId);
                      else next.add(modelId);
                      return next;
                    })
                  }
                  showModelCapEditor={true}
                  showCapabilityFilter={false}
                  showCompactModel={false}
                />
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
