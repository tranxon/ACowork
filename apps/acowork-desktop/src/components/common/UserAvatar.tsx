import { useEffect, useMemo } from "react";
import { useUserProfileStore } from "../../stores/userProfileStore";
import { pickDeterministicBuiltinIconId, pickRandomBuiltinIconId } from "../../lib/avatar";
import { BUILTIN_ICONS } from "../../lib/builtinIcons";
import type { BoringAvatarVariant } from "../../lib/types";

// ── Re-exports for back-compat ──────────────────────────────────────────
// The icon catalogue (BUILTIN_ICONS, BUILTIN_ICON_IDS, AGENT_DEFAULT_PALETTE)
// is defined in `lib/builtinIcons.ts` as a leaf module so the user-profile
// store can read it without creating a circular import. We re-export the
// values from here so existing consumers (ProfileTab, AgentSetupTab,
// agentProfileStore) keep working unchanged.
export { BUILTIN_ICONS, BUILTIN_ICON_IDS, AGENT_DEFAULT_PALETTE } from "../../lib/builtinIcons";

// ── Built-in icon wrapper ────────────────────────────────────────────────

function BuiltinIconAvatar({ iconId, size, className }: { iconId: string; size: number; className?: string }) {
  const src = BUILTIN_ICONS[iconId] ?? BUILTIN_ICONS["icon-01"];
  return (
    <img
      src={src}
      alt={iconId}
      draggable={false}
      className={`rounded-full object-cover ring-1 ring-zinc-300/60 dark:ring-zinc-600/60 ${className ?? ""}`}
      style={{ width: size, height: size }}
    />
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
