import type { ModelCapabilitiesInfo } from "../../lib/types";
import { StyledInput } from "../common/StyledInput";
import { useTranslation } from "../../i18n/useTranslation";

interface ModelCapEditorProps {
  modelId: string;
  caps: ModelCapabilitiesInfo;
  expanded: boolean;
  onToggle: () => void;
  onUpdate: (field: keyof ModelCapabilitiesInfo, value: unknown) => void;
}

const INPUT_MODALITIES = ["text", "image", "audio", "video"] as const;
const OUTPUT_MODALITIES = ["text", "image"] as const;

/** Reusable expandable per-model capability editor card. */
export function ModelCapEditor({
  modelId,
  caps,
  expanded,
  onToggle,
  onUpdate,
}: ModelCapEditorProps) {
  const { t } = useTranslation();

  return (
    <div className="rounded border border-zinc-200 dark:border-zinc-700">
      <button
        type="button"
        onClick={onToggle}
        className="flex w-full items-center gap-1 px-2 py-1.5 text-xs text-zinc-600 dark:text-zinc-300"
      >
        <span className="text-zinc-400">{expanded ? "\u25BC" : "\u25B6"}</span>
        <span className="flex-1 truncate text-left">{modelId}</span>
        <span className="text-zinc-400">{caps.context_window ? `${(caps.context_window / 1000).toFixed(0)}K ctx` : ""}</span>
      </button>
      {expanded && (
        <div className="border-t border-zinc-200 px-2 py-2 dark:border-zinc-700">
          <div className="flex gap-2">
            <div className="flex-1">
              <label className="mb-0.5 block text-xs text-zinc-400">{t("harness.contextWindow")}</label>
              <StyledInput
                type="number"
                value={caps.context_window?.toString() ?? ""}
                onChange={(e) => onUpdate("context_window", parseInt(e.target.value) || 0)}
                placeholder="e.g. 128000"
                className="dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
              />
            </div>
            <div className="flex-1">
              <label className="mb-0.5 block text-xs text-zinc-400">{t("harness.maxOutputTokens")}</label>
              <StyledInput
                type="number"
                value={caps.max_output_tokens?.toString() ?? ""}
                onChange={(e) => onUpdate("max_output_tokens", parseInt(e.target.value) || 0)}
                placeholder="e.g. 16384"
                className="dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
              />
            </div>
          </div>
          <div className="mt-1.5 flex flex-wrap items-center gap-3">
            <label className="flex items-center gap-1.5 text-xs text-zinc-500">
              <input
                type="checkbox"
                checked={caps.supports_tool_calling ?? false}
                onChange={(e) => onUpdate("supports_tool_calling", e.target.checked)}
                className="accent-[var(--color-accent)]"
              />
              {t("harness.supportsToolCalling")}
            </label>
            <label className="flex items-center gap-1.5 text-xs text-zinc-500">
              <input
                type="checkbox"
                checked={caps.supports_reasoning ?? false}
                onChange={(e) => onUpdate("supports_reasoning", e.target.checked)}
                className="accent-[var(--color-accent)]"
              />
              {t("harness.reasoning")}
            </label>
          </div>
          <div className="mt-1.5 flex gap-4">
            <div>
              <label className="mb-0.5 block text-xs text-zinc-400">Input</label>
              <div className="flex gap-2">
                {INPUT_MODALITIES.map(mod => (
                  <label key={mod} className="flex items-center gap-1 text-xs text-zinc-500">
                    <input
                      type="checkbox"
                      checked={caps.modalities?.input?.includes(mod) ?? false}
                      onChange={(e) => {
                        const current = caps.modalities?.input ?? [];
                        const nextMod = e.target.checked
                          ? [...current, mod]
                          : current.filter(m => m !== mod);
                        onUpdate("modalities", { ...caps.modalities, input: nextMod });
                      }}
                      className="accent-[var(--color-accent)]"
                    />
                    {mod}
                  </label>
                ))}
              </div>
            </div>
            <div>
              <label className="mb-0.5 block text-xs text-zinc-400">Output</label>
              <div className="flex gap-2">
                {OUTPUT_MODALITIES.map(mod => (
                  <label key={mod} className="flex items-center gap-1 text-xs text-zinc-500">
                    <input
                      type="checkbox"
                      checked={caps.modalities?.output?.includes(mod) ?? false}
                      onChange={(e) => {
                        const current = caps.modalities?.output ?? [];
                        const nextMod = e.target.checked
                          ? [...current, mod]
                          : current.filter(m => m !== mod);
                        onUpdate("modalities", { ...caps.modalities, output: nextMod });
                      }}
                      className="accent-[var(--color-accent)]"
                    />
                    {mod}
                  </label>
                ))}
              </div>
            </div>
          </div>
          {caps.supports_reasoning && (
            <div className="mt-1.5">
              <label className="mb-0.5 block text-xs text-zinc-400">{t("harness.defaultReasoningEffort")}</label>
              <select
                value={caps.default_reasoning_effort ?? "auto"}
                onChange={(e) => onUpdate("default_reasoning_effort", e.target.value)}
                className="w-full appearance-none rounded border border-zinc-200 bg-modal-surface px-2.5 py-1.5 text-xs text-zinc-800 outline-none transition-colors focus:border-[var(--color-accent)] dark:border-zinc-700 dark:text-zinc-200"
              >
                <option value="auto">Auto</option>
                <option value="off">Off</option>
                <option value="low">Low</option>
                <option value="medium">Medium</option>
                <option value="high">High</option>
              </select>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
