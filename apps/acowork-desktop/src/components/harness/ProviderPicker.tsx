import { useState, useMemo } from "react";
import type { VaultKeyEntry, ProviderListEntry } from "../../lib/types";
import { isLocalProvider } from "../../lib/providers";
import { StyledInput } from "../common/StyledInput";
import { ListBox, ListRow, ExpandableRow } from "../common/list";
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
  // Tools-tab style level-1 collapsible groups, default open
  const [customOpen, setCustomOpen] = useState(true);
  const [localOpen, setLocalOpen] = useState(true);
  const [remoteOpen, setRemoteOpen] = useState(true);

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

  // Remote providers not yet configured, search-filtered (rows under "可用"
  // must never include already-configured keys).
  const remoteAvailable = remoteProviders.filter((p) => !keys.some((k) => k.provider === p.id));
  const normalizedTerm = providerSearchTerm.trim().toLowerCase();
  const filteredRemoteProviders = normalizedTerm
    ? remoteAvailable.filter(
        (p) => p.name?.toLowerCase().includes(normalizedTerm) || p.id.toLowerCase().includes(normalizedTerm)
      )
    : remoteAvailable;

  // Unconfigured (available) providers per group + remote display window (first 5 when >5).
  const availableCustom = customProviders.filter((p) => !keys.some((k) => k.provider === p.id));
  const availableLocal = localProviders.filter((p) => !keys.some((k) => k.provider === p.id));
  const availableRemote = remoteAvailable.length;
  const remoteLimitReached = !providerSearchTerm.trim() && !showAllRemote && filteredRemoteProviders.length > 5;
  const displayedRemote = remoteLimitReached ? filteredRemoteProviders.slice(0, 5) : filteredRemoteProviders;

  if (providers.length === 0) {
    return (
      <div className="py-3 text-center text-xs text-zinc-400">{t("harness.noProvidersAvailable")}</div>
    );
  }

  return (
    <div className="space-y-4">
      {/* ── Custom Providers — level-1 collapsible group (Tools-tab
          grammar: chevron + title + count badge, default open) ── */}
      <ListBox dividers={false}>
        <ExpandableRow
          open={customOpen}
          onToggle={() => setCustomOpen((v) => !v)}
          title={<span className="flex items-center gap-1">🔧 {t("harness.customProviders", { count: availableCustom.length })}</span>}
          ariaLabel={t("harness.customProviders", { count: availableCustom.length })}
          bodyClassName="rounded-b-md border-t border-zinc-300 bg-panel-inset dark:border-zinc-700"
        >
          <ListBox variant="plain">
            {availableCustom.map((item) => {
              const providerId = item.id;
              const providerName = item.name || providerId;
              return (
                <ListRow
                  key={providerId}
                  surface="inset"
                  trailing={
                    <button
                      type="button"
                      onClick={() => onConnect(providerId, item)}
                      className="rounded-md bg-zinc-100 px-3 py-1 text-xs font-medium text-zinc-700 hover:bg-zinc-200 dark:bg-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-600"
                    >
                      {t("harness.connect")}
                    </button>
                  }
                >
                  <span className="block truncate text-xs font-medium text-zinc-700 dark:text-zinc-300">{providerName}</span>
                </ListRow>
              );
            })}
          </ListBox>
          {/* Add Custom Provider button — sits below the custom list */}
          <div className="px-3 pb-2 pt-1.5">
            <button
              type="button"
              onClick={onAddCustom}
              className="flex w-full items-center gap-2 rounded-md border-2 border-dashed border-zinc-300 px-3 py-2 text-xs font-medium text-zinc-600 transition-colors hover:border-[var(--color-accent)] hover:text-[var(--color-accent)] dark:border-zinc-600 dark:text-zinc-400 dark:hover:border-[var(--color-accent)] dark:hover:text-[var(--color-accent)]"
            >
              <Plus className="h-4 w-4" />
              {t("harness.addCustomProvider")}
            </button>
          </div>
        </ExpandableRow>
      </ListBox>

      {/* ── Local Providers — level-1 collapsible group ── */}
      {availableLocal.length > 0 && (
        <ListBox dividers={false}>
          <ExpandableRow
            open={localOpen}
            onToggle={() => setLocalOpen((v) => !v)}
            title={<span className="flex items-center gap-1">🏠 {t("harness.localProviders", { count: availableLocal.length })}</span>}
            ariaLabel={t("harness.localProviders", { count: availableLocal.length })}
            bodyClassName="rounded-b-md border-t border-zinc-300 bg-panel-inset dark:border-zinc-700"
          >
            <ListBox variant="plain">
              {availableLocal.map((item) => {
                const providerId = item.id;
                const providerName = item.name || providerId;
                return (
                  <ListRow
                    key={providerId}
                    surface="inset"
                    trailing={
                      <button
                        type="button"
                        onClick={() => onConnect(providerId, item)}
                        className="rounded-md bg-zinc-100 px-3 py-1 text-xs font-medium text-zinc-700 hover:bg-zinc-200 dark:bg-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-600"
                      >
                        {t("harness.connect")}
                      </button>
                    }
                  >
                    <span className="block truncate text-xs font-medium text-zinc-700 dark:text-zinc-300">{providerName}</span>
                  </ListRow>
                );
              })}
            </ListBox>
          </ExpandableRow>
        </ListBox>
      )}

      {/* ── Remote Providers — level-1 collapsible group with in-header search ── */}
      {remoteProviders.length > 0 && (
        <ListBox dividers={false}>
          <ExpandableRow
            open={remoteOpen}
            onToggle={() => setRemoteOpen((v) => !v)}
            title={<span className="flex items-center gap-1">☁️ {t("harness.remoteProviders", { count: providerSearchTerm.trim() ? `${filteredRemoteProviders.length}/${availableRemote}` : availableRemote })}</span>}
            ariaLabel={t("harness.remoteProviders", { count: providerSearchTerm.trim() ? `${filteredRemoteProviders.length}/${availableRemote}` : availableRemote })}
            trailing={
              <span onClick={(e) => e.stopPropagation()}>
                <div className="relative">
                  <StyledInput
                    type="text"
                    value={providerSearchTerm}
                    onChange={(e) => setProviderSearchTerm(e.target.value)}
                    placeholder={t("harness.searchProviders")}
                    className="w-[170px] bg-modal-surface pl-7 pr-2 placeholder-zinc-400 dark:border-zinc-600 dark:placeholder-zinc-500"
                  />
                  <Search className="pointer-events-none absolute left-2 top-1/2 -translate-y-1/2 h-3.5 w-3.5 text-zinc-400" />
                </div>
              </span>
            }
            bodyClassName="rounded-b-md border-t border-zinc-300 bg-panel-inset dark:border-zinc-700"
          >
            {providerSearchTerm.trim() && filteredRemoteProviders.length === 0 ? (
              <div className="py-3 text-center text-xs text-zinc-400">
                {t("harness.noProvidersMatch")}
              </div>
            ) : (
              <>
                <ListBox variant="plain">
                  {displayedRemote.map((item) => {
                    const providerId = item.id;
                    const providerName = item.name || providerId;
                    const modelCount = item.model_count;
                    return (
                      <ListRow
                        key={providerId}
                        surface="inset"
                        trailing={
                          <button
                            type="button"
                            onClick={() => onConnect(providerId, item)}
                            className="rounded-md bg-zinc-100 px-3 py-1 text-xs font-medium text-zinc-700 hover:bg-zinc-200 dark:bg-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-600"
                          >
                            {t("harness.addKey")}
                          </button>
                        }
                      >
                        <span className="block truncate text-xs font-medium text-zinc-700 dark:text-zinc-300">{providerName}</span>
                        {modelCount != null && (
                          <span className="mt-0.5 block text-[10px] text-zinc-400">{t("harness.modelsAvailable", { count: modelCount })}</span>
                        )}
                      </ListRow>
                    );
                  })}
                </ListBox>
                {remoteLimitReached && (
                  <div className="px-3 pb-2 pt-1.5">
                    <button
                      type="button"
                      onClick={() => setShowAllRemote(true)}
                      className="flex w-full items-center justify-center gap-1 rounded-md border border-dashed border-zinc-300 py-2 text-xs text-zinc-500 transition-colors hover:border-zinc-400 hover:text-zinc-700 dark:border-zinc-600 dark:text-zinc-400 dark:hover:border-zinc-500 dark:hover:text-zinc-300"
                    >
                      <ChevronsDown className="h-4 w-4" />
                      <>Show all ({filteredRemoteProviders.length})</>
                    </button>
                  </div>
                )}
              </>
            )}
          </ExpandableRow>
        </ListBox>
      )}
    </div>
  );
}
