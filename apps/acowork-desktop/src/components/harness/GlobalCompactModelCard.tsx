import { useCallback, useEffect, useMemo, useState } from "react";
import type {
  VaultKeyEntry,
  ProviderListEntry,
  CompactModelRef,
} from "../../lib/types";
import { getDefaultCompactModel, setDefaultCompactModel } from "../../lib/gateway-api";
import { cn } from "../../lib/utils";
import { useTranslation } from "../../i18n/useTranslation";
import { Dropdown } from "../common/Dropdown";
import { useToast } from "../common/ToastProvider";

export interface GlobalCompactModelCardProps {
  /** Configured providers (used to build the option list). */
  keys: VaultKeyEntry[];
  /** Available provider entries (name, model_count). */
  providers: ProviderListEntry[];
}

/**
 * "Global default compact model" — top of the Providers Tab.
 *
 * Holds a `provider_id::model_id` pick at the `provider_list.json` top
 * level (`default_compact_model`), independent of any single provider's
 * `compact_model`. Persistence:
 *
 *   PUT /api/settings/default-compact-model  →  Gateway writes provider_list.json
 *                                            →  MQTT republish triggers
 *                                            →  Runtimes refresh AgentCore.default_compact_model
 *
 * UX: a single-row layout with a native `<select>`. Picking an option
 * immediately PUTs the new value (optimistic update, rollback on error).
 * Selecting the empty "(not configured)" option clears the setting.
 */
export function GlobalCompactModelCard({
  keys,
  providers,
}: GlobalCompactModelCardProps) {
  const { t } = useTranslation();
  const { addToast } = useToast();

  const [current, setCurrent] = useState<CompactModelRef | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);

  const refresh = useCallback(async () => {
    try {
      const v = await getDefaultCompactModel();
      setCurrent(v);
    } catch {
      // Gateway may be down — leave current null, UI stays editable
      setCurrent(null);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  // Build (provider, model) options from configured keys, in stable order.
  const options = useMemo(() => {
    const out: { key: string; providerId: string; modelId: string }[] = [];
    for (const k of keys) {
      const modelIds =
        k.models && k.models.length > 0
          ? k.models
          : k.default_model
            ? [k.default_model]
            : [];
      for (const modelId of modelIds) {
        out.push({
          key: `${k.provider}::${modelId}`,
          providerId: k.provider,
          modelId,
        });
      }
    }
    return out;
  }, [keys]);

  const providerNameById = useMemo(() => {
    const m = new Map<string, string>();
    for (const p of providers) m.set(p.id, p.name);
    return m;
  }, [providers]);

  const handleChange = async (raw: string) => {
    // Optimistic update
    const next: CompactModelRef | null = raw
      ? (() => {
          const o = options.find((opt) => opt.key === raw);
          return o ? { provider_id: o.providerId, model_id: o.modelId } : null;
        })()
      : null;

    // Skip if unchanged
    if (
      (current?.provider_id ?? null) === (next?.provider_id ?? null) &&
      (current?.model_id ?? null) === (next?.model_id ?? null)
    ) {
      return;
    }

    const previous = current;
    setCurrent(next);
    setSaving(true);
    try {
      const persisted = await setDefaultCompactModel(next);
      setCurrent(persisted);
    } catch (e) {
      // Rollback
      setCurrent(previous);
      addToast({
        type: "error",
        message: t("harness.globalCompactModel.saveFailed", {
          error: e instanceof Error ? e.message : String(e),
        }),
      });
    } finally {
      setSaving(false);
    }
  };

  const selectedKey = current
    ? `${current.provider_id}::${current.model_id}`
    : "";

  // If the persisted pick is no longer present in options (provider removed),
  // expose it as a synthetic option so React's <select> doesn't auto-snap
  // to the empty placeholder and trigger a bogus onChange (which would
  // wipe the user's saved setting).
  const currentIsStale =
    !!current && !options.some((o) => o.key === selectedKey);

  // Render "(not configured)" placeholder option.
  const noneLabel = t("harness.globalCompactModel.noneLabel");
  const unavailableLabel = t("harness.globalCompactModel.unavailable");
  // Use an em-space on each side of the "·" separator so model and provider
  // don't visually collide inside the native <option> row. Native option
  // text is rendered by the browser without CSS, so we lean on Unicode
  // whitespace (U+2003) to widen the gap.
  const sep = "\u2003\u00b7\u2003";

  return (
    <div className="rounded-md border border-zinc-200 bg-modal-surface px-4 py-3 dark:border-zinc-700">
      <h2 className="text-xs font-medium">
        {t("harness.globalCompactModel.title")}
      </h2>
      <p className="mt-0.5 text-[11px] text-zinc-500 dark:text-zinc-400">
        {t("harness.globalCompactModel.description")}
      </p>
      <div className="mt-2">
        <Dropdown
          className={cn(saving && "opacity-60")}
          value={selectedKey}
          onChange={(v) => handleChange(v)}
          disabled={loading || saving}
          placeholder={{ value: "", label: noneLabel }}
          options={[
            ...options.map((o) => ({
              value: o.key,
              label: `${o.modelId}${sep}${providerNameById.get(o.providerId) ?? o.providerId}`,
            })),
            ...(currentIsStale && current
              ? [{
                  value: selectedKey,
                  label: `${current.model_id}${sep}${current.provider_id} (${unavailableLabel})`,
                }]
              : []),
          ]}
        />
      </div>
    </div>
  );
}