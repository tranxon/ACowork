import { useMemo } from "react";
import { useUserProfileStore } from "../../stores/userProfileStore";
import type { BoringAvatarVariant } from "../../lib/types";

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

// ── Hash-based color from string ────────────────────────────────────────

function hashString(str: string): number {
  let hash = 0;
  for (let i = 0; i < str.length; i++) {
    hash = str.charCodeAt(i) + ((hash << 5) - hash);
    hash |= 0;
  }
  return Math.abs(hash);
}

function pickColor(name: string, palette: string[]): string {
  if (palette.length === 0) return "#6366F1";
  return palette[hashString(name) % palette.length];
}

export const AGENT_DEFAULT_PALETTE = ["#6366F1", "#8B5CF6", "#EC4899", "#F59E0B", "#10B981", "#06B6D4", "#F97316", "#EF4444"];

// ── LetterAvatar fallback ───────────────────────────────────────────────

function LetterAvatar({ name, size, palette }: { name: string; size: number; palette: string[] }) {
  const initial = (name || "?")[0].toUpperCase();
  const bgColor = pickColor(name, palette);
  const textColor = "#ffffff";
  return (
    <div
      className="rounded-full ring-1 ring-zinc-300/60 dark:ring-zinc-600/60 flex items-center justify-center font-bold select-none"
      style={{
        width: size,
        height: size,
        backgroundColor: bgColor,
        color: textColor,
        fontSize: size * 0.42,
        lineHeight: 1,
      }}
      title={name}
    >
      {initial}
    </div>
  );
}

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

// ── Boring Avatars lazy component ───────────────────────────────────────

// Use function wrapper to avoid issues with named exports
function BoringAvatarBlock({ name, variant, colors, size }: { name: string; variant: BoringAvatarVariant; colors: string[]; size: number }) {
  // NOTE: boring-avatars v1.x exports default Avatar component
  // We inline a simple SVG-based geometric avatar as fallback for type safety.
  // When types are resolved, use: <Avatar name={name} variant={variant} colors={colors} size={size} square={false} />
  const seed = useMemo(() => hashString(name), [name]);
  const c1 = colors[seed % colors.length] ?? "#6366F1";
  const c2 = colors[(seed + 1) % colors.length] ?? "#10B981";

  // Simple geometric avatar (will be replaced by actual boring-avatars once typing is set up)
  if (variant === "beam") {
    return (
      <svg width={size} height={size} viewBox="0 0 36 36" className="rounded-full ring-1 ring-zinc-300/60 dark:ring-zinc-600/60">
        <rect width="36" height="36" rx="18" fill={c1} />
        <rect x="10" y="10" width="16" height="16" rx="8" fill={c2} opacity="0.7" />
        <circle cx="18" cy="18" r="5" fill={c1} />
      </svg>
    );
  }
  if (variant === "ring") {
    return (
      <svg width={size} height={size} viewBox="0 0 36 36" className="rounded-full ring-1 ring-zinc-300/60 dark:ring-zinc-600/60">
        <rect width="36" height="36" rx="18" fill={c1} />
        <circle cx="18" cy="18" r="10" fill="none" stroke={c2} strokeWidth="4" />
        <circle cx="18" cy="18" r="4" fill={c2} />
      </svg>
    );
  }
  if (variant === "pixel") {
    const rows = [];
    for (let y = 0; y < 6; y++) {
      for (let x = 0; x < 6; x++) {
        const v = (seed + x * 7 + y * 13) % 2;
        if (v === 0) continue;
        rows.push(<rect key={`${x}-${y}`} x={4 + x * 5} y={4 + y * 5} width="4" height="4" rx="1" fill={c2} />);
      }
    }
    return (
      <svg width={size} height={size} viewBox="0 0 36 36" className="rounded-full ring-1 ring-zinc-300/60 dark:ring-zinc-600/60">
        <rect width="36" height="36" rx="18" fill={c1} />
        {rows}
      </svg>
    );
  }

  // marble / sunset / bauhaus fallback
  return (
    <svg width={size} height={size} viewBox="0 0 36 36" className="rounded-full ring-1 ring-zinc-300/60 dark:ring-zinc-600/60">
      <rect width="36" height="36" rx="18" fill={c1} />
      <circle cx={16 + (seed % 5)} cy={16 + ((seed * 3) % 5)} r="8" fill={c2} opacity="0.6" />
      <circle cx={20 - (seed % 4)} cy={20 - ((seed * 2) % 4)} r="5" fill={c1} opacity="0.8" />
    </svg>
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

export function UserAvatar({
  displayName,
  avatarType: _type,
  avatarVariant: _variant,
  avatarIcon: _icon,
  avatarColors: _colors,
  size = 32,
  className,
}: UserAvatarProps) {
  const profile = useUserProfileStore((s) => s.profile);
  const storeColors = useUserProfileStore((s) => s.getColors)();

  const name = displayName ?? profile.displayName;
  const type = _type ?? profile.avatarType;
  const variant = _variant ?? profile.avatarVariant;
  const iconId = _icon ?? profile.avatarIcon;
  const colors = _colors && _colors.length > 0 ? _colors : storeColors;

  if (type === "icon" && iconId && BUILTIN_ICONS[iconId]) {
    return <BuiltinIconAvatar iconId={iconId} size={size} className={className} />;
  }

  if (type === "boring") {
    return <BoringAvatarBlock name={name || "user"} variant={variant} colors={colors} size={size} />;
  }

  // type === "letter" (fallback)
  return <LetterAvatar name={name || "?"} size={size} palette={colors} />;
}

export { BUILTIN_ICONS };
