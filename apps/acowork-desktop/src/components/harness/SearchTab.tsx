import { useState, useEffect, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { SearchKeyEntry, SearchProviderDef } from "../../lib/types";
import { StyledInput } from "../common/StyledInput";
import { ExpandableRow, ListBox, ListRow } from "../common/list";
import { SEARCH_PROVIDERS, lookupSearchProvider, searchKeyPlaceholder } from "../../lib/search-providers";
import { useTranslation } from "../../i18n/useTranslation";
import { ErrorBox } from "../common/ErrorBox";
import { getGatewayUrl } from "../../lib/config";

/** Search Provider configuration tab — mirrors ProvidersTab layout */
export function SearchTab() {
  const { t } = useTranslation();
  const [keys, setKeys] = useState<SearchKeyEntry[]>([]);
  const [keysLoading, setKeysLoading] = useState(true);
  const [showAddDialog, setShowAddDialog] = useState(false);
  const [showEditDialog, setShowEditDialog] = useState<string | null>(null);
  const [newProvider, setNewProvider] = useState("tavily");
  const [newKey, setNewKey] = useState("");
  const [newBaseUrl, setNewBaseUrl] = useState("");
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<{ success: boolean; message: string } | null>(null);

  // Edit dialog state
  const [editKey, setEditKey] = useState("");
  const [editBaseUrl, setEditBaseUrl] = useState("");
  const [editProviderDef, setEditProviderDef] = useState<SearchProviderDef | null>(null);

  // Tools-tab style level-1 collapsible groups (default open)
  const [configuredOpen, setConfiguredOpen] = useState(true);
  const [availableOpen, setAvailableOpen] = useState(true);

  const fetchKeys = useCallback(async () => {
    try {
      const result = await invoke<SearchKeyEntry[]>("list_search_keys");
      setKeys(result);
    } catch {
      // Gateway may not be running
    } finally {
      setKeysLoading(false);
    }
  }, []);

  useEffect(() => {
    fetchKeys();
  }, [fetchKeys]);

  const handleAdd = async () => {
    const providerDef = lookupSearchProvider(newProvider);
    if (providerDef?.requires_api_key !== false && !newKey.trim()) {
      setTestResult({ success: false, message: t("harnessSearch.pleaseEnterApiKey") });
      return;
    }

    // --- Test Key (when requires_api_key) ---
    if (providerDef?.requires_api_key !== false) {
      setTesting(true);
      setTestResult(null);
      try {
        // Temporarily add the key and test via Gateway search API
        await invoke("add_search_key", {
          provider: newProvider,
          key: newKey,
          baseUrl: newBaseUrl || undefined,
        });

        // Test search to verify the key works
        const resp = await fetch(`${getGatewayUrl()}/api/search/test?provider=${newProvider}`);
        if (!resp.ok) {
          const err = await resp.text();
          throw new Error(err || `Test failed with status ${resp.status}`);
        }
        setTestResult({ success: true, message: t("harnessSearch.apiKeyValid") });

        // Remove temporary key (will be re-added in the final save below)
        await invoke("remove_search_key", { provider: newProvider });
      } catch (e: any) {
        const errorMsg = e?.message || e?.toString() || "Test failed";
        setTestResult({ success: false, message: errorMsg });
        // Clean up temp key
        try { await invoke("remove_search_key", { provider: newProvider }); } catch { /* ignore */ }
        setTesting(false);
        return;
      }
      setTesting(false);
    }

    // --- Save ---
    try {
      await invoke("add_search_key", {
        provider: newProvider,
        key: newKey,
        baseUrl: newBaseUrl || undefined,
      });
      setShowAddDialog(false);
      setNewKey("");
      setNewBaseUrl("");
      setTestResult(null);
      await fetchKeys();
    } catch (e) {
      alert(t("harnessSearch.failedAddKey") + e);
    }
  };

  const handleRemove = async (provider: string) => {
    if (!confirm(t("harnessSearch.removeKeyConfirm", { provider }))) return;
    try {
      await invoke("remove_search_key", { provider });
      await fetchKeys();
    } catch (e) {
      alert(t("harnessSearch.failedRemoveKey") + e);
    }
  };

  const handleEdit = (provider: string) => {
    const keyEntry = keys.find((k) => k.provider === provider);
    const def = lookupSearchProvider(provider);
    setEditKey(keyEntry?.key_preview ?? "");
    setEditBaseUrl(keyEntry?.base_url ?? def?.base_url ?? "");
    setEditProviderDef(def ?? null);
    setShowEditDialog(provider);
  };

  const handleEditSave = async () => {
    if (!showEditDialog) return;
    try {
      const keyEntry = keys.find((k) => k.provider === showEditDialog);
      const updatePayload: Record<string, unknown> = {
        provider: showEditDialog,
        baseUrl: editBaseUrl || undefined,
      };
      // Only include key if user actually typed a new one (not the masked preview)
      if (editKey && editKey !== keyEntry?.key_preview) {
        updatePayload.key = editKey;
      }
      await invoke("update_search_key", updatePayload);
      setShowEditDialog(null);
      await fetchKeys();
    } catch (e) {
      alert(t("harnessSearch.failedUpdateKey") + e);
    }
  };

  // Helper: get available providers (not yet configured)
  const availableProviders = SEARCH_PROVIDERS.filter(
    (p) => !keys.some((k) => k.provider === p.id)
  );

  return (
    <div className="max-w-2xl space-y-4">
      {/* Configured Search Providers — Tools-tab level-1 collapsible
          card, default open, unified rows in the inset body. */}
      {keysLoading ? (
        <div className="py-3 text-center text-xs text-zinc-400">{t("harnessSearch.loading")}</div>
      ) : keys.length > 0 && (
        <ListBox dividers={false}>
          <ExpandableRow
            open={configuredOpen}
            onToggle={() => setConfiguredOpen((v) => !v)}
            title={t("harnessSearch.configuredSearchProviders", { count: keys.length })}
            ariaLabel={t("harnessSearch.configuredSearchProviders", { count: keys.length })}
            bodyClassName="rounded-b-md border-t border-zinc-300 bg-panel-inset dark:border-zinc-700"
          >
            <ListBox variant="plain">
              {keys.map((keyEntry) => {
                const def = lookupSearchProvider(keyEntry.provider);
                const providerName = def?.name || keyEntry.provider;

                return (
                  <ListRow
                    key={keyEntry.provider}
                    trailing={
                      <div className="flex shrink-0 items-center gap-2.5">
                        <span className="text-xs" style={{ color: "var(--color-accent)" }}>{t("harnessSearch.active")}</span>
                        <button
                          type="button"
                          onClick={() => handleEdit(keyEntry.provider)}
                          className="text-xs hover:opacity-70"
                          style={{ color: "var(--color-accent)" }}
                        >
                          {t("harnessSearch.edit")}
                        </button>
                        <button
                          type="button"
                          onClick={() => handleRemove(keyEntry.provider)}
                          className="text-xs text-red-500 hover:text-red-700"
                        >
                          {t("harnessSearch.remove")}
                        </button>
                      </div>
                    }
                  >
                    <div className="flex items-center gap-2">
                      <span className="truncate text-xs font-medium text-zinc-700 dark:text-zinc-300">{providerName}</span>
                      <span className="shrink-0 text-[11px] text-zinc-400">{t("harnessSearch.key")}: {keyEntry.key_preview}</span>
                    </div>
                  </ListRow>
                );
              })}
            </ListBox>
          </ExpandableRow>
        </ListBox>
      )}

      {/* Available Search Providers — Tools-tab level-1 collapsible
          card, default open. */}
      <ListBox dividers={false}>
        <ExpandableRow
          open={availableOpen}
          onToggle={() => setAvailableOpen((v) => !v)}
          title={t("harnessSearch.availableSearchProviders", { count: availableProviders.length })}
          ariaLabel={t("harnessSearch.availableSearchProviders", { count: availableProviders.length })}
          bodyClassName="rounded-b-md border-t border-zinc-300 bg-panel-inset dark:border-zinc-700"
        >
          {availableProviders.length === 0 ? (
            <div className="px-3 py-3 text-center text-xs text-zinc-400">{t("harnessSearch.allConfigured")}</div>
          ) : (
            <ListBox variant="plain">
              {availableProviders.map((item) => (
                <ListRow
                  key={item.id}
                  trailing={
                    <button
                      type="button"
                      onClick={() => {
                        setNewProvider(item.id);
                        setNewBaseUrl(item.base_url);
                        setShowAddDialog(true);
                      }}
                      className="rounded-md bg-zinc-100 px-3 py-1 text-xs font-medium text-zinc-700 hover:bg-zinc-200 dark:bg-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-600"
                    >
                      {t("harnessSearch.addKey")}
                    </button>
                  }
                >
                  <div className="flex min-w-0 items-baseline gap-2">
                    <span className="truncate text-xs font-medium text-zinc-700 dark:text-zinc-300">{item.name}</span>
                    <span className="truncate text-[11px] text-zinc-400">{item.description}</span>
                  </div>
                  {item.free_quota && (
                    <div className="mt-0.5 text-[10px] text-zinc-400">{item.free_quota}</div>
                  )}
                </ListRow>
              ))}
            </ListBox>
          )}
        </ExpandableRow>
      </ListBox>

      {/* Add key dialog */}
      {showAddDialog && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-modal-overlay">
          <div className="w-[400px] max-h-[85vh] overflow-y-auto rounded-md bg-modal-surface p-6 shadow-xl">
            <h3 className="mb-3 text-sm font-semibold">
              {t("harnessSearch.addSearchProvider")} {lookupSearchProvider(newProvider)?.name || newProvider}
            </h3>

            <div className="space-y-3">
              {/* Provider display (read-only) */}
              <div>
                <label className="mb-1 block text-xs text-zinc-500">{t("harnessSearch.provider")}</label>
                <div className="w-full rounded-md border border-zinc-200 bg-zinc-50 px-3 py-2 text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200">
                  {lookupSearchProvider(newProvider)?.name || newProvider}
                </div>
              </div>

              {/* API Key */}
              {lookupSearchProvider(newProvider)?.requires_api_key !== false && (
                <div>
                  <label className="mb-1 block text-xs text-zinc-500">{t("harnessSearch.apiKey")}</label>
                  <StyledInput
                    type="password"
                    value={newKey}
                    onChange={(e) => setNewKey(e.target.value)}
                    placeholder={searchKeyPlaceholder(newProvider)}
                  />
                </div>
              )}

              {/* Base URL */}
              <div>
                <label className="mb-1 block text-xs text-zinc-500">{t("harnessSearch.baseUrl")} <span className="text-zinc-400">({newProvider === "searxng" ? t("harnessSearch.required") : t("harnessSearch.optional")})</span></label>
                <StyledInput
                  type="text"
                  value={newBaseUrl}
                  onChange={(e) => setNewBaseUrl(e.target.value)}
                  placeholder={lookupSearchProvider(newProvider)?.base_url || "https://..."}
                  fontMono
                />
              </div>

              {/* Test Result */}
              {testResult && testResult.success && (
                <div className="rounded-md bg-green-50 px-3 py-2 text-xs text-green-700 dark:bg-green-900/20 dark:text-green-400">
                  {testResult.message}
                </div>
              )}
              {testResult && !testResult.success && (
                <ErrorBox message={testResult.message} />
              )}
              {testing && (
                <div className="text-xs text-zinc-400">{t("harnessSearch.testing")}</div>
              )}
            </div>

            <div className="mt-4 flex justify-end gap-2">
              <button
                onClick={() => { setShowAddDialog(false); setNewKey(""); setTestResult(null); }}
                className="rounded-md px-3 py-1.5 text-xs font-medium text-zinc-600 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-700"
              >
                {t("common.cancel")}
              </button>
              <button
                onClick={handleAdd}
                disabled={testing}
                className="rounded-md px-4 py-1.5 text-xs font-medium text-white disabled:opacity-50"
                style={{ backgroundColor: testing ? "var(--color-accent)" : "var(--color-accent)" }}
              >
                {testing ? t("harnessSearch.testing") : t("harnessSearch.testAndSave")}
              </button>
            </div>
          </div>
        </div>
      )}

      {/* Edit key dialog */}
      {showEditDialog && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-modal-overlay">
          <div className="w-[400px] max-h-[85vh] overflow-y-auto rounded-md bg-modal-surface p-6 shadow-xl">
            <h3 className="mb-3 text-sm font-semibold">
              {t("harnessSearch.editSearchProvider")} {editProviderDef?.name || showEditDialog}
            </h3>

            <div className="space-y-3">
              {/* Provider display (read-only) */}
              <div>
                <label className="mb-1 block text-xs text-zinc-500">{t("harnessSearch.provider")}</label>
                <div className="w-full rounded-md border border-zinc-200 bg-zinc-50 px-3 py-2 text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200">
                  {editProviderDef?.name || showEditDialog}
                </div>
              </div>

              {/* API Key */}
              {editProviderDef?.requires_api_key !== false && (
                <div>
                  <label className="mb-1 block text-xs text-zinc-500">{t("harnessSearch.apiKey")} <span className="text-zinc-400">({t("harnessSearch.leaveEmptyToKeep")})</span></label>
                  <StyledInput
                    type="password"
                    value={editKey}
                    onChange={(e) => setEditKey(e.target.value)}
                    placeholder={searchKeyPlaceholder(showEditDialog)}
                  />
                  <p className="mt-0.5 text-xs text-zinc-400">{t("harnessSearch.current")}: {keys.find(k => k.provider === showEditDialog)?.key_preview}</p>
                </div>
              )}

              {/* Base URL */}
              <div>
                <label className="mb-1 block text-xs text-zinc-500">{t("harnessSearch.baseUrl")}</label>
                <StyledInput
                  type="text"
                  value={editBaseUrl}
                  onChange={(e) => setEditBaseUrl(e.target.value)}
                  placeholder="https://..."
                  fontMono
                />
              </div>
            </div>

            <div className="mt-4 flex justify-end gap-2">
              <button
                onClick={() => setShowEditDialog(null)}
                className="rounded-md px-3 py-1.5 text-xs font-medium text-zinc-600 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-700"
              >
                {t("common.cancel")}
              </button>
              <button
                onClick={handleEditSave}
                className="rounded-md px-4 py-1.5 text-xs font-medium text-white"
                style={{ backgroundColor: "var(--color-accent)" }}
              >
                {t("harnessSearch.save")}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
