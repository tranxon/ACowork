import { useCallback, useEffect, useState } from "react";
import type { ForgettingStatus } from "../../lib/types";
import { getGatewayUrl } from "../../lib/config";
import { useTranslation } from "../../i18n/useTranslation";
import { useToast } from "../common/ToastProvider";
import { Switch } from "../common/Switch";
import { StyledInput } from "../common/StyledInput";
import { ListBox, ExpandableRow } from "../common/list";
import { log } from "../../lib/logger";
import { with503Retry } from "../../lib/httpRetry";

/**
 * "记忆遗忘" settings card — right below the 记忆沉淀 card (ADR-057 §5.3
 * redesign).
 *
 * Reads/writes the four `agent_config.json` forgetting fields through the
 * same `GET/PUT /agents/{id}/config` contract as the distill card; the
 * card only ever touches the runtime layer (agent_config.json), never the
 * manifest.
 *
 * - Enabled switch → `memory_forgetting_enabled` (default off — forgetting
 *   is opt-in).
 * - Half-life (days) → `memory_forgetting_half_life_days` (default 180).
 *   This is THE core parameter: before the half-life an episodic node
 *   keeps (almost) full retrieval weight; after it the node is
 *   progressively down-weighted (`retention = exp(-ln2 · age/half_life)`).
 * - Dormant threshold → `memory_forgetting_dormant_threshold` (0–1,
 *   default 0.1). When retention falls below this the node transitions
 *   Active → Dormant and is excluded from retrieval.
 * - Archive days → `memory_forgetting_archive_days` (default 90). A
 *   Dormant node older than this is archived to the PurgeLog (30-day
 *   recovery window).
 *
 * The sediment layer (Knowledge / Procedural / Autobiographical) is
 * intentionally never decayed — this card only governs episodic nodes.
 *
 * Empty number inputs mean "no runtime opinion" — the field is not sent
 * (fallback: system default). Blur saves the numeric fields; the switch
 * saves immediately on change.
 */
export function MemoryForgettingSettings({
  agentId,
  running,
  forgettingStatus,
}: {
  agentId: string | null;
  running: boolean;
  forgettingStatus: ForgettingStatus | null;
}) {
  const { t } = useTranslation();
  const { addToast } = useToast();

  const [loaded, setLoaded] = useState(false);
  const [enabled, setEnabled] = useState(false);
  const [halfLifeInput, setHalfLifeInput] = useState("");
  const [dormantInput, setDormantInput] = useState("");
  const [archiveInput, setArchiveInput] = useState("");
  const [expanded, setExpanded] = useState(false);
  const [savingField, setSavingField] = useState<string | null>(null);

  // ── Load the runtime forgetting fields from agent_config.json ────────
  const loadConfig = useCallback(async () => {
    if (!agentId || !running) return;
    try {
      const res = await with503Retry(
        () => fetch(`${getGatewayUrl()}/api/agents/${agentId}/config`),
        { tag: `MemoryForgettingSettings.loadConfig(${agentId})`, logger: log },
      );
      if (!res.ok) return;
      const data = await res.json();
      // Same nested envelope as the distill card: `{ agent_id, config, … }`.
      const cfg = (data?.config ?? {}) as {
        memory_forgetting_enabled?: boolean | null;
        memory_forgetting_half_life_days?: number | null;
        memory_forgetting_dormant_threshold?: number | null;
        memory_forgetting_archive_days?: number | null;
      };
      setEnabled(!!cfg.memory_forgetting_enabled);
      setHalfLifeInput(
        cfg.memory_forgetting_half_life_days != null
          ? String(cfg.memory_forgetting_half_life_days)
          : "",
      );
      setDormantInput(
        cfg.memory_forgetting_dormant_threshold != null
          ? String(cfg.memory_forgetting_dormant_threshold)
          : "",
      );
      setArchiveInput(
        cfg.memory_forgetting_archive_days != null
          ? String(cfg.memory_forgetting_archive_days)
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

  // ── PUT a single forgetting field to agent_config.json ───────────────
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
            message: t("memoryPanel.forgettingSaveFailed", {
              status: res.status,
            }),
          });
        }
      } catch (e) {
        log.warn("[MemoryForgettingSettings] save failed:", field, e);
        addToast({
          type: "error",
          message: t("memoryPanel.forgettingSaveFailed", { status: "network" }),
        });
      } finally {
        setSavingField(null);
      }
    },
    [agentId, addToast, t],
  );

  const handleToggle = (v: boolean) => {
    setEnabled(v);
    // The switch owns the enable bit AND the expand/collapse affordance
    // (same grammar as the distill card).
    setExpanded(v);
    void putField("memory_forgetting_enabled", v);
  };

  const saveNumber = (
    field: string,
    raw: string,
    setter: (v: string) => void,
  ) => {
    setter(raw);
    const trimmed = raw.trim();
    if (trimmed === "") {
      // Empty → clear runtime opinion (fall back to system defaults).
      void putField(field, null);
      return;
    }
    const n = Number(trimmed);
    if (!Number.isFinite(n) || n <= 0) return; // keep last value; do not persist junk
    if (field === "memory_forgetting_dormant_threshold" && n >= 1) return; // must be < 1
    void putField(field, n);
  };

  // ── Render ────────────────────────────────────────────────────────────
  const numInputCls =
    "rounded-md border border-zinc-200 bg-modal-surface px-2 py-1 text-[11px] outline-none focus:border-[var(--color-accent)] dark:border-zinc-700 dark:text-zinc-200";

  const runtimeHint =
    loaded && forgettingStatus ? (
      <p className="flex items-center gap-1.5 text-[10px] text-zinc-400 dark:text-zinc-500">
        <span>
          {forgettingStatus.enabled
            ? t("memoryPanel.forgettingTitle")
            : t("memoryPanel.forgettingDisabledHint")}
        </span>
      </p>
    ) : null;

  return (
    <div className="p-3">
      <ListBox dividers={false}>
        <ExpandableRow
          open={expanded}
          onToggle={() => setExpanded((v) => !v)}
          title={t("memoryPanel.forgettingTitle")}
          ariaLabel={t("memoryPanel.forgettingTitle")}
          trailing={
            <span onClick={(e) => e.stopPropagation()}>
              <Switch
                checked={enabled}
                onChange={handleToggle}
                disabled={!running || savingField === "memory_forgetting_enabled"}
                size="sm"
                aria-label={t("memoryPanel.forgettingEnabled")}
              />
            </span>
          }
          bodyClassName="rounded-b-md border-t border-zinc-300 bg-panel-inset px-3 py-2 dark:border-zinc-700"
        >
          <div className="flex flex-col gap-2">
            {runtimeHint}
            <p className="text-[10px] leading-relaxed text-zinc-500 dark:text-zinc-400">
              {t("memoryPanel.forgettingDisabledHint")}
            </p>

            <div className="grid grid-cols-3 gap-2">
              <label className="flex flex-col gap-1">
                <span className="text-[10px] text-zinc-500 dark:text-zinc-400">
                  {t("memoryPanel.forgettingHalfLife")}
                </span>
                <StyledInput
                  type="number"
                  min={1}
                  value={halfLifeInput}
                  placeholder={String(180)}
                  onChange={(e) => setHalfLifeInput(e.target.value)}
                  onBlur={(e) =>
                    saveNumber(
                      "memory_forgetting_half_life_days",
                      e.target.value,
                      setHalfLifeInput,
                    )
                  }
                  disabled={!running || !enabled}
                  className={numInputCls}
                />
              </label>
              <label className="flex flex-col gap-1">
                <span className="text-[10px] text-zinc-500 dark:text-zinc-400">
                  {t("memoryPanel.forgettingDormantThreshold")}
                </span>
                <StyledInput
                  type="number"
                  min={0}
                  max={0.99}
                  step={0.05}
                  value={dormantInput}
                  placeholder={String(0.1)}
                  onChange={(e) => setDormantInput(e.target.value)}
                  onBlur={(e) =>
                    saveNumber(
                      "memory_forgetting_dormant_threshold",
                      e.target.value,
                      setDormantInput,
                    )
                  }
                  disabled={!running || !enabled}
                  className={numInputCls}
                />
              </label>
              <label className="flex flex-col gap-1">
                <span className="text-[10px] text-zinc-500 dark:text-zinc-400">
                  {t("memoryPanel.forgettingArchiveDays")}
                </span>
                <StyledInput
                  type="number"
                  min={1}
                  value={archiveInput}
                  placeholder={String(90)}
                  onChange={(e) => setArchiveInput(e.target.value)}
                  onBlur={(e) =>
                    saveNumber(
                      "memory_forgetting_archive_days",
                      e.target.value,
                      setArchiveInput,
                    )
                  }
                  disabled={!running || !enabled}
                  className={numInputCls}
                />
              </label>
            </div>
          </div>
        </ExpandableRow>
      </ListBox>
    </div>
  );
}
