import { NavButton } from "../common/NavButton";
import { useTranslation } from "../../i18n/useTranslation";
import { useSettingsStore } from "../../stores/settingsStore";
import { Bug, Activity, Database, FolderKanban, Wrench } from "lucide-react";

type PanelTab = "debug" | "status" | "setup" | "memory" | "workspace";

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
    const { opacity, theme } = useSettingsStore();
    const isDark = theme === "dark" || (theme === "system" && window.matchMedia("(prefers-color-scheme: dark)").matches);
    const bgColor = isDark ? `rgba(41,42,44,${opacity})` : `rgba(226,227,233,${opacity})`;

    const items: NavItem[] = [
        { tab: "workspace", icon: FolderKanban, i18nKey: "resultsPanel.workspace", show: true },
        { tab: "debug", icon: Bug, i18nKey: "resultsPanel.debug", show: isDebugMode },
        { tab: "status", icon: Activity, i18nKey: "resultsPanel.status", show: true },
        { tab: "memory", icon: Database, i18nKey: "resultsPanel.memory", show: agentRunning },
        { tab: "setup", icon: Wrench, i18nKey: "resultsPanel.setup", show: agentRunning },
    ];

    return (
        <nav className="flex w-10 shrink-0 flex-col items-center gap-2 py-2 dark:border-zinc-800"
            style={{ backgroundColor: bgColor } as React.CSSProperties}
        >
            {items
                .filter((item) => item.show)
                .map(({ tab, icon: Icon, i18nKey }) => {
                    const isActive = !collapsed && activeTab === tab;
                    return (
                        <NavButton
                            key={tab}
                            active={isActive}
                            onClick={() => onTabChange(tab)}
                            tooltip={t(i18nKey)}
                            tooltipPosition="left"
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
