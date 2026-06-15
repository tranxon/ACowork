import { useEffect, useMemo, useState } from "react";
import { BUILTIN_ICONS } from "./UserAvatar";
import {
  pickDeterministicBuiltinIconId,
  resolveAgentAvatarUrl,
} from "../../lib/avatar";
import { useAgentProfileStore } from "../../stores/agentProfileStore";

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

export interface AgentAvatarProps {
  /** Agent identifier — used as seed for deterministic avatar generation */
  agentId: string;
  /** Display name (fallback for letter avatar) */
  displayName?: string;
  /**
   * Raw avatar path from manifest.avatar (e.g. "assets/avatar.png").
   * When set AND the agent has a profile iconId, profile icon wins.
   * When set AND no profile icon is set, the gateway avatar endpoint is tried.
   * Pass `null`/omit to skip the packaged avatar entirely.
   */
  avatarUrl?: string | null;
  /** Built-in icon ID from profile settings (e.g. "icon-02") */
  iconId?: string | null;
  /** Size in pixels */
  size?: number;
  /** Additional CSS classes */
  className?: string;
}

export function AgentAvatar({
  agentId,
  avatarUrl,
  iconId,
  size = 32,
  className,
}: AgentAvatarProps) {
  // 1. Profile icon takes priority (explicit user choice)
  if (iconId && BUILTIN_ICONS[iconId]) {
    return <BuiltinIconAvatar iconId={iconId} size={size} className={className} />;
  }

  // 2. Packaged avatar (manifest.avatar) — try to load from gateway endpoint.
  //    On 404 / network error, fall through to the deterministic random icon.
  if (avatarUrl) {
    return <PackagedAgentAvatar agentId={agentId} fallbackSeed={agentId} size={size} className={className} />;
  }

  // 3. No profile icon, no packaged avatar — deterministic random builtin icon.
  //    This is the same icon the install hook will persist via
  //    `assignRandomAvatarIfMissing`, so first paint matches the saved state.
  return <DeterministicBuiltinAvatar seed={agentId} size={size} className={className} />;
}

// ── Internal: packaged avatar (manifest.avatar) ─────────────────────────

function PackagedAgentAvatar({
  agentId,
  fallbackSeed,
  size,
  className,
}: {
  agentId: string;
  fallbackSeed: string;
  size: number;
  className?: string;
}) {
  const url = useMemo(() => resolveAgentAvatarUrl(agentId), [agentId]);
  const [errored, setErrored] = useState(false);

  // If the URL builder or image load failed, fall back to a deterministic
  // random builtin icon (persisted by the install hook in the background).
  if (!url || errored) {
    return <DeterministicBuiltinAvatar seed={fallbackSeed} size={size} className={className} />;
  }

  return (
    <img
      src={url}
      alt={agentId}
      draggable={false}
      onError={() => setErrored(true)}
      className={`rounded-full object-cover ring-1 ring-zinc-300/60 dark:ring-zinc-600/60 ${className ?? ""}`}
      style={{ width: size, height: size }}
    />
  );
}

// ── Internal: deterministic builtin icon with self-healing persistence ──

function DeterministicBuiltinAvatar({
  seed,
  size,
  className,
}: {
  seed: string;
  size: number;
  className?: string;
}) {
  const fallbackIconId = useMemo(() => pickDeterministicBuiltinIconId(seed), [seed]);
  const profileIconId = useAgentProfileStore((s) => s.profiles[seed]?.avatarIconId);

  // If the profile store has an icon for this agent, use it. This avoids a
  // mismatch when the persisted icon differs from the deterministic pick
  // (e.g. user manually changed it via AgentSetupTab).
  const iconId = profileIconId && BUILTIN_ICONS[profileIconId] ? profileIconId : fallbackIconId;

  // Self-heal: if no profile entry exists, persist the deterministic icon
  // in the background. This is idempotent and runs once per (seed, render-mount).
  useEffect(() => {
    if (!seed) return;
    const state = useAgentProfileStore.getState();
    const existing = state.profiles[seed];
    if (existing && existing.avatarIconId) return;
    if (!fallbackIconId) return;
    state.setProfile(seed, { avatarIconId: fallbackIconId });
  }, [seed, fallbackIconId]);

  if (!iconId) {
    return <BuiltinIconAvatar iconId="icon-01" size={size} className={className} />;
  }
  return <BuiltinIconAvatar iconId={iconId} size={size} className={className} />;
}
