import { useState, useEffect, useMemo } from "react";
import { useSkillStore } from "../../stores/skillStore";
import { useAgentStore } from "../../stores/agentStore";
import { SkillDetail } from "./SkillDetail";
import { RefreshCw, AlertTriangle, Wrench } from "lucide-react";
import { cn } from "../../lib/utils";

export function SkillBrowser() {
  const { selectedAgentId } = useAgentStore();
  const {
    skills,
    total,
    selectedSkillName,
    selectedSkillDetail,
    loading,
    error,
    fetchSkills,
    selectSkill,
    clearSkills,
  } = useSkillStore();

  const [searchQuery, setSearchQuery] = useState("");

  // Load skills when agent changes
  useEffect(() => {
    if (!selectedAgentId) return;
    clearSkills();
    void fetchSkills(selectedAgentId);
  }, [selectedAgentId, clearSkills, fetchSkills]);

  const handleRefresh = () => {
    if (!selectedAgentId) return;
    void fetchSkills(selectedAgentId);
  };

  const handleSelectSkill = (skillName: string) => {
    if (!selectedAgentId) return;
    void selectSkill(selectedAgentId, skillName);
  };

  // Frontend filter
  const filteredSkills = useMemo(() => {
    if (!searchQuery.trim()) return skills;
    const q = searchQuery.toLowerCase();
    return skills.filter(
      (s) =>
        s.name.toLowerCase().includes(q) ||
        s.description.toLowerCase().includes(q) ||
        s.triggers.some((t) => t.toLowerCase().includes(q)),
    );
  }, [skills, searchQuery]);

  // ── Empty state: no agent selected ──
  if (!selectedAgentId) {
    return (
      <div className="flex flex-1 items-center justify-center bg-white dark:bg-zinc-900">
        <div className="text-center">
          <Wrench className="mx-auto h-12 w-12 text-zinc-300 dark:text-zinc-600" />
          <p className="mt-3 text-sm text-zinc-400 dark:text-zinc-500">
            Select an agent to browse skills
          </p>
        </div>
      </div>
    );
  }

  return (
    <div className="flex flex-1 flex-col bg-white dark:bg-zinc-900">
      {/* Header */}
      <div className="flex items-center justify-between border-b border-zinc-200 px-6 py-4 dark:border-zinc-800">
        <h1 className="text-xl font-semibold">Skills</h1>
        <button
          onClick={handleRefresh}
          disabled={loading}
          className="inline-flex items-center gap-1.5 rounded-md border border-zinc-200 px-3 py-1.5 text-xs font-medium text-zinc-700 hover:bg-zinc-50 disabled:opacity-50 dark:border-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-800"
        >
          <RefreshCw className={cn("h-3.5 w-3.5", loading && "animate-spin")} />
          Refresh
        </button>
      </div>

      {/* Error banner */}
      {error && (
        <div className="flex items-center gap-2 border-b border-red-200 bg-red-50 px-6 py-2 dark:border-red-900 dark:bg-red-950">
          <AlertTriangle className="h-4 w-4 text-red-600 dark:text-red-400" />
          <span className="text-xs text-red-700 dark:text-red-300">{error}</span>
        </div>
      )}

      {/* Main content: list + detail */}
      <div className="flex flex-1 overflow-hidden">
        {/* Left sidebar — skill list */}
        <div className="flex w-[35%] min-w-[240px] max-w-[360px] flex-col overflow-hidden border-r border-zinc-200 dark:border-zinc-800">
          {/* Search */}
          <div className="border-b border-zinc-200 p-3 dark:border-zinc-800">
            <input
              type="text"
              value={searchQuery}
              onChange={(e) => setSearchQuery(e.target.value)}
              placeholder="Search skills..."
              className="w-full rounded-md border border-zinc-200 bg-white px-3 py-1.5 text-sm outline-none focus:border-zinc-400 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200 dark:focus:border-zinc-500"
            />
          </div>

          {/* Skill list */}
          <div className="flex-1 overflow-y-auto">
            {loading && skills.length === 0 && (
              <div className="flex h-full items-center justify-center">
                <RefreshCw className="h-5 w-5 animate-spin text-zinc-400 dark:text-zinc-500" />
              </div>
            )}

            {!loading && filteredSkills.length === 0 && (
              <div className="flex h-full items-center justify-center text-sm text-zinc-400 dark:text-zinc-500">
                {searchQuery ? "No matching skills" : "No skills available"}
              </div>
            )}

            <div className="divide-y divide-zinc-100 dark:divide-zinc-800">
              {filteredSkills.map((skill) => {
                const isSelected = skill.name === selectedSkillName;
                return (
                  <button
                    key={skill.name}
                    onClick={() => handleSelectSkill(skill.name)}
                    className={cn(
                      "flex w-full flex-col gap-1.5 px-4 py-3 text-left transition-colors",
                      isSelected
                        ? "bg-zinc-100 dark:bg-zinc-800"
                        : "hover:bg-zinc-50 dark:hover:bg-zinc-800/50",
                    )}
                  >
                    <span className="text-sm font-medium text-zinc-800 dark:text-zinc-200">
                      {skill.name}
                    </span>
                    <p className="text-xs text-zinc-500 dark:text-zinc-400">
                      {skill.description}
                    </p>
                    {skill.triggers.length > 0 && (
                      <div className="flex flex-wrap gap-1">
                        {skill.triggers.slice(0, 3).map((t) => (
                          <span
                            key={t}
                            className="rounded bg-blue-50 px-1.5 py-0.5 text-[10px] font-medium text-blue-700 dark:bg-blue-900/30 dark:text-blue-300"
                          >
                            {t}
                          </span>
                        ))}
                        {skill.triggers.length > 3 && (
                          <span className="rounded bg-zinc-100 px-1.5 py-0.5 text-[10px] text-zinc-500 dark:bg-zinc-800 dark:text-zinc-400">
                            +{skill.triggers.length - 3}
                          </span>
                        )}
                      </div>
                    )}
                  </button>
                );
              })}
            </div>
          </div>

          {/* List footer */}
          <div className="flex items-center justify-between border-t border-zinc-200 px-4 py-2 text-xs text-zinc-500 dark:border-zinc-800 dark:text-zinc-400">
            <span>
              {total > 0 ? (
                <>
                  {filteredSkills.length} of {total}
                </>
              ) : (
                "No skills"
              )}
            </span>
          </div>
        </div>

        {/* Right panel — skill detail */}
        <div className="flex flex-1 flex-col overflow-hidden">
          <SkillDetail
            detail={selectedSkillDetail}
            loading={loading && selectedSkillName !== null && selectedSkillDetail === null}
          />
        </div>
      </div>
    </div>
  );
}
