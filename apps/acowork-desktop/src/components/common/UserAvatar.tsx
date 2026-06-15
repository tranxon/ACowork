import { useEffect, useMemo } from "react";
import { useUserProfileStore } from "../../stores/userProfileStore";
import { pickDeterministicBuiltinIconId, pickRandomBuiltinIconId } from "../../lib/avatar";
import type { BoringAvatarVariant } from "../../lib/types";
import { Tooltip } from "./Tooltip";

// ── Built-in icons (bundled JPG assets) ────────────────────────────────
// JPG files live at src/assets/builtin-icons/icon-XX.jpg.
// Vite resolves each path to a hashed, cache-friendly URL at build time.

const ICON_URL_MAP = import.meta.glob<string>(
  "../../assets/builtin-icons/icon-*.jpg",
  { eager: true, query: "?url", import: "default" },
);

const BUILTIN_ICONS: Record<string, string> = Object.fromEntries(
  Object.entries(ICON_URL_MAP)
    .map(([path, url]) => {
      const match = path.match(/icon-\d+/);
      return match ? [match[0], url as unknown as string] : null;
    })
    .filter((entry): entry is [string, string] => entry !== null)
    .sort(([a], [b]) => a.localeCompare(b)),
);

/** Extract built-in icon IDs for selection UI */
export const BUILTIN_ICON_IDS = Object.keys(BUILTIN_ICONS);

export const AGENT_DEFAULT_PALETTE = ["#6366F1", "#8B5CF6", "#EC4899", "#F59E0B", "#10B981", "#06B6D4", "#F97316", "#EF4444"];

// ── Built-in icon wrapper ────────────────────────────────────────────────

function BuiltinIconAvatar({ iconId, size, className }: { iconId: string; size: number; className?: string }) {
  const src = BUILTIN_ICONS[iconId] ?? BUILTIN_ICONS["icon-01"];
  return (
    <Tooltip content={iconId} variant="plain">
      <img
        src={src}
        alt={iconId}
        draggable={false}
        className={`rounded-full object-cover ring-1 ring-zinc-300/60 dark:ring-zinc-600/60 ${className ?? ""}`}
        style={{ width: size, height: size }}
      />
    </Tooltip>
  );
}

// ── Public component ────────────────────────────────────────────────────

export interface UserAvatarProps {
  displayName?: string;
  /** Override profile settings. If omitted, reads from userProfileStore. */
  avatarType?: "boring" | "icon" | "letter";
  avatarVariant?: BoringAvatarVariant;
  avatarIcon?: string;
  avatarColors?: string[];
  size?: number;
  className?: string;
}

/**
 * User avatar. Always renders a builtin icon — letter/gradient generation
 * has been removed in favour of the bundled icon set. If the profile has
 * no `avatarIcon` set (legacy state, or before onboarding completed), a
 * deterministic random builtin icon is shown and persisted in the background.
 */
export function UserAvatar({
  displayName,
  avatarIcon: _icon,
  size = 32,
  className,
}: UserAvatarProps) {
  const profileIconId = useUserProfileStore((s) => s.profile.avatarIcon);
  const setProfile = useUserProfileStore((s) => s.setProfile);

  const fallbackIconId = useMemo(
    () => pickDeterministicBuiltinIconId(displayName ?? "user"),
    [displayName],
  );

  // Self-heal: if no profile icon is set (legacy data, pre-onboarding),
  // persist a random one in the background so the next render reads it
  // from the store. Idempotent.
  useEffect(() => {
    if (profileIconId) return;
    const iconId = pickRandomBuiltinIconId();
    if (iconId) setProfile({ avatarIcon: iconId });
  }, [profileIconId, setProfile]);

  const iconId =
    (_icon && BUILTIN_ICONS[_icon] ? _icon : null) ??
    (profileIconId && BUILTIN_ICONS[profileIconId] ? profileIconId : null) ??
    fallbackIconId ??
    "icon-01";

  return <BuiltinIconAvatar iconId={iconId} size={size} className={className} />;
}

export { BUILTIN_ICONS };
