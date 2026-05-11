import { useMemo } from "react";
import { useUserProfileStore } from "../../stores/userProfileStore";
import type { BoringAvatarVariant } from "../../lib/types";

// ── SVG icons (inline data URIs for built-in avatars) ──────────────────
// Free icons from iconpacks.net — 20 simple user avatar SVGs

const BUILTIN_ICONS: Record<string, string> = {
  "icon-01": '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><rect x="4" y="2" width="16" height="2.5" rx="1" fill="#2C2520"/><rect x="6" y="4.5" width="12" height="2" fill="#2C2520"/><line x1="16" y1="4.5" x2="18" y2="9" stroke="#2C2520" stroke-width="1" stroke-linecap="round"/><circle cx="18" cy="9" r="1" fill="#4CAF50"/><circle cx="12" cy="14" r="7.5" fill="#FDDCB5"/><circle cx="9" cy="13.5" r="0.9" fill="#2C2520"/><circle cx="15" cy="13.5" r="0.9" fill="#2C2520"/><path d="M10 17Q12 19 14 17" fill="none" stroke="#2C2520" stroke-width="0.8" stroke-linecap="round"/></svg>',
  "icon-02": '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M4 8Q6 1.5 12 2Q18 1.5 20 8Q20 9.5 18 10Q12 11.5 6 10Q4 9.5 4 8z" fill="#6B4226"/><circle cx="12" cy="14" r="7.5" fill="#FDDCB5"/><circle cx="9.5" cy="13.5" r="0.9" fill="#2C2520"/><circle cx="14.5" cy="13.5" r="0.9" fill="#2C2520"/><path d="M10 17Q12 19 14 17" fill="none" stroke="#2C2520" stroke-width="0.8" stroke-linecap="round"/></svg>',
  "icon-03": '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M3 6Q5 1.5 12 2Q19 1.5 21 6v2Q18 9 12 9Q6 9 3 7z" fill="#2C2520"/><path d="M5 8Q5 16 12 17Q19 16 19 8v5Q19 14 12 15Q5 14 5 13z" fill="#2C2520"/><circle cx="12" cy="13.5" r="6.5" fill="#FDDCB5"/><circle cx="9" cy="13" r="0.9" fill="#2C2520"/><circle cx="15" cy="13" r="0.9" fill="#2C2520"/><ellipse cx="12" cy="16" rx="2.5" ry="1.2" fill="#E88080"/></svg>',
  "icon-04": '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M3.5 8Q5 1.5 8 3.5Q10 2 12 2.5Q14 2 16.5 3Q18.5 2 20.5 8Q20.5 10 18 10.5Q12 12 6 10.5Q3.5 10 3.5 8z" fill="#2C2520"/><circle cx="12" cy="14" r="7.5" fill="#FDDCB5"/><rect x="6" y="12" width="6" height="3" rx="1.5" fill="none" stroke="#4CAF50" stroke-width="1.5"/><rect x="12" y="12" width="6" height="3" rx="1.5" fill="none" stroke="#4CAF50" stroke-width="1.5"/><line x1="7" y1="13.5" x2="12" y2="13.5" stroke="#4CAF50" stroke-width="0.7"/><line x1="12" y1="13.5" x2="17" y2="13.5" stroke="#4CAF50" stroke-width="0.7"/><circle cx="8.5" cy="13" r="0.7" fill="#2C2520"/><circle cx="15.5" cy="13" r="0.7" fill="#2C2520"/><line x1="10.5" y1="17" x2="13.5" y2="17" stroke="#2C2520" stroke-width="0.8" stroke-linecap="round"/></svg>',
  "icon-05": '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M4 6l1.5-3L7 6l2-3.5L11 6l2-3L15 6l2-3.5L19 6l1.5-3L21 6Q21 9 19 10Q12 12 5 10Q4 9 4 6z" fill="#D2691E"/><circle cx="12" cy="14" r="7.5" fill="#FDDCB5"/><circle cx="9" cy="13.5" r="0.9" fill="#2C2520"/><circle cx="15" cy="13.5" r="0.9" fill="#2C2520"/><path d="M10 17Q12 19 14 17" fill="none" stroke="#2C2520" stroke-width="0.8" stroke-linecap="round"/></svg>',
  "icon-06": '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M4 7Q7 2 12 2Q17 2 20 7Q20 9 18 9.5Q12 11 6 9.5Q4 9 4 7z" fill="#F0C060"/><circle cx="5.5" cy="12.5" r="3" fill="#F0C060"/><circle cx="18.5" cy="12.5" r="3" fill="#F0C060"/><rect x="4" y="0.5" width="2" height="4" rx="1" fill="#FF69B4"/><rect x="18" y="0.5" width="2" height="4" rx="1" fill="#FF69B4"/><circle cx="12" cy="14" r="7.5" fill="#FDDCB5"/><circle cx="9" cy="13.5" r="0.9" fill="#2C2520"/><circle cx="15" cy="13.5" r="0.9" fill="#2C2520"/><path d="M10 17Q12 19 14 17" fill="none" stroke="#2C2520" stroke-width="0.8" stroke-linecap="round"/></svg>',
  "icon-07": '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M5 6.5Q8 7 8 11v1H6v-2q-1 0-1 1.5z" fill="#C0C0C0"/><path d="M19 6.5Q16 7 16 11v1h2v-2q1 0 1 1.5z" fill="#C0C0C0"/><circle cx="12" cy="14" r="7.5" fill="#FDDCB5"/><circle cx="8.5" cy="13.5" r="1.5" fill="none" stroke="#6B4226" stroke-width="0.8"/><circle cx="15.5" cy="13.5" r="1.5" fill="none" stroke="#6B4226" stroke-width="0.8"/><line x1="7" y1="13.5" x2="10" y2="13.5" stroke="#6B4226" stroke-width="0.6"/><line x1="14" y1="13.5" x2="17" y2="13.5" stroke="#6B4226" stroke-width="0.6"/><circle cx="8.5" cy="13" r="0.6" fill="#2C2520"/><circle cx="15.5" cy="13" r="0.6" fill="#2C2520"/><path d="M10 17Q12 19 14 17" fill="none" stroke="#2C2520" stroke-width="0.8" stroke-linecap="round"/></svg>',
  "icon-08": '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><circle cx="6" cy="6.5" r="1.8" fill="#C0C0C0"/><circle cx="9" cy="5" r="2" fill="#C0C0C0"/><circle cx="12" cy="4.5" r="2.2" fill="#C0C0C0"/><circle cx="15" cy="5" r="2" fill="#C0C0C0"/><circle cx="18" cy="6.5" r="1.8" fill="#C0C0C0"/><circle cx="6" cy="8.5" r="1.5" fill="#C0C0C0"/><circle cx="9" cy="7.5" r="1.8" fill="#C0C0C0"/><circle cx="12" cy="7" r="2" fill="#C0C0C0"/><circle cx="15" cy="7.5" r="1.8" fill="#C0C0C0"/><circle cx="18" cy="8.5" r="1.5" fill="#C0C0C0"/><circle cx="12" cy="14" r="7.5" fill="#FDDCB5"/><circle cx="9" cy="13.5" r="0.9" fill="#2C2520"/><circle cx="15" cy="13.5" r="0.9" fill="#2C2520"/><path d="M10 17Q12 19 14 17" fill="none" stroke="#2C2520" stroke-width="0.8" stroke-linecap="round"/></svg>',
  "icon-09": '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><ellipse cx="12" cy="5" rx="8" ry="4.5" fill="#C0503C"/><rect x="3" y="5" width="18" height="1.5" rx="0.5" fill="#C0503C"/><circle cx="12" cy="5" r="1.2" fill="#C0503C"/><circle cx="12" cy="14" r="7.5" fill="#E0AC69"/><circle cx="9" cy="13.5" r="0.9" fill="#2C2520"/><circle cx="15" cy="13.5" r="0.9" fill="#2C2520"/><path d="M10 17Q12 19 14 17" fill="none" stroke="#2C2520" stroke-width="0.8" stroke-linecap="round"/></svg>',
  "icon-10": '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M7 6c0-2.5 0-5 5-5s5 2.5 5 5v2" fill="#fff"/><circle cx="9" cy="2.5" r="1" fill="#4CAF50"/><circle cx="12" cy="1.5" r="1.2" fill="#4CAF50"/><circle cx="15" cy="2.5" r="1" fill="#4CAF50"/><rect x="6" y="7" width="12" height="1" rx="0.3" fill="#fff"/><circle cx="12" cy="14" r="7.5" fill="#FDDCB5"/><circle cx="9" cy="13.5" r="0.9" fill="#2C2520"/><circle cx="15" cy="13.5" r="0.9" fill="#2C2520"/><path d="M10 17Q12 19 14 17" fill="none" stroke="#2C2520" stroke-width="0.8" stroke-linecap="round"/></svg>',
  "icon-11": '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M4 7Q7 1.5 12 2Q17 1.5 20 7Q20 9 18 9.5Q12 11 6 9.5Q4 9 4 7z" fill="#2C2520"/><circle cx="12" cy="9" r="2.5" fill="none" stroke="#E0E0E0" stroke-width="1.2"/><circle cx="12" cy="9" r="1" fill="#E0E0E0"/><rect x="10" y="6.5" width="4" height="2.5" rx="0.5" fill="#E0E0E0"/><path d="M15 14Q17.5 16 15 19" fill="none" stroke="#4CAF50" stroke-width="1" stroke-linecap="round"/><path d="M9 14Q6.5 16 9 19" fill="none" stroke="#4CAF50" stroke-width="1" stroke-linecap="round"/><circle cx="15" cy="19" r="1.2" fill="#4CAF50"/><circle cx="9" cy="19" r="1.2" fill="#4CAF50"/><circle cx="12" cy="14" r="7.5" fill="#FDDCB5"/><circle cx="9" cy="13.5" r="0.9" fill="#2C2520"/><circle cx="15" cy="13.5" r="0.9" fill="#2C2520"/><path d="M10 17Q12 19 14 17" fill="none" stroke="#2C2520" stroke-width="0.8" stroke-linecap="round"/></svg>',
  "icon-12": '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M3 5Q5 1.5 12 3.5Q19 1.5 21 5v13Q18 10 12 10Q6 10 3 13z" fill="#4A3728"/><path d="M3 6Q4 3 12 4.5Q20 3 21 6v8Q17 12 12 12Q7 12 3 14.5z" fill="#5C4A3A"/><circle cx="12" cy="13.5" r="6.5" fill="#E0AC69"/><circle cx="9" cy="13" r="0.8" fill="#2C2520"/><circle cx="15" cy="13" r="0.8" fill="#2C2520"/><path d="M10 16Q12 17.5 14 16" fill="none" stroke="#2C2520" stroke-width="0.7" stroke-linecap="round"/></svg>',
  "icon-13": '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M4 7Q7 1.5 12 2Q17 1.5 20 7Q20 9 18 9.5Q12 11 6 9.5Q4 9 4 7z" fill="#6B4226"/><circle cx="12" cy="14" r="7.5" fill="#FDDCB5"/><path d="M5 18Q6 21.5 12 22Q18 21.5 19 18Q12 20 5 18z" fill="#6B4226"/><path d="M6 15.5Q8 19 12 19.5Q16 19 18 15.5Q12 17 6 15.5z" fill="#6B4226"/><circle cx="9" cy="13.5" r="0.9" fill="#2C2520"/><circle cx="15" cy="13.5" r="0.9" fill="#2C2520"/><path d="M10 17Q12 18.5 14 17" fill="none" stroke="#2C2520" stroke-width="0.8" stroke-linecap="round"/></svg>',
  "icon-14": '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M4 7Q7 1.5 12 2Q17 1.5 20 7Q20 9 18 9.5Q12 11 6 9.5Q4 9 4 7z" fill="#2C2520"/><rect x="5.5" y="7" width="13" height="3.5" rx="2" fill="none" stroke="#4CAF50" stroke-width="1.3"/><line x1="12" y1="7" x2="12" y2="10.5" stroke="#4CAF50" stroke-width="0.8"/><circle cx="9" cy="8.7" r="1.2" fill="#4CAF50" opacity="0.4"/><circle cx="15" cy="8.7" r="1.2" fill="#4CAF50" opacity="0.4"/><circle cx="12" cy="14" r="7.5" fill="#E0AC69"/><circle cx="9" cy="13.5" r="0.9" fill="#2C2520"/><circle cx="15" cy="13.5" r="0.9" fill="#2C2520"/><path d="M10 17Q12 19 14 17" fill="none" stroke="#2C2520" stroke-width="0.8" stroke-linecap="round"/></svg>',
  "icon-15": '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><ellipse cx="12" cy="3.5" rx="7" ry="3" fill="#2C2520"/><rect x="5" y="1" width="14" height="2.5" rx="1" fill="#2C2520"/><path d="M5 7Q7 3.5 11 4Q16 3 18 6Q19 8 18.5 9.5Q12 11 5.5 9.5Q5 8 5 7z" fill="#6B4226"/><circle cx="12" cy="14" r="7.5" fill="#FDDCB5"/><circle cx="9" cy="13.5" r="0.9" fill="#2C2520"/><circle cx="15" cy="13.5" r="0.9" fill="#2C2520"/><path d="M10 17.5Q12 19.5 14 17.5" fill="none" stroke="#2C2520" stroke-width="0.8" stroke-linecap="round"/></svg>',
  "icon-16": '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M4 7Q7 1.5 12 2Q17 1.5 20 7Q20 9 18 9.5Q12 11 6 9.5Q4 9 4 7z" fill="#6B4226"/><path d="M4 6L2 8l2 2h2l-1-3.5z" fill="#FF69B4"/><path d="M20 6l2 2-2 2h-2l1-3.5z" fill="#FF69B4"/><circle cx="3.5" cy="8.5" r="1.5" fill="#FF69B4"/><circle cx="20.5" cy="8.5" r="1.5" fill="#FF69B4"/><circle cx="12" cy="14" r="7.5" fill="#FDDCB5"/><circle cx="9" cy="13.5" r="0.9" fill="#2C2520"/><circle cx="15" cy="13.5" r="0.9" fill="#2C2520"/><path d="M10 17Q12 19 14 17" fill="none" stroke="#2C2520" stroke-width="0.8" stroke-linecap="round"/></svg>',
  "icon-17": '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><rect x="4" y="3" width="16" height="5" rx="1" fill="#2C2520"/><rect x="5" y="7" width="14" height="1.5" rx="0.5" fill="#2C2520"/><circle cx="12" cy="4.5" r="1.5" fill="#4CAF50"/><rect x="11" y="3.5" width="2" height="2" fill="#FFD700"/><circle cx="12" cy="14" r="7.5" fill="#E0AC69"/><rect x="6" y="12" width="5" height="2.5" rx="1.2" fill="#2C2520"/><rect x="13" y="12" width="5" height="2.5" rx="1.2" fill="#2C2520"/><line x1="11" y1="13.5" x2="13" y2="13.5" stroke="#2C2520" stroke-width="0.8"/><line x1="10" y1="17" x2="14" y2="17" stroke="#2C2520" stroke-width="0.8" stroke-linecap="round"/></svg>',
  "icon-18": '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><circle cx="6" cy="7" r="2" fill="#C0503C"/><circle cx="9" cy="5.5" r="2.2" fill="#C0503C"/><circle cx="12" cy="5" r="2.5" fill="#C0503C"/><circle cx="15" cy="5.5" r="2.2" fill="#C0503C"/><circle cx="18" cy="7" r="2" fill="#C0503C"/><circle cx="6" cy="9" r="1.5" fill="#C0503C"/><circle cx="9" cy="8.5" r="1.8" fill="#C0503C"/><circle cx="12" cy="8" r="2" fill="#C0503C"/><circle cx="15" cy="8.5" r="1.8" fill="#C0503C"/><circle cx="18" cy="9" r="1.5" fill="#C0503C"/><circle cx="12" cy="14" r="7.5" fill="#FDDCB5"/><circle cx="8.5" cy="13.5" r="1.8" fill="none" stroke="#4CAF50" stroke-width="1.3"/><circle cx="15.5" cy="13.5" r="1.8" fill="none" stroke="#4CAF50" stroke-width="1.3"/><line x1="6.7" y1="13.5" x2="10.3" y2="13.5" stroke="#4CAF50" stroke-width="0.8"/><line x1="13.7" y1="13.5" x2="17.3" y2="13.5" stroke="#4CAF50" stroke-width="0.8"/><circle cx="8.5" cy="13" r="0.6" fill="#2C2520"/><circle cx="15.5" cy="13" r="0.6" fill="#2C2520"/><path d="M10 17Q12 19 14 17" fill="none" stroke="#2C2520" stroke-width="0.8" stroke-linecap="round"/></svg>',
  "icon-19": '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M3 4.5Q8 1.5 12 2Q16 1.5 21 4.5" fill="none" stroke="#4CAF50" stroke-width="2.5" stroke-linecap="round"/><rect x="2" y="6.5" width="5.5" height="6" rx="2" fill="#4CAF50"/><rect x="16.5" y="6.5" width="5.5" height="6" rx="2" fill="#4CAF50"/><circle cx="3.5" cy="9.5" r="0.7" fill="#fff"/><circle cx="20.5" cy="9.5" r="0.7" fill="#fff"/><rect x="8" y="13.5" width="2" height="5" rx="0.5" fill="#4CAF50"/><path d="M8 16l-2 0" stroke="#4CAF50" stroke-width="1" stroke-linecap="round"/><circle cx="12" cy="14" r="7.5" fill="#8B5E3C"/><circle cx="9" cy="13.5" r="0.9" fill="#fff"/><circle cx="15" cy="13.5" r="0.9" fill="#fff"/><path d="M10 17Q12 19 14 17" fill="none" stroke="#fff" stroke-width="0.8" stroke-linecap="round"/></svg>',
  "icon-20": '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><circle cx="12" cy="14" r="7.5" fill="#E0AC69"/><circle cx="9" cy="13.5" r="0.9" fill="#2C2520"/><circle cx="15" cy="13.5" r="0.9" fill="#2C2520"/><path d="M10 17Q12 19 14 17" fill="none" stroke="#2C2520" stroke-width="0.8" stroke-linecap="round"/></svg>',
};

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
  const svgStr = BUILTIN_ICONS[iconId] ?? BUILTIN_ICONS["icon-01"];
  return (
    <div
      className={`rounded-full bg-[#4CAF50] flex items-center justify-center overflow-hidden ring-1 ring-zinc-300/60 dark:ring-zinc-600/60 ${className ?? ""}`}
      style={{ width: size, height: size }}
    >
      <div
        className="flex items-center justify-center"
        style={{ width: size * 0.80, height: size * 0.80 }}
        dangerouslySetInnerHTML={{ __html: svgStr }}
      />
    </div>
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
