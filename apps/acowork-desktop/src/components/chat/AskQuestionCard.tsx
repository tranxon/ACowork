import { useEffect, useState } from "react";
import { MessageCircleQuestion } from "lucide-react";
import type { AskQuestionEvent } from "../../lib/types";
import { StyledTextarea } from "../common/StyledInput";
import { useTranslation } from "../../i18n/useTranslation";

interface AskQuestionCardProps {
  event: AskQuestionEvent;
  agentId: string;
  sessionId?: string | null;
  onAnswer: (requestId: string, answer: string) => void;
}

/**
 * AskQuestionCard: renders an ask_user_question prompt with options + "Other" textarea.
 *
 * Design:
 * - Shows question title (if present) and question text
 * - Options rendered as radio buttons with optional descriptions
 * - Last option is always "Other" which reveals a textarea for free-text input
 * - Submit button uses accent color, inline with the card (no modal/dialog)
 * - Disabled after submission
 * - Shows live countdown badge sourced from `event.timeout_seconds` (server-derived
 *   from `approval_timeout_secs`). Mirrors the approval flow in ExploreBlock.
 */
export function AskQuestionCard({ event, onAnswer }: AskQuestionCardProps) {
  const { t } = useTranslation();
  const [selected, setSelected] = useState<string | null>(null);
  const [otherText, setOtherText] = useState("");
  const [submitted, setSubmitted] = useState(false);

  // Countdown timer for question wait timeout.
  // Mirrors the implementation in ExploreBlock.tsx for tool approvals.
  const [remainingSecs, setRemainingSecs] = useState<number | null>(null);
  useEffect(() => {
    const total = event.timeout_seconds;
    if (!total || total <= 0) {
      setRemainingSecs(null);
      return;
    }
    setRemainingSecs(total);
    const interval = setInterval(() => {
      setRemainingSecs((prev) => {
        if (prev === null || prev <= 1) {
          clearInterval(interval);
          return 0;
        }
        return prev - 1;
      });
    }, 1000);
    return () => clearInterval(interval);
  }, [event.timeout_seconds]);

  const isOtherSelected = selected === "__other__";
  const canSubmit = !submitted && (selected !== null) && (!isOtherSelected || otherText.trim().length > 0);
  const countdownLabel = remainingSecs !== null && remainingSecs > 0
    ? `${Math.floor(remainingSecs / 60)}:${String(remainingSecs % 60).padStart(2, "0")}`
    : remainingSecs === 0 ? "expired" : null;

  const handleSubmit = () => {
    if (!canSubmit) return;
    const answer = isOtherSelected ? otherText.trim() : selected!;
    setSubmitted(true);
    onAnswer(event.request_id, answer);
  };

  return (
    <div
      className="my-1.5 max-w-[var(--content-max-width)] rounded-md border border-zinc-200 bg-zinc-50 px-3 py-2 dark:border-zinc-700 dark:bg-zinc-800/40"
    >
      {/* Header */}
      <div className="flex items-start gap-1.5 mb-1.5">
        <MessageCircleQuestion
          className="h-3.5 w-3.5 shrink-0 mt-0.5"
          style={{ color: "var(--color-accent)" }}
        />
        <div className="min-w-0 flex-1">
          {event.title && (
            <div className="text-xs font-medium text-zinc-800 dark:text-zinc-200 mb-0.5">
              {event.title}
            </div>
          )}
          <div className="text-xs text-zinc-700 dark:text-zinc-300">
            {event.question}
          </div>
        </div>
        {/* Countdown badge — mirrors approval-flow timer in ExploreBlock */}
        {countdownLabel && countdownLabel !== "expired" && (
          <span
            className="text-[10px] font-mono text-amber-600 dark:text-amber-400 shrink-0 min-w-[2.5rem] text-right mt-0.5"
            aria-label={t("askQuestionCard.expiresIn")}
          >
            {countdownLabel}
          </span>
        )}
        {countdownLabel === "expired" && (
          <span className="text-[10px] font-mono text-zinc-400 dark:text-zinc-500 shrink-0 mt-0.5">
            {t("askQuestionCard.expired")}
          </span>
        )}
      </div>

      {/* Options */}
      <div className="ml-5 space-y-0.5">
        {event.options.map((opt, idx) => (
          <label
            key={idx}
            className={`flex items-center gap-1.5 rounded px-2 py-1 text-xs transition-colors cursor-pointer
              ${submitted ? "opacity-60 pointer-events-none" : "hover:bg-zinc-100 dark:hover:bg-zinc-700/50"}
              ${selected === opt.label ? "bg-zinc-100 dark:bg-zinc-700/50" : ""}`}
          >
            <input
              type="radio"
              name={`question-${event.request_id}`}
              value={opt.label}
              checked={selected === opt.label}
              disabled={submitted}
              onChange={() => setSelected(opt.label)}
              className="shrink-0 h-3 w-3"
              style={{ accentColor: "var(--color-accent)" }}
            />
            <span className="font-medium text-zinc-800 dark:text-zinc-200">{opt.label}</span>
            {opt.description && (
              <span className="text-zinc-500 dark:text-zinc-400">— {opt.description}</span>
            )}
          </label>
        ))}

        {/* Other option */}
        <label
          className={`flex items-center gap-1.5 rounded px-2 py-1 text-xs transition-colors cursor-pointer
            ${submitted ? "opacity-60 pointer-events-none" : "hover:bg-zinc-100 dark:hover:bg-zinc-700/50"}
            ${isOtherSelected ? "bg-zinc-100 dark:bg-zinc-700/50" : ""}`}
        >
          <input
            type="radio"
            name={`question-${event.request_id}`}
            value="__other__"
            checked={isOtherSelected}
            disabled={submitted}
            onChange={() => setSelected("__other__")}
            className="shrink-0 h-3 w-3"
            style={{ accentColor: "var(--color-accent)" }}
          />
          <span className="font-medium text-zinc-800 dark:text-zinc-200">{t("askQuestionCard.other")}</span>
        </label>

        {/* Other textarea */}
        {isOtherSelected && !submitted && (
          <StyledTextarea
            className="mt-1 ml-4 border-zinc-300 bg-white dark:border-zinc-600 dark:bg-zinc-800"
            rows={1}
            placeholder={t("askQuestionCard.placeholder")}
            value={otherText}
            onChange={(e) => setOtherText(e.target.value)}
            autoFocus
          />
        )}
      </div>

      {/* Submit button */}
      <div className="ml-5 mt-1.5 flex items-center gap-2">
        <button
          disabled={!canSubmit}
          onClick={handleSubmit}
          className="rounded px-3 py-0.5 text-xs font-medium text-white transition-opacity hover:opacity-90 disabled:opacity-40 disabled:cursor-not-allowed"
          style={{ backgroundColor: "var(--color-accent)" }}
        >
          {submitted ? t("askQuestionCard.submitted") : t("askQuestionCard.submit")}
        </button>
        {submitted && (
          <span className="text-[10px] text-zinc-500 dark:text-zinc-400">
            Answer sent
          </span>
        )}
      </div>
    </div>
  );
}
