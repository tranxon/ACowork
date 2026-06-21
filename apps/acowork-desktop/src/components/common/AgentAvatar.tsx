import { useEffect, useMemo, useState } from "react";
import { BUILTIN_ICONS } from "./UserAvatar";
import {
  pickDeterministicBuiltinIconId,
  resolveAgentAvatarObjectUrl,
  resolveAgentAvatarUrl,
} from "../../lib/avatar";
import { useAgentStore } from "../../stores/agentStore";

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
   * When set, the packaged avatar wins over any auto-assigned or user-chosen
   * builtin icon — see `docs/design/zh/02-agent-package.md` for the priority
   * contract. The Gateway URL is built lazily via `resolveAgentAvatarUrl`
   * and the bytes are fetched once via the in-flight dedup helper.
   */
  avatarUrl?: string | null;
  /**
   * Manifest version (semver). Appended as `?v=<version>` to bust the HTTP
   * cache when the package is re-installed with a new version.
   */
  version?: string | null;
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
  version,
  iconId,
  size = 32,
  className,
}: AgentAvatarProps) {
  // Avatar resolution priority (per `docs/design/zh/02-agent-package.md`):
  //
  // 1. Packaged avatar from `manifest.avatar` (highest priority — the
  //    package author ships an asset, the client should render it). Always
  //    wins over any auto-assigned or user-chosen builtin icon.
  // 2. Profile-store iconId — either an auto-assigned fallback (kept when
  //    the manifest has no `avatar`) or a user's explicit pick from the
  //    icon picker in AgentSetupTab.
  // 3. Deterministic random builtin icon — the first render before the
  //    install hook has persisted a profile entry. Self-heals via
  //    `useEffect` so subsequent renders match the saved state.
  if (avatarUrl) {
    return (
      <PackagedAgentAvatar
        agentId={agentId}
        version={version}
        fallbackSeed={agentId}
        size={size}
        className={className}
      />
    );
  }

  if (iconId && BUILTIN_ICONS[iconId]) {
    return <BuiltinIconAvatar iconId={iconId} size={size} className={className} />;
  }

  return <DeterministicBuiltinAvatar seed={agentId} size={size} className={className} />;
}

// ── Internal: packaged avatar (manifest.avatar) ─────────────────────────

function PackagedAgentAvatar({
  agentId,
  version,
  fallbackSeed,
  size,
  className,
}: {
  agentId: string;
  version?: string | null;
  fallbackSeed: string;
  size: number;
  className?: string;
}) {
  // We always start with the direct HTTP URL so the first paint is
  // synchronous — no empty avatar flash. In parallel we kick off the
  // blob-URL fetch (deduped + cached) and swap to it once it resolves;
  // the browser will not re-decode the image because the URL change keeps
  // the rendered frame stable (and modern browsers cross-fade naturally).
  const directUrl = useMemo(
    () => resolveAgentAvatarUrl(agentId, version),
    [agentId, version],
  );
  const [resolvedUrl, setResolvedUrl] = useState<string | null>(directUrl);
  const [errored, setErrored] = useState(false);

  useEffect(() => {
    let cancelled = false;
    setResolvedUrl(directUrl);
    setErrored(false);
    if (!directUrl) return;
    resolveAgentAvatarObjectUrl(agentId, version)
      .then((url) => {
        if (cancelled) return;
        if (url) setResolvedUrl(url);
      })
      .catch(() => {
        if (cancelled) return;
        // keep directUrl
      });
    return () => {
      cancelled = true;
    };
  }, [agentId, version, directUrl]);

  // If the URL builder failed or the image load errored, fall back to a
  // deterministic random builtin icon.
  if (!resolvedUrl || errored) {
    return <DeterministicBuiltinAvatar seed={fallbackSeed} size={size} className={className} />;
  }

  return (
    <img
      src={resolvedUrl}
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
  const profileIconId = useAgentStore((s) => s.agents[seed]?.profile?.avatarIconId ?? null);

  // If the profile store has an icon for this agent, use it. This avoids a
  // mismatch when the persisted icon differs from the deterministic pick
  // (e.g. user manually changed it via AgentSetupTab).
  const iconId = profileIconId && BUILTIN_ICONS[profileIconId] ? profileIconId : fallbackIconId;

  // Self-heal: if no profile entry exists, persist the deterministic icon
  // in the background. This is idempotent and runs once per (seed, render-mount).
  useEffect(() => {
    if (!seed) return;
    const state = useAgentStore.getState();
    const existing = state.agents[seed]?.profile;
    if (existing?.avatarIconId) return;
    if (!fallbackIconId) return;
    state.setProfile(seed, { avatarIconId: fallbackIconId });
  }, [seed, fallbackIconId]);

  if (!iconId) {
    return <BuiltinIconAvatar iconId="icon-01" size={size} className={className} />;
  }
  return <BuiltinIconAvatar iconId={iconId} size={size} className={className} />;
}
