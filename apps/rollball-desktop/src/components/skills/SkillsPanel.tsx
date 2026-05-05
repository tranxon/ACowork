import { useState, useEffect, useRef } from "react";
import { Wrench, ChevronDown, FolderPlus } from "lucide-react";
import { cn } from "../../lib/utils";
import { useSkillStore } from "../../stores/skillStore";
import { useAgentStore } from "../../stores/agentStore";

export function SkillsPanel() {
  const { selectedAgentId } = useAgentStore();
  const { skills, loading, fetchSkills } = useSkillStore();
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  // Load skills when agent changes or dropdown opens
  useEffect(() => {
    if (!selectedAgentId) return;
    void fetchSkills(selectedAgentId);
  }, [selectedAgentId, fetchSkills]);

  // Close on outside click
  useEffect(() => {
    if (!open) return;
    const handler = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [open]);

  const handleImportSkills = () => {
    // TODO: Open directory picker to import skills from external directory
    setOpen(false);
  };

  const skillCount = skills.length;

  return (
    <>
      <div ref={ref} className="relative inline-block">
        {/* Trigger button */}
        <button
          type="button"
          onClick={() => setOpen(!open)}
          className={cn(
            "inline-flex items-center gap-1 rounded-lg px-2 py-1 text-xs transition-colors",
            "text-zinc-500 hover:bg-zinc-200 dark:hover:bg-zinc-700 hover:text-zinc-700 dark:hover:text-zinc-200",
            open && "bg-zinc-200 dark:bg-zinc-700 text-zinc-900 dark:text-zinc-100",
          )}
        >
          <Wrench size={14} />
          <span className="max-w-[80px] truncate font-medium">
            {skillCount > 0 ? `${skillCount} Skills` : "Skills"}
          </span>
          <ChevronDown className="h-3 w-3 text-zinc-400" />
        </button>

        {/* Dropdown menu */}
        {open && (
          <div className="absolute bottom-full left-0 mb-2 w-72 rounded-lg border border-zinc-200 bg-white shadow-lg dark:border-zinc-700 dark:bg-zinc-800" style={{ zIndex: 100 }}>
            {/* Skills list */}
            <div className="max-h-56 overflow-y-auto py-1">
              {loading && skills.length === 0 ? (
                <div className="py-4 text-center text-xs text-zinc-400">Loading...</div>
              ) : skills.length === 0 ? (
                <div className="py-4 text-center text-xs text-zinc-400">No skills loaded</div>
              ) : (
                <div className="space-y-0.5">
                  {skills.map((skill) => (
                    <div
                      key={skill.name}
                      className="flex items-center gap-2 px-2 py-1.5 hover:bg-zinc-50 dark:hover:bg-zinc-700/50"
                    >
                      <Wrench className="h-3.5 w-3.5 shrink-0 text-zinc-400" />
                      <div className="min-w-0 flex-1">
                        <div className="truncate text-xs text-zinc-800 dark:text-zinc-200">
                          {skill.name}
                        </div>
                        {skill.description && (
                          <div className="truncate text-[10px] text-zinc-500 dark:text-zinc-400">
                            {skill.description}
                          </div>
                        )}
                      </div>
                      {skill.triggers.length > 0 && (
                        <span className="shrink-0 rounded bg-zinc-100 px-1.5 py-0.5 text-[10px] text-zinc-500 dark:bg-zinc-700 dark:text-zinc-400">
                          {skill.triggers.length}
                        </span>
                      )}
                    </div>
                  ))}
                </div>
              )}
            </div>

            {/* Divider */}
            <div className="border-t border-zinc-200 dark:border-zinc-700" />

            {/* Import Skills button */}
            <div className="p-2">
              <button
                onClick={handleImportSkills}
                className="mx-1.5 flex w-[calc(100%-0.75rem)] items-center justify-center gap-1.5 rounded-md bg-zinc-100 px-3 py-2 text-xs font-medium text-zinc-700 transition-colors hover:bg-zinc-200 dark:bg-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-600"
              >
                <FolderPlus className="h-3.5 w-3.5" />
                Import Skills
              </button>
            </div>
          </div>
        )}
      </div>
    </>
  );
}
