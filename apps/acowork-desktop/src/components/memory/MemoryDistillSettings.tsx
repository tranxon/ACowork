import { useCallback, useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { VaultKeyEntry, DistillerStatus } from "../../lib/types";
import { fetchProviders } from "../../lib/gateway-api";
import { getGatewayUrl } from "../../lib/config";
import { useTranslation } from "../../i18n/useTranslation";
import { useToast } from "../common/ToastProvider";
import { Switch } from "../common/Switch";
import { Dropdown } from "../common/Dropdown";
import { StyledInput } from "../common/StyledInput";
import { ListBox, ExpandableRow } from "../common/list";
import { log } from "../../lib/logger";
import { with503Retry } from "../../lib/httpRetry";

/**
 * "记忆蒸馏" settings card — top of the memory panel (ADR-071 D3/D5).
 *
 * Reads/writes the five `agent_config.json` distiller fields through the
 * same `GET/PUT /agents/{id}/config` contract the Agent Setup panel uses:
 * the card only ever touches `agent_config.json` (runtime layer 1);
 * manifest `[memory.distiller]` stays the first-run seed / fallback.
 *
 * - Enabled switch → `distiller_enabled`
 * - Model pick → `distiller_model` (`{provider_id, model_id}`), same
 *   provider/model option source as the Harness compact-model card
 *   (vault keys + provider display names).
 * - Interval / accumulation / idle inputs → the three trigger fields.
 *
 * Empty number inputs mean "no runtime opinion" — the field is not sent
 * (fallback: manifest → system default). Blur saves the numeric fields;
 * the switch and model pick save immediately on change.
 *
 * Visual grammar: level-1 collapsible card (same shell as the Status-tab
 * Session Status / Tools-tab Builtin Tools / Debug-tab Context Snapshots
 * / Memory tab node list). The header is chevron + title + trailing
 * Switch — nothing else, so the row matches every other level-1 card
 * visually. The Switch owns the enable bit AND auto-expands the body
 * when enabled / auto-collapses when disabled (`handleToggle`), so the
 * user never has to first flip the switch then click the title row.
 * The backlog / last-run hint is intentionally rendered INSIDE the
 * expanded body — it is runtime status, not card chrome.
 */
export function MemoryDistillSettings({
  agentId,
  running,
  distillerStatus,
}: {
  agentId: string | null;
  running: boolean;
  distillerStatus: DistillerStatus | null;
}) {
  const { t } = useTranslation();
  const { addToast } = useToast();

  const [loaded, setLoaded] = useState(false);
  const [enabled, setEnabled] = useState(false);
  const [modelKey, setModelKey] = useState(""); // "provider::model", "" = unset
  const [intervalInput, setIntervalInput] = useState("");
  const [accInput, setAccInput] = useState("");
  const [idleInput, setIdleInput] = useState("");
  const [expanded, setExpanded] = useState(false);
  const [savingField, setSavingField] = useState<string | null>(null);

  // ── Load the runtime distiller fields from agent_config.json ─────────
  const loadConfig = useCallback(async () => {
    if (!agentId || !running) return;
    try {
      const res = await with503Retry(
        () => fetch(`${getGatewayUrl()}/api/agents/${agentId}/config`),
        { tag: `MemoryDistillSettings.loadConfig(${agentId})`, logger: log },
      );
      if (!res.ok) return;
      const data = await res.json();
      // Same nested envelope as AgentSetupTab: `{ agent_id, config, … }`.
      const cfg = (data?.config ?? {}) as {
        distiller_enabled?: boolean | null;
        distiller_model?: { provider_id: string; model_id: string } | null;
        distiller_interval_minutes?: number | null;
        distiller_accumulation_threshold?: number | null;
        distiller_idle_minutes?: number | null;
      };
      setEnabled(!!cfg.distiller_enabled);
      setModelKey(
        cfg.distiller_model
          ? `${cfg.distiller_model.provider_id}::${cfg.distiller_model.model_id}`
          : "",
      );
      setIntervalInput(
        cfg.distiller_interval_minutes != null
          ? String(cfg.distiller_interval_minutes)
          : "",
      );
      setAccInput(
        cfg.distiller_accumulation_threshold != null
          ? String(cfg.distiller_accumulation_threshold)
          : "",
      );
      setIdleInput(
        cfg.distiller_idle_minutes != null
          ? String(cfg.distiller_idle_minutes)
          : "",
      );
    } catch {
      // Best-effort: runtime not reachable yet; keep last values.
    } finally {
      setLoaded(true);
    }
  }, [agentId, running]);

  useEffect(() => {
    setLoaded(false);
    void loadConfig();
  }, [loadConfig]);

  // ── Model options: vault keys + provider display names ───────────────
  // Same data source as `GlobalCompactModelCard` (Harness tab): vault
  // entries define which (provider, model) pairs the user can actually
  // select; `/api/models` supplies human-readable provider names.
  const [options, providerNames] = useModelOptions();

  const modelDropdownOptions = useMemo(() => {
    const sep = "\u2003\u00b7\u2003";
    return options.map((o) => ({
      value: o.key,
      label: `${o.modelId}${sep}${providerNames.get(o.providerId) ?? o.providerId}`,
    }));
  }, [options, providerNames]);

  const selectedModelStale =
    modelKey !== "" && !options.some((o) => o.key === modelKey);

  // ── PUT a single distiller field to agent_config.json ────────────────
  const putField = useCallback(
    async (field: string, value: unknown) => {
      if (!agentId) return;
      setSavingField(field);
      try {
        const res = await fetch(
          `${getGatewayUrl()}/api/agents/${agentId}/config`,
          {
            method: "PUT",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ [field]: value }),
          },
        );
        if (!res.ok) {
          addToast({
            type: "error",
            message: t("memoryPanel.distillerSaveFailed", {
              status: res.status,
            }),
          });
        }
      } catch (e) {
        log.warn("[MemoryDistillSettings] save failed:", field, e);
        addToast({
          type: "error",
          message: t("memoryPanel.distillerSaveFailed", { status: "network" }),
        });
      } finally {
        setSavingField(null);
      }
    },
    [agentId, addToast, t],
  );

  const handleToggle = (v: boolean) => {
    setEnabled(v);
    // Switch owns the enable bit, so it should also own the affordance
    // of showing / hiding the configuration body: enabling auto-expands
    // so the user lands on the controls they just turned on; disabling
    // auto-collapses because the body becomes inert (all inputs are
    // disabled when `!enabled` anyway).
    setExpanded(v);
    void putField("distiller_enabled", v);
  };

  const handleModelChange = (raw: string) => {
    setModelKey(raw);
    if (raw === "") {
      // Explicit clear → agent_config field removed (manifest/default fallback).
      void putField("distiller_model", null);
      return;
    }
    const o = options.find((opt) => opt.key === raw);
    if (o) {
      void putField("distiller_model", {
        provider_id: o.providerId,
        model_id: o.modelId,
      });
    }
  };

  const saveNumber = (
    field: string,
    raw: string,
    setter: (v: string) => void,
  ) => {
    setter(raw);
    const trimmed = raw.trim();
    if (trimmed === "") {
      // Empty → clear runtime opinion (fall back to manifest / defaults).
      void putField(field, null);
      return;
    }
    const n = Number(trimmed);
    if (!Number.isFinite(n) || n <= 0) return; // keep last value; do not persist junk
    void putField(field, n);
  };

  // ── Render ────────────────────────────────────────────────────────────
  const numInputCls =
    "rounded-md border border-zinc-200 bg-modal-surface px-2 py-1 text-[11px] outline-none focus:border-[var(--color-accent)] dark:border-zinc-700 dark:text-zinc-200";

  // Backlog / last-run hint lives INSIDE the expanded body — it is
  // status info about the runtime, not metadata about the card itself,
  // and only meaningful once the user has opted in to the distiller.
  // Showing it in the title row made the header feel busier than its
  // peers (Session Status, Builtin Tools, Context Snapshots, Memory
  // Nodes), which keep the title row to chevron + title + trailing.
  const runtimeHint =
    loaded && distillerStatus ? (
      <p className="flex items-center gap-1.5 text-[10px] text-zinc-400 dark:text-zinc-500">
        <span>
          {t("memoryPanel.distillerBacklog", {
            count: distillerStatus.episode_backlog,
          })}
        </span>
        <span aria-hidden>·</span>
        {distillerStatus.last_run ? (
          <span>
            {t("memoryPanel.distillerLastRun", {
              scanned: distillerStatus.last_run.episodes_scanned,
              promoted: distillerStatus.last_run.total_promoted,
            })}
          </span>
        ) : (
          <span>{t("memoryPanel.distillerNeverRun")}</span>
        )}
      </p>
    ) : null;

  return (
    // Level-1 collapsible card in the same grammar as Session Status /
    // Builtin Tools / Context Snapshots / Memory Nodes: `ListBox` shell
    // + `ExpandableRow`. The title row stays clean (chevron + title +
    // trailing Switch) so it visually matches every other level-1
    // card on the right panel. The trailing Switch owns enable AND the
    // auto expand/collapse (see `handleToggle`), so the user never has
    // to first enable then click the title row.
    //
    // `border-b` on the wrapper is the shared vertical hairline that
    // separates this card from the 记忆遗忘 card below — same zinc-200 /
    // zinc-800 tone as the stats-grid hairline at the top of the panel
    // and the hairline between 记忆遗忘 and 记忆搜索. The wrapper owns
    // its own border (not the inner ListBox) so the line spans the
    // full panel width and the spacing above / below the hairline is
    // always the wrapper's `p-3` on each side — uniform across the
    // three cards.
    <div className="border-b border-zinc-200 p-3 dark:border-zinc-800">
      <ListBox dividers={false}>
        <ExpandableRow
          open={expanded}
          onToggle={() => setExpanded((v) => !v)}
          title={t("memoryPanel.distillerTitle")}
          ariaLabel={t("memoryPanel.distillerTitle")}
          trailing={
            // The Switch component does not stop propagation itself;
            // ExpandableRow's contract requires trailing interactive
            // elements to swallow the click so toggling does not also
            // collapse/expand the card.
            <span onClick={(e) => e.stopPropagation()}>
              <Switch
                checked={enabled}
                onChange={handleToggle}
                disabled={!running || savingField === "distiller_enabled"}
                size="sm"
                aria-label={t("memoryPanel.distillerEnabled")}
              />
            </span>
          }
          bodyClassName="rounded-b-md border-t border-zinc-300 bg-panel-inset px-3 py-2 dark:border-zinc-700"
        >
          <div className="flex flex-col gap-2">
            {runtimeHint}
            <label className="flex flex-col gap-1">
              <span className="text-[10px] text-zinc-500 dark:text-zinc-400">
                {t("memoryPanel.distillerModel")}
              </span>
              <Dropdown
                className="!py-1 text-[11px]"
                value={modelKey}
                onChange={handleModelChange}
                disabled={!running || !enabled || savingField === "distiller_model"}
                placeholder={{
                  value: "",
                  label: t("memoryPanel.distillerModelPlaceholder"),
                  selectable: true,
                }}
                options={[
                  ...modelDropdownOptions,
                  ...(selectedModelStale && modelKey
                    ? [
                        {
                          value: modelKey,
                          label: `${modelKey.split("::")[1]} · ${modelKey.split("::")[0]}`,
                        },
                      ]
                    : []),
                ]}
              />
            </label>

            <div className="grid grid-cols-3 gap-2">
              <label className="flex flex-col gap-1">
                <span className="text-[10px] text-zinc-500 dark:text-zinc-400">
                  {t("memoryPanel.distillerInterval")}
                </span>
                <StyledInput
                  type="number"
                  min={1}
                  value={intervalInput}
                  placeholder={String(60)}
                  onChange={(e) => setIntervalInput(e.target.value)}
                  onBlur={(e) =>
                    saveNumber(
                      "distiller_interval_minutes",
                      e.target.value,
                      setIntervalInput,
                    )
                  }
                  disabled={!running || !enabled}
                  className={numInputCls}
                />
              </label>
              <label className="flex flex-col gap-1">
                <span className="text-[10px] text-zinc-500 dark:text-zinc-400">
                  {t("memoryPanel.distillerAccumulation")}
                </span>
                <StyledInput
                  type="number"
                  min={1}
                  value={accInput}
                  placeholder={String(50)}
                  onChange={(e) => setAccInput(e.target.value)}
                  onBlur={(e) =>
                    saveNumber(
                      "distiller_accumulation_threshold",
                      e.target.value,
                      setAccInput,
                    )
                  }
                  disabled={!running || !enabled}
                  className={numInputCls}
                />
              </label>
              <label className="flex flex-col gap-1">
                <span className="text-[10px] text-zinc-500 dark:text-zinc-400">
                  {t("memoryPanel.distillerIdle")}
                </span>
                <StyledInput
                  type="number"
                  min={1}
                  value={idleInput}
                  placeholder={String(30)}
                  onChange={(e) => setIdleInput(e.target.value)}
                  onBlur={(e) =>
                    saveNumber("distiller_idle_minutes", e.target.value, setIdleInput)
                  }
                  disabled={!running || !enabled}
                  className={numInputCls}
                />
              </label>
            </div>

            {!enabled && (
              <p className="text-[10px] text-zinc-400 dark:text-zinc-500">
                {t("memoryPanel.distillerDisabledHint")}
              </p>
            )}
          </div>
        </ExpandableRow>
      </ListBox>
    </div>
  );
}

/** Vault (provider, model) options + provider display-name map. */
function useModelOptions(): [
  { key: string; providerId: string; modelId: string }[],
  Map<string, string>,
] {
  const [options, setOptions] = useState<
    { key: string; providerId: string; modelId: string }[]
  >([]);
  const [names, setNames] = useState<Map<string, string>>(new Map());

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        // Provider display names (Gateway models cache).
        const providers = await fetchProviders();
        if (cancelled) return;
        const nameMap = new Map<string, string>();
        for (const p of providers) nameMap.set(p.id, p.name);
        setNames(nameMap);
      } catch {
        // Gateway unreachable — fall back to raw provider ids in labels.
      }
      try {
        // Configured (provider, model) pairs from the vault — same source
        // as the Harness compact-model card.
        const keys = await invoke<VaultKeyEntry[]>("list_keys");
        if (cancelled) return;
        const opts: { key: string; providerId: string; modelId: string }[] = [];
        for (const k of keys) {
          const modelIds =
            k.models && k.models.length > 0
              ? k.models
              : k.default_model
                ? [k.default_model]
                : [];
          for (const modelId of modelIds) {
            opts.push({
              key: `${k.provider}::${modelId}`,
              providerId: k.provider,
              modelId,
            });
          }
        }
        setOptions(opts);
      } catch {
        // Tauri bridge unavailable (web preview) — dropdown stays empty;
        // the agent_config value is still shown via the stale fallback.
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  return [options, names];
}
