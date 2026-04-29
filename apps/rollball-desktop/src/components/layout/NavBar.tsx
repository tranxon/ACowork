import type { NavView } from "../../lib/types";
import { cn } from "../../lib/utils";
import { MessageSquare, Wrench, Brain, Settings } from "lucide-react";

interface NavBarProps {
  currentView: NavView;
  onViewChange: (view: NavView) => void;
}

const navItems: { view: NavView; icon: typeof MessageSquare; label: string; disabled?: boolean }[] = [
  { view: "chat", icon: MessageSquare, label: "Chat" },
  { view: "memory", icon: Brain, label: "Memory" },
  { view: "skills", icon: Wrench, label: "Skills" },
  { view: "settings", icon: Settings, label: "Settings" },
];

export function NavBar({ currentView, onViewChange }: NavBarProps) {
  return (
    <nav
      className="flex w-[48px] flex-col items-center border-r border-zinc-200 bg-zinc-50 py-2 dark:border-zinc-800 dark:bg-zinc-900"
      role="navigation"
      aria-label="Main navigation"
    >
      {navItems.map(({ view, icon: Icon, label, disabled }) => (
        <button
          key={view}
          onClick={() => !disabled && onViewChange(view)}
          disabled={disabled}
          className={cn(
            "flex h-10 w-10 items-center justify-center rounded-md transition-colors duration-150",
            currentView === view && !disabled
              ? "bg-zinc-200 text-zinc-900 dark:bg-zinc-700 dark:text-zinc-100"
              : disabled
                ? "cursor-not-allowed text-zinc-300 opacity-50 dark:text-zinc-600"
                : "text-zinc-400 hover:bg-zinc-100 hover:text-zinc-600 dark:text-zinc-500 dark:hover:bg-zinc-800 dark:hover:text-zinc-300",
          )}
          title={disabled ? `${label} — Available in Developer Mode` : label}
          aria-label={label}
          aria-current={currentView === view ? "page" : undefined}
        >
          <Icon className="h-5 w-5" />
        </button>
      ))}
    </nav>
  );
}
