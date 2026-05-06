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
      className="flex w-[48px] flex-col items-center gap-2.5 bg-[#E2E3E9] py-2 dark:bg-[#292A2C]"
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
              : "text-zinc-700 hover:bg-zinc-300 hover:text-zinc-900 dark:text-zinc-300 dark:hover:bg-zinc-700 dark:hover:text-zinc-100",
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
