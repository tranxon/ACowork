import { NavButton } from "../common/NavButton";
import { OutlineSettingsIcon, FilledSettingsIcon } from "../common/SettingsIcon";
import { OutlineDatabaseIcon, FilledDatabaseIcon } from "../common/DatabaseIcon";
import { useTranslation } from "../../i18n/useTranslation";
import { Bug, Activity, FolderKanban, Wrench } from "lucide-react";

type PanelTab = "debug" | "status" | "setup" | "tools" | "memory" | "workspace";

interface RightNavBarProps {
  activeTab: PanelTab;
  onTabChange: (tab: PanelTab) => void;
  isDebugMode: boolean;
  agentRunning: boolean;
  collapsed: boolean;
}

interface NavItem {
  tab: PanelTab;
  /** Lucide icon component. Omitted for tabs that render a custom icon (e.g. setup). */
  icon?: typeof Bug;
  i18nKey: string;
  show: boolean;
}

export function RightNavBar({ activeTab, onTabChange, isDebugMode, agentRunning, collapsed }: RightNavBarProps) {
  const { t } = useTranslation();

  const items: NavItem[] = [
    { tab: "workspace", icon: FolderKanban, i18nKey: "resultsPanel.workspace", show: true },
    { tab: "debug", icon: Bug, i18nKey: "resultsPanel.debug", show: isDebugMode },
    { tab: "status", icon: Activity, i18nKey: "resultsPanel.status", show: true },
    // memory uses the shared FilledDatabaseIcon/OutlineDatabaseIcon so the
    // middle disk-separator line stays visible on selection; `icon` is
    // intentionally omitted.
    { tab: "memory", i18nKey: "resultsPanel.memory", show: agentRunning },
    // setup uses the shared FilledSettingsIcon/OutlineSettingsIcon so the
    // center hole is preserved on selection; `icon` is intentionally omitted.
    { tab: "setup", i18nKey: "resultsPanel.setup", show: agentRunning },
    { tab: "tools", icon: Wrench, i18nKey: "resultsPanel.tools", show: agentRunning },
  ];

  return (
    <nav className="flex w-10 shrink-0 flex-col items-center gap-2 py-2 dark:border-zinc-800">
      {items
        .filter((item) => item.show)
        .map(({ tab, icon: Icon, i18nKey }, index) => {
          const isActive = !collapsed && activeTab === tab;
          return (
            <NavButton
              key={tab}
              active={isActive}
              onClick={() => onTabChange(tab)}
              tooltip={t(i18nKey)}
              tooltipPosition="left"
              // First button's top edge aligns with the SessionTabBar/ResultsPanel border-b (~33px)
              className={index === 0 ? "mt-[25px]" : undefined}
            >
              {tab === "setup" ? (
                // Settings gear: use the shared icon so the filled state
                // preserves the center hole via SVG mask (a naïve
                // `fill="currentColor"` would erase the inner circle).
                isActive ? (
                  <FilledSettingsIcon className="h-5 w-5" />
                ) : (
                  <OutlineSettingsIcon className="h-5 w-5" />
                )
              ) : tab === "memory" ? (
                // Database/cylinder: use the shared icon so the filled state
                // preserves the middle disk-separator line via SVG mask (a
                // naïve `fill="currentColor"` would close the open arc into
                // a thin lens shape, erasing the line and flattening the
                // cylinder). The mask carves a thin groove along the arc so
                // the background shows through, restoring the 3D feel.
                isActive ? (
                  <FilledDatabaseIcon className="h-5 w-5" />
                ) : (
                  <OutlineDatabaseIcon className="h-5 w-5" />
                )
              ) : Icon ? (
                <Icon
                  className="h-5 w-5"
                  fill={isActive ? "currentColor" : "none"}
                  strokeWidth={isActive ? 1.5 : 1.75}
                />
              ) : null}
            </NavButton>
          );
        })}
    </nav>
  );
}
