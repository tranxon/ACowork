import { useEffect, useMemo } from "react";
import { useUserProfileStore } from "../../stores/userProfileStore";
import { pickDeterministicBuiltinIconId, pickRandomBuiltinIconId, resolveUserAvatarFileUrl } from "../../lib/avatar";
import { BUILTIN_ICONS } from "../../lib/builtinIcons";
import type { BoringAvatarVariant } from "../../lib/types";

// ── Re-exports for back-compat ──────────────────────────────────────────
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

// ── Custom file avatar wrapper ───────────────────────────────────────────

function CustomFileAvatar({ path, size, className }: { path: string; size: number; className?: string }) {
  const url = resolveUserAvatarFileUrl(path);
  return (
    <img
      src={url}
      alt="User avatar"
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
  /** Custom avatar file path (from backend user profile, e.g. "assets/avatar-01.png"). */
  avatarUrl?: string | null;
  /** Builtin avatar icon ID (from backend user profile, e.g. "icon-05"). */
  builtinAvatarId?: string | null;
}

/**
 * User avatar with custom file support (ADR-017).
 *
 * Resolution priority:
 * 1. Custom avatar file (via Gateway /api/user/avatar-file endpoint)
 * 2. Builtin icon ID (from backend or local store)
 * 3. Deterministic random builtin icon — the final fallback
 */
export function UserAvatar({
  displayName,
  avatarIcon: _icon,
  size = 32,
  className,
  avatarUrl,
  builtinAvatarId,
}: UserAvatarProps) {
  const profileIconId = useUserProfileStore((s) => s.profile.avatarIcon);
  const setProfile = useUserProfileStore((s) => s.setProfile);

  const fallbackIconId = useMemo(
    () => pickDeterministicBuiltinIconId(displayName ?? "user"),
    [displayName],
  );

  // Self-heal: if no profile icon is set (legacy data, pre-onboarding),
  // persist a random one in the background so the next render reads it from
  // the store. Idempotent.
  useEffect(() => {
    if (profileIconId) return;
    const iconId = pickRandomBuiltinIconId();
    if (iconId) setProfile({ avatarIcon: iconId });
  }, [profileIconId, setProfile]);

  // ADR-017: Resolution priority
  if (avatarUrl) {
    return <CustomFileAvatar path={avatarUrl} size={size} className={className} />;
  }

  if (builtinAvatarId && BUILTIN_ICONS[builtinAvatarId]) {
    return <BuiltinIconAvatar iconId={builtinAvatarId} size={size} className={className} />;
  }

  const iconId =
    (_icon && BUILTIN_ICONS[_icon] ? _icon : null) ??
    (profileIconId && BUILTIN_ICONS[profileIconId] ? profileIconId : null) ??
    fallbackIconId ??
    "icon-01";

  return <BuiltinIconAvatar iconId={iconId} size={size} className={className} />;
}
