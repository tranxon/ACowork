/**
 * Format a millisecond timestamp for the chat bubble hover tooltip.
 *
 * - Uses the user's locale + timezone via `toLocaleString` with explicit
 *   fields (year/month/day/hour/minute/second) so the output is stable
 *   across runtimes. Relying on the default `toLocaleString()` format
 *   produces subtly different strings between Tauri WebKit, Chromium,
 *   and jsdom tests (e.g. Tauri inserts " " between date and time on
 *   some locales).
 * - Returns "" for falsy / invalid input so the value can be passed
 *   directly to `<Tooltip content={...} />`. The Tooltip component
 *   short-circuits on empty content and renders the trigger without a
 *   wrapper — this keeps the DOM clean when a message has no usable
 *   timestamp (legacy JSONL entries, etc.).
 */
export function formatBubbleTime(ms: number | undefined | null): string {
  if (!ms) return "";
  const d = new Date(ms);
  if (Number.isNaN(d.getTime())) return "";
  return d.toLocaleString([], {
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
}
