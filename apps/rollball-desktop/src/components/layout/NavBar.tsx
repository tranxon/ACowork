import type { NavView } from "../../lib/types";
import { cn } from "../../lib/utils";
import { MessageSquare, Settings } from "lucide-react";

interface NavBarProps {
  currentView: NavView;
  onViewChange: (view: NavView) => void;
}

const navItems: { view: NavView; icon: typeof MessageSquare; label: string }[] = [
  { view: "chat", icon: MessageSquare, label: "Chat" },
  { view: "settings", icon: Settings, label: "Settings" },
];

export function NavBar({ currentView, onViewChange }: NavBarProps) {
  return (
    <nav
      className="flex w-[48px] flex-col items-center border-r border-zinc-200 bg-zinc-50 py-2 dark:border-zinc-800 dark:bg-zinc-900"
      role="navigation"
      aria-label="Main navigation"
    >
      {navItems.map(({ view, icon: Icon, label }) => (
        <button
          key={view}
          onClick={() => onViewChange(view)}
          className={cn(
            "flex h-10 w-10 items-center justify-center rounded-md transition-colors duration-150",
            currentView === view
              ? "bg-zinc-200 text-zinc-900 dark:bg-zinc-700 dark:text-zinc-100"
              : "text-zinc-400 hover:bg-zinc-100 hover:text-zinc-600 dark:text-zinc-500 dark:hover:bg-zinc-800 dark:hover:text-zinc-300",
          )}
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
