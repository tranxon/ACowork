import type { NavView } from "../../lib/types";
import { cn } from "../../lib/utils";
import { MessageSquare, Settings } from "lucide-react";
import { UserAvatar } from "../common/UserAvatar";
import { useUserProfileStore } from "../../stores/userProfileStore";
import { useSettingsStore } from "../../stores/settingsStore";

interface NavBarProps {
  currentView: NavView;
  onViewChange: (view: NavView) => void;
  /** Called when user clicks their avatar — navigate to profile settings */
  onAvatarClick: () => void;
}

const navItems: { view: NavView; icon: typeof MessageSquare; label: string }[] = [
  { view: "chat", icon: MessageSquare, label: "Chat" },
  { view: "settings", icon: Settings, label: "Settings" },
];

export function NavBar({ currentView, onViewChange, onAvatarClick }: NavBarProps) {
  const profile = useUserProfileStore((s) => s.profile);
  const { opacity, theme } = useSettingsStore();
  const isDark = theme === "dark" || (theme === "system" && window.matchMedia("(prefers-color-scheme: dark)").matches);
  // Original gray: #E2E3E9 (light) / #292A2C (dark), modulated by opacity
  const bgColor = isDark ? `rgba(41,42,44,${opacity})` : `rgba(226,227,233,${opacity})`;

  return (
    <nav
      className="flex w-[48px] flex-col items-center gap-0 py-2"
      role="navigation"
      aria-label="Main navigation"
      style={{
        backgroundColor: bgColor,
      } as React.CSSProperties}
    >
      {/* User avatar — click to edit profile (WeChat-style top placement) */}
      <button
        onClick={onAvatarClick}
        className="flex items-center justify-center rounded-md transition-colors duration-150 hover:ring-2 hover:ring-zinc-400 dark:hover:ring-zinc-500"
        title="Edit Profile"
        aria-label="Edit Profile"
      >
        <UserAvatar
          displayName={profile.displayName}
          size={40}
          className="shrink-0"
        />
      </button>

      {/* Divider */}
      <div className="my-2 h-px w-6 bg-zinc-300 dark:bg-zinc-600" />

      {/* Navigation items */}
      {navItems.map(({ view, icon: Icon, label }) => (
        <button
          key={view}
          onClick={() => onViewChange(view)}
          className={cn(
            "flex h-10 w-10 items-center justify-center rounded-md transition-colors duration-150",
            currentView === view
              ? ""
              : "text-zinc-400 hover:text-zinc-600 hover:bg-zinc-200/50 dark:text-zinc-500 dark:hover:text-zinc-300 dark:hover:bg-zinc-700/50",
          )}
          style={currentView === view ? { color: "var(--color-accent-green)" } : undefined}
          title={label}
          aria-label={label}
          aria-current={currentView === view ? "page" : undefined}
        >
          <Icon className="h-5 w-5" />
        </button>
      ))}
    </nav>
  );
}
