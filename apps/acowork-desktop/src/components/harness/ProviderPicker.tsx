import { useState, useMemo } from "react";
import type { VaultKeyEntry, ProviderListEntry } from "../../lib/types";
import { isLocalProvider } from "../../lib/providers";
import { StyledInput } from "../common/StyledInput";
import { useTranslation } from "../../i18n/useTranslation";
import { Search, Plus, ChevronsDown } from "lucide-react";

interface ProviderPickerProps {
  providers: ProviderListEntry[];
  keys: VaultKeyEntry[];
  onConnect: (providerId: string, provider: ProviderListEntry) => void;
  onAddCustom: () => void;
}

/** Reusable available-providers list. Renders custom / local / remote sections
 *  with "Connect" buttons and an "Add Custom Provider" button. Pure UI —
 *  caller handles the add flow. */
export function ProviderPicker({ providers, keys, onConnect, onAddCustom }: ProviderPickerProps) {
  const { t } = useTranslation();
  const [providerSearchTerm, setProviderSearchTerm] = useState("");
  const [showAllRemote, setShowAllRemote] = useState(false);

  // Split providers into local / custom / remote
  const { localProviders, remoteProviders, customProviders } = useMemo(() => {
    const local: ProviderListEntry[] = [];
    const remote: ProviderListEntry[] = [];
    const custom: ProviderListEntry[] = [];
    for (const p of providers) {
      if (p.custom) {
        custom.push(p);
      } else if (p.local || isLocalProvider(p.id)) {
        local.push(p);
      } else {
        remote.push(p);
      }
    }
    return { localProviders: local, remoteProviders: remote, customProviders: custom };
  }, [providers]);

  // Filter remote providers by search term
  const filteredRemoteProviders = useMemo(() => {
    if (!providerSearchTerm.trim()) return remoteProviders;
    const term = providerSearchTerm.toLowerCase().trim();
    return remoteProviders.filter(p =>
      p.name?.toLowerCase().includes(term) ||
      p.id.toLowerCase().includes(term)
    );
  }, [remoteProviders, providerSearchTerm]);

  if (providers.length === 0) {
    return (
      <div className="py-3 text-center text-xs text-zinc-400">{t("harness.noProvidersAvailable")}</div>
    );
  }

  return (
    <div className="space-y-3">
      {/* Custom Providers */}
      <div>
        <h4 className="mb-1.5 text-xs font-medium text-zinc-500 dark:text-zinc-400">🔧 {t("harness.customProviders")}</h4>
        <div className="space-y-1">
          {customProviders.map((item) => {
            const providerId = item.id;
            const providerName = item.name || providerId;
            const keyEntry = keys.find((k) => k.provider === providerId);
            if (keyEntry) return null;
            return (
              <div key={providerId} className="rounded-md border border-zinc-200 px-3 py-1.5 dark:border-zinc-700">
                <div className="flex items-center justify-between">
                  <div className="min-w-0 flex-1">
                    <span className="text-xs font-medium">{providerName}</span>
                  </div>
                  <button
                    onClick={() => onConnect(providerId, item)}
                    className="rounded-md bg-zinc-100 px-3 py-1 text-xs font-medium text-zinc-700 hover:bg-zinc-200 dark:bg-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-600"
                  >
                    {t("harness.connect")}
                  </button>
                </div>
              </div>
            );
          })}

          {/* Add Custom Provider button */}
          <button
            onClick={onAddCustom}
            className="flex w-full items-center gap-2 rounded-md border-2 border-dashed border-zinc-300 px-3 py-2 text-xs font-medium text-zinc-600 transition-colors hover:border-[var(--color-accent)] hover:text-[var(--color-accent)] dark:border-zinc-600 dark:text-zinc-400 dark:hover:border-[var(--color-accent)] dark:hover:text-[var(--color-accent)]"
          >
            <Plus className="h-4 w-4" />
            {t("harness.addCustomProvider")}
          </button>
        </div>
      </div>

      {/* Local Providers */}
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
                <div key={providerId} className="rounded-md border border-zinc-200 px-3 py-1.5 dark:border-zinc-700">
                  <div className="flex items-center justify-between">
                    <div className="min-w-0 flex-1">
                      <span className="text-xs font-medium">{providerName}</span>
                    </div>
                    <button
                      onClick={() => onConnect(providerId, item)}
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

      {/* Remote Providers (expandable) */}
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
                      <div key={providerId} className="rounded-md border border-zinc-200 px-3 py-1.5 dark:border-zinc-700">
                        <div className="flex items-center justify-between">
                          <div className="min-w-0 flex-1">
                            <span className="text-xs font-medium">{providerName}</span>
                            {modelCount != null ? (
                              <span className="ml-2 text-xs text-zinc-400">{t("harness.modelsAvailable", { count: modelCount })}</span>
                            ) : null}
                          </div>
                          <button
                            onClick={() => onConnect(providerId, item)}
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
                  className="mt-1 flex w-full items-center justify-center gap-1 rounded-md border border-dashed border-zinc-300 py-2 text-xs text-zinc-500 transition-colors hover:border-zinc-400 hover:text-zinc-700 dark:border-zinc-600 dark:text-zinc-400 dark:hover:border-zinc-500 dark:hover:text-zinc-300"
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
  );
}
