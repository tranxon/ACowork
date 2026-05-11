import { useMemo } from "react";
import {
  AGENT_DEFAULT_PALETTE,
  BUILTIN_ICONS,
} from "./UserAvatar";

// ── Helpers ─────────────────────────────────────────────────────────────

function hashString(str: string): number {
  let hash = 0;
  for (let i = 0; i < str.length; i++) {
    hash = str.charCodeAt(i) + ((hash << 5) - hash);
    hash |= 0;
  }
  return Math.abs(hash);
}

function GeometricAvatar({ name, size, palette }: { name: string; size: number; palette: string[] }) {
  const seed = useMemo(() => hashString(name), [name]);
  const variant = seed % 6; // deterministic variant from name
  const c1 = palette[seed % palette.length] ?? "#6366F1";
  const c2 = palette[(seed + 1) % palette.length] ?? "#10B981";
  const c3 = palette[(seed + 2) % palette.length] ?? "#EC4899";

  if (variant === 0) {
    // beam style
    return (
      <svg width={size} height={size} viewBox="0 0 36 36" className="rounded-full ring-1 ring-zinc-300/60 dark:ring-zinc-600/60">
        <rect width="36" height="36" rx="18" fill={c1} />
        <rect x="10" y="10" width="16" height="16" rx="8" fill={c2} opacity="0.7" />
        <circle cx="18" cy="18" r="5" fill={c1} />
      </svg>
    );
  }
  if (variant === 1) {
    // ring style
    return (
      <svg width={size} height={size} viewBox="0 0 36 36" className="rounded-full ring-1 ring-zinc-300/60 dark:ring-zinc-600/60">
        <rect width="36" height="36" rx="18" fill={c1} />
        <circle cx="18" cy="18" r="10" fill="none" stroke={c2} strokeWidth="4" />
        <circle cx="18" cy="18" r="4" fill={c2} />
      </svg>
    );
  }
  if (variant === 2) {
    // pixel style
    const cells: [number, number][] = [];
    for (let y = 0; y < 6; y++) {
      for (let x = 0; x < 6; x++) {
        if ((seed + x * 7 + y * 13) % 3 !== 0) continue;
        cells.push([4 + x * 5, 4 + y * 5]);
      }
    }
    return (
      <svg width={size} height={size} viewBox="0 0 36 36" className="rounded-full ring-1 ring-zinc-300/60 dark:ring-zinc-600/60">
        <rect width="36" height="36" rx="18" fill={c1} />
        {cells.map(([x, y], i) => (
          <rect key={i} x={x} y={y} width="4" height="4" rx="1" fill={c2} />
        ))}
      </svg>
    );
  }
  if (variant === 3) {
    // sunset style
    return (
      <svg width={size} height={size} viewBox="0 0 36 36" className="rounded-full ring-1 ring-zinc-300/60 dark:ring-zinc-600/60">
        <rect width="36" height="36" rx="18" fill={c1} />
        <circle cx={18} cy={14} r="8" fill={c2} opacity="0.7" />
        <rect x="6" y="20" width="24" height="8" rx="2" fill={c3} opacity="0.6" />
      </svg>
    );
  }
  if (variant === 4) {
    // bauhaus style
    return (
      <svg width={size} height={size} viewBox="0 0 36 36" className="rounded-full ring-1 ring-zinc-300/60 dark:ring-zinc-600/60">
        <rect width="36" height="36" rx="18" fill={c1} />
        <circle cx="14" cy="14" r="7" fill={c2} opacity="0.7" />
        <rect x="16" y="16" width="12" height="12" rx="4" fill={c3} opacity="0.6" />
      </svg>
    );
  }
  // marble style
  return (
    <svg width={size} height={size} viewBox="0 0 36 36" className="rounded-full ring-1 ring-zinc-300/60 dark:ring-zinc-600/60">
      <rect width="36" height="36" rx="18" fill={c1} />
      <circle cx={16 + (seed % 5)} cy={16 + ((seed * 3) % 5)} r="8" fill={c2} opacity="0.6" />
      <circle cx={20 - (seed % 4)} cy={20 - ((seed * 2) % 4)} r="5" fill={c3} opacity="0.8" />
    </svg>
  );
}

// ── Public component ────────────────────────────────────────────────────

export interface AgentAvatarProps {
  /** Agent identifier — used as seed for deterministic avatar generation */
  agentId: string;
  /** Display name (fallback for letter avatar) */
  displayName?: string;
  /** Custom avatar image URL (from Gateway /agents/:id/avatar endpoint) */
  avatarUrl?: string;
  /** Built-in icon ID from profile settings (e.g. "icon-02") */
  iconId?: string | null;
  /** Size in pixels */
  size?: number;
  /** Additional CSS classes */
  className?: string;
}

export function AgentAvatar({
  agentId,
  displayName,
  avatarUrl,
  iconId,
  size = 32,
  className,
}: AgentAvatarProps) {
  const name = displayName || agentId;
  const palette = AGENT_DEFAULT_PALETTE;

  // Built-in icon takes priority (from agent profile settings)
  if (iconId && BUILTIN_ICONS[iconId]) {
    const src = BUILTIN_ICONS[iconId];
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

  // Custom avatar image takes priority
  if (avatarUrl) {
    return (
      <img
        src={avatarUrl}
        alt={name}
        className={`rounded-full ring-1 ring-zinc-300/60 dark:ring-zinc-600/60 object-cover ${className ?? ""}`}
        style={{ width: size, height: size }}
      />
    );
  }

  // Deterministic geometric avatar from agent name
  return <GeometricAvatar name={name} size={size} palette={palette} />;
}
