import { useState, useEffect, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { SearchKeyEntry, SearchProviderDef } from "../../lib/types";
import { cn } from "../../lib/utils";
import { SEARCH_PROVIDERS, lookupSearchProvider, searchKeyPlaceholder } from "../../lib/search-providers";
import { Star } from "lucide-react";

/** Search Provider configuration tab — mirrors ProvidersTab layout */
export function SearchTab() {
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
      setTestResult({ success: false, message: "Please enter an API Key first" });
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
        const resp = await fetch(`${import.meta.env.VITE_GATEWAY_URL || "http://127.0.0.1:19876"}/api/search/test?provider=${newProvider}`);
        if (!resp.ok) {
          const err = await resp.text();
          throw new Error(err || `Test failed with status ${resp.status}`);
        }
        setTestResult({ success: true, message: "API Key is valid!" });

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
      alert(`Failed to add search key: ${e}`);
    }
  };

  const handleRemove = async (provider: string) => {
    if (!confirm(`Remove search key for ${provider}?`)) return;
    try {
      await invoke("remove_search_key", { provider });
      await fetchKeys();
    } catch (e) {
      alert(`Failed to remove search key: ${e}`);
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
      alert(`Failed to update search key: ${e}`);
    }
  };

  // Helper: get available providers (not yet configured)
  const availableProviders = SEARCH_PROVIDERS.filter(
    (p) => !keys.some((k) => k.provider === p.id)
  );

  return (
    <div className="max-w-2xl space-y-4">
      <div className="rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <div className="flex items-center justify-between mb-3">
          <h2 className="text-xs font-medium">Search Provider Management</h2>
        </div>

        {/* Configured Search Providers (top section) */}
        {keysLoading ? (
          <div className="py-3 text-center text-xs text-zinc-400">Loading...</div>
        ) : keys.length > 0 && (
          <div>
            <h3 className="mb-2 text-xs font-medium text-zinc-500">Configured Search Providers</h3>
            <div className="space-y-1">
              {keys.map((keyEntry) => {
                const def = lookupSearchProvider(keyEntry.provider);
                const providerName = def?.name || keyEntry.provider;

                return (
                  <div key={keyEntry.provider} className="rounded-lg border border-zinc-200 px-3 py-1.5 dark:border-zinc-700">
                    <div className="flex items-center justify-between gap-2">
                      <div className="flex items-center flex-nowrap gap-2">
                        <span className="shrink-0 text-xs font-medium">{providerName}</span>
                      </div>
                      <div className="flex items-center gap-2">
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
                  </div>
                );
              })}
            </div>
          </div>
        )}

      </div>

      <div className="rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">

        {/* Available Search Providers (bottom section) */}
        <div>
          <h3 className="mb-2 text-xs font-medium text-zinc-500">Available Search Providers</h3>
          <div className="space-y-1">
            {availableProviders.length === 0 ? (
              <div className="py-3 text-center text-xs text-zinc-400">All search providers configured</div>
            ) : (
              availableProviders.map((item) => (
                <div key={item.id} className="rounded-lg border border-zinc-200 px-3 py-1.5 dark:border-zinc-700">
                  <div className="flex items-center justify-between">
                    <div className="min-w-0 flex-1">
                      <span className="text-xs font-medium">{item.name}</span>
                      <span className="ml-2 text-xs text-zinc-400">{item.description}</span>
                    </div>
                    <button
                      onClick={() => {
                        setNewProvider(item.id);
                        setNewBaseUrl(item.base_url);
                        setShowAddDialog(true);
                      }}
                      className="rounded-md bg-zinc-100 px-3 py-1 text-xs font-medium text-zinc-700 hover:bg-zinc-200 dark:bg-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-600"
                    >
                      Add Key
                    </button>
                  </div>
                  <div className="mt-0.5 text-xs text-zinc-400">{item.free_quota}</div>
                </div>
              ))
            )}
          </div>
        </div>
      </div>

      {/* Add key dialog */}
      {showAddDialog && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50">
          <div className="w-[400px] max-h-[85vh] overflow-y-auto rounded-lg bg-white p-6 shadow-xl dark:bg-zinc-800">
            <h3 className="mb-3 text-sm font-semibold">
              Add Search Provider: {lookupSearchProvider(newProvider)?.name || newProvider}
            </h3>

            <div className="space-y-3">
              {/* Provider display (read-only) */}
              <div>
                <label className="mb-1 block text-xs text-zinc-500">Provider</label>
                <div className="w-full rounded-md border border-zinc-200 bg-zinc-50 px-3 py-2 text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200">
                  {lookupSearchProvider(newProvider)?.name || newProvider}
                </div>
              </div>

              {/* API Key */}
              {lookupSearchProvider(newProvider)?.requires_api_key !== false && (
                <div>
                  <label className="mb-1 block text-xs text-zinc-500">API Key</label>
                  <input
                    type="password"
                    value={newKey}
                    onChange={(e) => setNewKey(e.target.value)}
                    placeholder={searchKeyPlaceholder(newProvider)}
                    className="w-full rounded-md border border-zinc-200 px-3 py-2 text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                  />
                </div>
              )}

              {/* Base URL */}
              <div>
                <label className="mb-1 block text-xs text-zinc-500">Base URL <span className="text-zinc-400">({newProvider === "searxng" ? "required" : "optional"})</span></label>
                <input
                  type="text"
                  value={newBaseUrl}
                  onChange={(e) => setNewBaseUrl(e.target.value)}
                  placeholder={lookupSearchProvider(newProvider)?.base_url || "https://..."}
                  className="w-full rounded-md border border-zinc-200 px-3 py-2 font-mono text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                />
              </div>

              {/* Test Result */}
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
              {testing && (
                <div className="text-xs text-zinc-400">Testing...</div>
              )}
            </div>

            <div className="mt-4 flex justify-end gap-2">
              <button
                onClick={() => { setShowAddDialog(false); setNewKey(""); setTestResult(null); }}
                className="rounded-md px-3 py-1.5 text-xs font-medium text-zinc-600 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-700"
              >
                Cancel
              </button>
              <button
                onClick={handleAdd}
                disabled={testing}
                className="rounded-md px-4 py-1.5 text-xs font-medium text-white disabled:opacity-50"
                style={{ backgroundColor: testing ? "var(--color-accent)" : "var(--color-accent)" }}
              >
                {testing ? "Testing..." : "Test & Save"}
              </button>
            </div>
          </div>
        </div>
      )}

      {/* Edit key dialog */}
      {showEditDialog && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50">
          <div className="w-[400px] max-h-[85vh] overflow-y-auto rounded-lg bg-white p-6 shadow-xl dark:bg-zinc-800">
            <h3 className="mb-3 text-sm font-semibold">
              Edit Search Provider: {editProviderDef?.name || showEditDialog}
            </h3>

            <div className="space-y-3">
              {/* Provider display (read-only) */}
              <div>
                <label className="mb-1 block text-xs text-zinc-500">Provider</label>
                <div className="w-full rounded-md border border-zinc-200 bg-zinc-50 px-3 py-2 text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200">
                  {editProviderDef?.name || showEditDialog}
                </div>
              </div>

              {/* API Key */}
              {editProviderDef?.requires_api_key !== false && (
                <div>
                  <label className="mb-1 block text-xs text-zinc-500">API Key <span className="text-zinc-400">(leave empty to keep current)</span></label>
                  <input
                    type="password"
                    value={editKey}
                    onChange={(e) => setEditKey(e.target.value)}
                    placeholder={searchKeyPlaceholder(showEditDialog)}
                    className="w-full rounded-md border border-zinc-200 px-3 py-2 text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                  />
                  <p className="mt-0.5 text-xs text-zinc-400">Current: {keys.find(k => k.provider === showEditDialog)?.key_preview}</p>
                </div>
              )}

              {/* Base URL */}
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
                className="rounded-md px-4 py-1.5 text-xs font-medium text-white"
                style={{ backgroundColor: "var(--color-accent)" }}
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
