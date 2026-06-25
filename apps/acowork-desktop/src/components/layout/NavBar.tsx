import { type ComponentType } from "react";
import type { NavView } from "../../lib/types";
import { UserAvatar } from "../common/UserAvatar";
import { Tooltip } from "../common/Tooltip";
import { NavButton } from "../common/NavButton";
import { OutlineSettingsIcon, FilledSettingsIcon } from "../common/SettingsIcon";
import { useUserProfileStore } from "../../stores/userProfileStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { useTranslation } from "../../i18n/useTranslation";

interface NavBarProps {
  currentView: NavView;
  onViewChange: (view: NavView) => void;
  /** Called when user clicks their avatar — navigate to profile settings */
  onAvatarClick: () => void;
}

const topNavItems: { view: NavView; icon: ComponentType<{ className?: string }>; i18nKey: string }[] = [
  { view: "chat", icon: OutlineChatIcon, i18nKey: "navBar.chat" },
  { view: "projects", icon: OutlineProjectsIcon, i18nKey: "navBar.projects" },
  { view: "docs", icon: OutlineDocsIcon, i18nKey: "navBar.docs" },
  { view: "harness", icon: OutlineHarnessIcon, i18nKey: "navBar.harness" },
];

const bottomNavItems: { view: NavView; icon: ComponentType<{ className?: string }>; i18nKey: string }[] = [
  { view: "settings", icon: OutlineSettingsIcon, i18nKey: "navBar.settings" },
];

/** Filled chat bubble SVG — oval/pill style */
function FilledChatIcon({ className }: { className?: string }) {
  return (
    <svg className={className} viewBox="0 0 24 24" fill="currentColor" stroke="currentColor" strokeWidth="2">
      <g transform="translate(1.2, 1.2) scale(0.9)">
        <path d="M12 3C6.5 3 2 7.1 2 12c0 2.5 1.1 4.8 2.9 6.5L3 22l5.3-2.3C9.6 20.5 11.2 21 13 21c5.5 0 10-4.1 10-9s-4.5-9-11-9z" />
      </g>
    </svg>
  );
}

/** Outline chat bubble SVG — same oval shape, stroke-only */
function OutlineChatIcon({ className }: { className?: string }) {
  return (
    <svg className={className} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.75" strokeLinecap="round" strokeLinejoin="round">
      <g transform="translate(1.2, 1.2) scale(0.9)">
        <path d="M12 3C6.5 3 2 7.1 2 12c0 2.5 1.1 4.8 2.9 6.5L3 22l5.3-2.3C9.6 20.5 11.2 21 13 21c5.5 0 10-4.1 10-9s-4.5-9-11-9z" />
      </g>
    </svg>
  );
}

/** Outline puzzle piece icon — stroke-only for non-selected harness state */
function OutlineHarnessIcon({ className }: { className?: string }) {
  return (
    <svg className={className} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.75" strokeLinecap="round" strokeLinejoin="round">
      <g transform="translate(1.2, 1.2) scale(0.9, 0.9)">
        <path d="M19.439 7.85c-.049.322.059.648.289.878l1.568 1.568c.47.47.706 1.087.706 1.704s-.235 1.233-.706 1.704l-1.611 1.611a.98.98 0 0 1-.837.276c-.47-.07-.802-.48-.968-.925a2.501 2.501 0 1 0-3.214 3.214c.446.166.855.497.925.968a.979.979 0 0 1-.276.837l-1.61 1.611a2.404 2.404 0 0 1-1.705.706 2.404 2.404 0 0 1-1.704-.706l-1.568-1.568a1.026 1.026 0 0 0-.877-.29c-.493.074-.84.504-1.02.968a2.5 2.5 0 1 1-3.237-3.237c.464-.18.894-.527.967-1.02a1.026 1.026 0 0 0-.289-.877l-1.568-1.568A2.404 2.404 0 0 1 1.998 12c0-.617.236-1.234.706-1.704L4.315 8.685a.98.98 0 0 1 .837-.276c.47.07.802.48.968.925a2.501 2.501 0 1 0 3.214-3.214c-.446-.166-.855-.497-.925-.968a.979.979 0 0 1 .276-.837l1.611-1.611a2.404 2.404 0 0 1 1.704-.706c.617 0 1.234.236 1.704.706l1.568 1.568c.23.23.556.338.877.29.493-.074.84-.504 1.02-.969a2.5 2.5 0 1 1 3.237 3.237c-.464.18-.894.527-.967 1.02Z" />
      </g>
    </svg>
  );
}

/** Filled puzzle piece icon — solid version for active harness state */
function FilledHarnessIcon({ className }: { className?: string }) {
  return (
    <svg className={className} viewBox="0 0 24 24" fill="currentColor" stroke="currentColor" strokeWidth="2">
      <g transform="translate(1.2, 1.2) scale(0.9, 0.9)">
        <path d="M19.439 7.85c-.049.322.059.648.289.878l1.568 1.568c.47.47.706 1.087.706 1.704s-.235 1.233-.706 1.704l-1.611 1.611a.98.98 0 0 1-.837.276c-.47-.07-.802-.48-.968-.925a2.501 2.501 0 1 0-3.214 3.214c.446.166.855.497.925.968a.979.979 0 0 1-.276.837l-1.61 1.611a2.404 2.404 0 0 1-1.705.706 2.404 2.404 0 0 1-1.704-.706l-1.568-1.568a1.026 1.026 0 0 0-.877-.29c-.493.074-.84.504-1.02.968a2.5 2.5 0 1 1-3.237-3.237c.464-.18.894-.527.967-1.02a1.026 1.026 0 0 0-.289-.877l-1.568-1.568A2.404 2.404 0 0 1 1.998 12c0-.617.236-1.234.706-1.704L4.315 8.685a.98.98 0 0 1 .837-.276c.47.07.802.48.968.925a2.501 2.501 0 1 0 3.214-3.214c-.446-.166-.855-.497-.925-.968a.979.979 0 0 1 .276-.837l1.611-1.611a2.404 2.404 0 0 1 1.704-.706c.617 0 1.234.236 1.704.706l1.568 1.568c.23.23.556.338.877.29.493-.074.84-.504 1.02-.969a2.5 2.5 0 1 1 3.237 3.237c-.464.18-.894.527-.967 1.02Z" />
      </g>
    </svg>
  );
}

/** Outline document icon */
function OutlineDocsIcon({ className }: { className?: string }) {
  return (
    <svg className={className} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.75" strokeLinecap="round" strokeLinejoin="round">
      {/* Document */}
      <path d="M14 3H6a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V9.5L14 3z" />
      <polyline points="14,3 14,9 20,9" />
      <line x1="8" y1="12" x2="16" y2="12" />
      <line x1="8" y1="15" x2="12" y2="15" />
    </svg>
  );
}

/** Filled document icon */
function FilledDocsIcon({ className }: { className?: string }) {
  return (
    <svg className={className} viewBox="0 0 24 24" fill="currentColor" stroke="currentColor" strokeWidth="1.75">
      {/* Document */}
      <path d="M14 3H6a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V9.5L14 3z" />
      <polyline points="14,3 14,9 20,9" fill="none" stroke="white" />
      <line x1="8" y1="12" x2="16" y2="12" stroke="white" />
      <line x1="8" y1="15" x2="12" y2="15" stroke="white" />
    </svg>
  );
}

/**
 * Outline Kanban-board icon — the classic project-management visual:
 * an outer frame containing three columns of varying heights (Todo / Doing / Done).
 * `isDark` is accepted for API compatibility with the call site but the outline
 * variant looks identical in both themes (stroke uses currentColor).
 */
function OutlineProjectsIcon({ className }: { className?: string; isDark?: boolean }) {
  return (
    <svg
      className={className}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.75"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      {/* Board frame */}
      <rect x="3" y="4" width="18" height="16" rx="2" />
      {/* Three column cards of different heights */}
      <line x1="8"  y1="8"  x2="8"  y2="14" />
      <line x1="12" y1="8"  x2="12" y2="17" />
      <line x1="16" y1="8"  x2="16" y2="12" />
    </svg>
  );
}

/**
 * Filled Kanban-board icon — solid filled board with white columns "carved" out,
 * matching the FilledDocsIcon style (solid currentColor shape + white inner strokes).
 */
function FilledProjectsIcon({ className }: { className?: string }) {
  return (
    <svg
      className={className}
      viewBox="0 0 24 24"
      fill="currentColor"
      stroke="currentColor"
      strokeWidth="1.75"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      {/* Solid board frame — same stroke width as outline version, so the outer
          visual bounds match exactly (the 0.875 stroke half-extends the fill). */}
      <rect x="3" y="4" width="18" height="16" rx="2" />
      {/* Three column cards carved out in white, varying heights (Todo / Doing / Done) */}
      <line x1="8"  y1="8"  x2="8"  y2="14" stroke="white" />
      <line x1="12" y1="8"  x2="12" y2="17" stroke="white" />
      <line x1="16" y1="8"  x2="16" y2="12" stroke="white" />
    </svg>
  );
}

export function NavBar({ currentView, onViewChange, onAvatarClick }: NavBarProps) {
  const { t } = useTranslation();
  const profile = useUserProfileStore((s) => s.profile);
  const { theme } = useSettingsStore();
  const isDark = theme === "dark" || (theme === "system" && window.matchMedia("(prefers-color-scheme: dark)").matches);

  return (
    <nav
      className="flex w-[var(--spacing-nav)] flex-col items-center gap-2 pt-2"
      role="navigation"
      aria-label="Main navigation"
    >
      {/* User avatar — click to edit profile (WeChat-style top placement) */}
      <Tooltip content={t("navBar.editProfile")} variant="plain" position="right">
        <button
          onClick={onAvatarClick}
          className="mb-3 flex items-center justify-center rounded-md transition-colors duration-150 hover:ring-2 hover:ring-zinc-400 dark:hover:ring-zinc-500"
          aria-label={t("navBar.editProfile")}
        >
          <UserAvatar
            displayName={profile.displayName}
            avatarUrl={profile.backendAvatarUrl ?? null}
            builtinAvatarId={profile.backendBuiltinAvatarId ?? null}
            size={40}
            className="shrink-0"
          />
        </button>
      </Tooltip>

      {/* Top navigation items */}
      {topNavItems.map(({ view, icon: Icon, i18nKey }) => (
        <NavButton
          key={view}
          active={currentView === view}
          onClick={() => onViewChange(view)}
          tooltip={t(i18nKey)}
          tooltipPosition="right"
        >
          {currentView === view ? (
            view === "chat" ? (
              <FilledChatIcon className="h-6 w-6" />
            ) : view === "harness" ? (
              <FilledHarnessIcon className="h-6 w-6" />
            ) : view === "docs" ? (
              <FilledDocsIcon className="h-6 w-6" />
            ) : view === "projects" ? (
              <FilledProjectsIcon className="h-6 w-6" />
            ) : (
              <FilledSettingsIcon className="h-6 w-6" />
            )
          ) : (
            view === "projects" ? (
              <OutlineProjectsIcon className="h-6 w-6" isDark={isDark} />
            ) : (
              <Icon className="h-6 w-6" />
            )
          )}
        </NavButton>
      ))}

      {/* Spacer */}
      <div className="flex-1" />

      {/* Bottom navigation items */}
      {bottomNavItems.map(({ view, icon: Icon, i18nKey }) => (
        <NavButton
          key={view}
          active={currentView === view}
          onClick={() => onViewChange(view)}
          tooltip={t(i18nKey)}
          tooltipPosition="right"
        >
          {currentView === view ? (
            <FilledSettingsIcon className="h-6 w-6" />
          ) : (
            <Icon className="h-6 w-6" />
          )}
        </NavButton>
      ))}
    </nav>
  );
}
