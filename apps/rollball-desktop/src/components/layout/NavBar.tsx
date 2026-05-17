import { useId, type ComponentType } from "react";
import type { NavView } from "../../lib/types";
import { cn } from "../../lib/utils";
import { Settings } from "lucide-react";
import { UserAvatar } from "../common/UserAvatar";
import { useUserProfileStore } from "../../stores/userProfileStore";
import { useSettingsStore } from "../../stores/settingsStore";

interface NavBarProps {
  currentView: NavView;
  onViewChange: (view: NavView) => void;
  /** Called when user clicks their avatar — navigate to profile settings */
  onAvatarClick: () => void;
}

const navItems: { view: NavView; icon: ComponentType<{ className?: string }>; label: string }[] = [
  { view: "chat", icon: OutlineChatIcon, label: "Chat" },
  { view: "settings", icon: Settings, label: "Settings" },
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
    <svg className={className} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
      <g transform="translate(1.2, 1.2) scale(0.9)">
        <path d="M12 3C6.5 3 2 7.1 2 12c0 2.5 1.1 4.8 2.9 6.5L3 22l5.3-2.3C9.6 20.5 11.2 21 13 21c5.5 0 10-4.1 10-9s-4.5-9-11-9z" />
      </g>
    </svg>
  );
}

/** Filled gear icon with center hole punched out via SVG mask */
function FilledSettingsIcon({ className }: { className?: string }) {
  const maskId = useId();
  return (
    <svg className={className} viewBox="0 0 24 24" fill="currentColor" stroke="currentColor" strokeWidth="2">
      <defs>
        <mask id={maskId}>
          <rect width="24" height="24" fill="white" />
          <circle cx="12" cy="12" r="3" fill="black" />
        </mask>
      </defs>
      <g mask={`url(#${maskId})`}>
        <path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z" />
      </g>
    </svg>
  );
}

export function NavBar({ currentView, onViewChange, onAvatarClick }: NavBarProps) {
  const profile = useUserProfileStore((s) => s.profile);
  const { opacity, theme } = useSettingsStore();
  const isDark = theme === "dark" || (theme === "system" && window.matchMedia("(prefers-color-scheme: dark)").matches);
  // Original gray: #E2E3E9 (light) / #292A2C (dark), modulated by opacity
  const bgColor = isDark ? `rgba(41,42,44,${opacity})` : `rgba(226,227,233,${opacity})`;

  return (
    <nav
      className="flex w-[var(--spacing-nav)] flex-col items-center gap-0 py-2"
      role="navigation"
      aria-label="Main navigation"
      style={{
        backgroundColor: bgColor,
      } as React.CSSProperties}
    >
      {/* User avatar — click to edit profile (WeChat-style top placement) */}
      <button
        onClick={onAvatarClick}
        className="mb-3 flex items-center justify-center rounded-md transition-colors duration-150 hover:ring-2 hover:ring-zinc-400 dark:hover:ring-zinc-500"
        title="Edit Profile"
        aria-label="Edit Profile"
      >
        <UserAvatar
          displayName={profile.displayName}
          size={40}
          className="shrink-0"
        />
      </button>

      {/* Navigation items */}
      {navItems.map(({ view, icon: Icon, label }) => (
        <button
          key={view}
          onClick={() => onViewChange(view)}
          className={cn(
            "flex h-12 w-10 items-center justify-center rounded-md transition-colors duration-150",
            currentView === view
              ? ""
              : "text-zinc-500 hover:text-zinc-600 hover:bg-zinc-200/50 dark:text-zinc-400 dark:hover:text-zinc-300 dark:hover:bg-zinc-700/50",
          )}
          style={currentView === view ? { color: "var(--color-accent-green)" } : undefined}
          title={label}
          aria-label={label}
          aria-current={currentView === view ? "page" : undefined}
        >
          {currentView === view ? (
            view === "chat" ? (
              <FilledChatIcon className="h-6 w-6" />
            ) : (
              <FilledSettingsIcon className="h-6 w-6" />
            )
          ) : (
            <Icon className="h-6 w-6" />
          )}
        </button>
      ))}
    </nav>
  );
}
