import { NavButton } from "../common/NavButton";
import { useTranslation } from "../../i18n/useTranslation";
import { Bug, Activity, Database, FolderKanban, Settings, Wrench } from "lucide-react";

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
  icon: typeof Bug;
  i18nKey: string;
  show: boolean;
}

export function RightNavBar({ activeTab, onTabChange, isDebugMode, agentRunning, collapsed }: RightNavBarProps) {
  const { t } = useTranslation();

  const items: NavItem[] = [
    { tab: "workspace", icon: FolderKanban, i18nKey: "resultsPanel.workspace", show: true },
    { tab: "debug", icon: Bug, i18nKey: "resultsPanel.debug", show: isDebugMode },
    { tab: "status", icon: Activity, i18nKey: "resultsPanel.status", show: true },
    { tab: "memory", icon: Database, i18nKey: "resultsPanel.memory", show: agentRunning },
    { tab: "setup", icon: Settings, i18nKey: "resultsPanel.setup", show: agentRunning },
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
              <Icon
                className="h-5 w-5"
                fill={isActive ? "currentColor" : "none"}
                strokeWidth={isActive ? 1.5 : 1.75}
              />
            </NavButton>
          );
        })}
    </nav>
  );
}
