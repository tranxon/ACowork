/**
 * Built-in icon catalogue.
 *
 * This module is intentionally a **leaf** — it must not import from
 * `UserAvatar.tsx`, `userProfileStore.ts`, `agentProfileStore.ts`, or any
 * other module that participates in the user-profile / store evaluation
 * chain. Doing so would re-introduce the circular dependency that caused
 * the `Cannot access 'BUILTIN_ICON_IDS' before initialization` TDZ error
 * at app startup.
 *
 * The icon assets live at `src/assets/builtin-icons/icon-XX.jpg`. Vite
 * resolves each path to a hashed, cache-friendly URL at build time.
 */

const ICON_URL_MAP = import.meta.glob<string>(
  "../assets/builtin-icons/icon-*.jpg",
  { eager: true, query: "?url", import: "default" },
);

export const BUILTIN_ICONS: Record<string, string> = Object.fromEntries(
  Object.entries(ICON_URL_MAP)
    .map(([path, url]) => {
      const match = path.match(/icon-\d+/);
      return match ? [match[0], url as unknown as string] : null;
    })
    .filter((entry): entry is [string, string] => entry !== null)
    .sort(([a], [b]) => a.localeCompare(b)),
);

/** Stable list of bundled icon IDs (sorted, e.g. ["icon-01", "icon-02", ...]). */
export const BUILTIN_ICON_IDS: readonly string[] = Object.keys(BUILTIN_ICONS);

/** Default palette used by the agent installer when no palette is set. */
export const AGENT_DEFAULT_PALETTE: readonly string[] = [
  "#6366F1", "#8B5CF6", "#EC4899", "#F59E0B",
  "#10B981", "#06B6D4", "#F97316", "#EF4444",
];
