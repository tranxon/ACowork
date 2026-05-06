import { useState, useEffect, useRef, useCallback } from "react";
import { Wrench, ChevronDown, FolderPlus, Check, Loader2, AlertCircle, X } from "lucide-react";
import { cn } from "../../lib/utils";
import { useSkillStore } from "../../stores/skillStore";
import { useAgentStore } from "../../stores/agentStore";

export function SkillsPanel() {
  const { selectedAgentId } = useAgentStore();
  const {
    skills,
    loading,
    fetchSkills,
    activeSkill,
    setActiveSkill,
    clearActiveSkill,
    importSkill,
  } = useSkillStore();
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  // Import dialog state
  const [importDialogOpen, setImportDialogOpen] = useState(false);
  const [selectedFile, setSelectedFile] = useState<File | null>(null);
  const [importing, setImporting] = useState(false);
  const [importError, setImportError] = useState<string | null>(null);
  const [importSuccess, setImportSuccess] = useState<string | null>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);

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

  const handleImportClick = () => {
    setOpen(false);
    setImportDialogOpen(true);
    setSelectedFile(null);
    setImportError(null);
    setImportSuccess(null);
  };

  const handleFileSelect = (e: React.ChangeEvent<HTMLInputElement>) => {
    const file = e.target.files?.[0];
    if (file) {
      setSelectedFile(file);
      setImportError(null);
    }
  };

  const handleDrop = useCallback((e: React.DragEvent) => {
    e.preventDefault();
    e.stopPropagation();
    const file = e.dataTransfer.files?.[0];
    if (file && file.name.endsWith(".zip")) {
      setSelectedFile(file);
      setImportError(null);
    } else {
      setImportError("Please drop a .zip file");
    }
  }, []);

  const handleDragOver = useCallback((e: React.DragEvent) => {
    e.preventDefault();
    e.stopPropagation();
  }, []);

  const handleImport = async () => {
    if (!selectedAgentId || !selectedFile) return;

    setImporting(true);
    setImportError(null);
    setImportSuccess(null);

    const result = await importSkill(selectedAgentId, selectedFile);

    setImporting(false);
    if (result.success) {
      setImportSuccess(result.message || `Skill "${result.skillName}" imported successfully`);
      setSelectedFile(null);
      // Auto-close after 2 seconds
      setTimeout(() => {
        setImportDialogOpen(false);
        setImportSuccess(null);
      }, 2000);
    } else {
      setImportError(result.message || "Import failed");
    }
  };

  const handleCloseDialog = () => {
    setImportDialogOpen(false);
    setSelectedFile(null);
    setImportError(null);
    setImportSuccess(null);
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
                  {skills.map((skill) => {
                    const isActive = activeSkill?.name === skill.name;
                    return (
                      <button
                        key={skill.name}
                        type="button"
                        onClick={() => {
                          if (isActive) {
                            clearActiveSkill();
                          } else {
                            setActiveSkill(skill);
                          }
                          setOpen(false);
                        }}
                        className={cn(
                          "flex w-full items-center gap-2 px-2 py-1.5 text-left transition-colors",
                          isActive
                            ? "bg-blue-50 dark:bg-blue-900/20"
                            : "hover:bg-zinc-50 dark:hover:bg-zinc-700/50",
                        )}
                      >
                        <Wrench className={cn("h-3.5 w-3.5 shrink-0", isActive ? "text-blue-500" : "text-zinc-400")} />
                        <div className="min-w-0 flex-1">
                          <div className={cn("truncate text-xs", isActive ? "text-blue-700 dark:text-blue-300 font-medium" : "text-zinc-800 dark:text-zinc-200")}>
                            {skill.name}
                          </div>
                          {skill.description && (
                            <div className="truncate text-[10px] text-zinc-500 dark:text-zinc-400">
                              {skill.description}
                            </div>
                          )}
                        </div>
                        {isActive && (
                          <Check className="h-3.5 w-3.5 shrink-0 text-blue-500" />
                        )}
                        {!isActive && skill.triggers.length > 0 && (
                          <span className="shrink-0 rounded bg-zinc-100 px-1.5 py-0.5 text-[10px] text-zinc-500 dark:bg-zinc-700 dark:text-zinc-400">
                            {skill.triggers.length}
                          </span>
                        )}
                      </button>
                    );
                  })}
                </div>
              )}
            </div>

            {/* Divider */}
            <div className="border-t border-zinc-200 dark:border-zinc-700" />

            {/* Import Skills button */}
            <div className="p-2">
              <button
                onClick={handleImportClick}
                className="mx-1.5 flex w-[calc(100%-0.75rem)] items-center justify-center gap-1.5 rounded-md bg-zinc-100 px-3 py-2 text-xs font-medium text-zinc-700 transition-colors hover:bg-zinc-200 dark:bg-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-600"
              >
                <FolderPlus className="h-3.5 w-3.5" />
                Import Skills
              </button>
            </div>
          </div>
        )}
      </div>

      {/* Import Dialog */}
      {importDialogOpen && (
        <div className="fixed inset-0 z-[200] flex items-center justify-center bg-black/50">
          <div className="w-96 rounded-lg border border-zinc-200 bg-white p-6 shadow-xl dark:border-zinc-700 dark:bg-zinc-800">
            {/* Header */}
            <div className="mb-4 flex items-center justify-between">
              <h3 className="text-sm font-semibold text-zinc-900 dark:text-zinc-100">
                Import Skill
              </h3>
              <button
                onClick={handleCloseDialog}
                className="rounded p-1 text-zinc-400 hover:bg-zinc-100 hover:text-zinc-600 dark:hover:bg-zinc-700 dark:hover:text-zinc-300"
              >
                <X className="h-4 w-4" />
              </button>
            </div>

            {/* Description */}
            <p className="mb-4 text-xs text-zinc-500 dark:text-zinc-400">
              Select a skill ZIP package to import. The ZIP must contain a{" "}
              <code className="rounded bg-zinc-100 px-1 py-0.5 text-zinc-700 dark:bg-zinc-700 dark:text-zinc-300">
                SKILL.md
              </code>{" "}
              file with YAML frontmatter.
            </p>

            {/* Drop zone */}
            <div
              onDrop={handleDrop}
              onDragOver={handleDragOver}
              onClick={() => fileInputRef.current?.click()}
              className={cn(
                "mb-3 cursor-pointer rounded-lg border-2 border-dashed p-6 text-center transition-colors",
                selectedFile
                  ? "border-blue-300 bg-blue-50 dark:border-blue-700 dark:bg-blue-900/20"
                  : "border-zinc-300 hover:border-zinc-400 dark:border-zinc-600 dark:hover:border-zinc-500",
              )}
            >
              <input
                ref={fileInputRef}
                type="file"
                accept=".zip"
                onChange={handleFileSelect}
                className="hidden"
              />
              {selectedFile ? (
                <div className="text-xs">
                  <div className="mb-1 font-medium text-blue-700 dark:text-blue-300">
                    {selectedFile.name}
                  </div>
                  <div className="text-zinc-500 dark:text-zinc-400">
                    {(selectedFile.size / 1024).toFixed(1)} KB
                  </div>
                </div>
              ) : (
                <div className="text-xs text-zinc-500 dark:text-zinc-400">
                  <FolderPlus className="mx-auto mb-2 h-6 w-6" />
                  <div>Click to select or drop a .zip file</div>
                </div>
              )}
            </div>

            {/* Error / Success messages */}
            {importError && (
              <div className="mb-3 flex items-center gap-2 rounded-md bg-red-50 p-2 text-xs text-red-700 dark:bg-red-900/20 dark:text-red-300">
                <AlertCircle className="h-3.5 w-3.5 shrink-0" />
                {importError}
              </div>
            )}
            {importSuccess && (
              <div className="mb-3 flex items-center gap-2 rounded-md bg-green-50 p-2 text-xs text-green-700 dark:bg-green-900/20 dark:text-green-300">
                <Check className="h-3.5 w-3.5 shrink-0" />
                {importSuccess}
              </div>
            )}

            {/* Actions */}
            <div className="flex justify-end gap-2">
              <button
                onClick={handleCloseDialog}
                className="rounded-md px-3 py-1.5 text-xs font-medium text-zinc-600 transition-colors hover:bg-zinc-100 dark:text-zinc-300 dark:hover:bg-zinc-700"
              >
                Cancel
              </button>
              <button
                onClick={handleImport}
                disabled={!selectedFile || importing}
                className={cn(
                  "inline-flex items-center gap-1.5 rounded-md px-3 py-1.5 text-xs font-medium transition-colors",
                  !selectedFile || importing
                    ? "cursor-not-allowed bg-zinc-200 text-zinc-400 dark:bg-zinc-700 dark:text-zinc-500"
                    : "bg-blue-600 text-white hover:bg-blue-700 dark:bg-blue-600 dark:hover:bg-blue-500",
                )}
              >
                {importing && <Loader2 className="h-3 w-3 animate-spin" />}
                {importing ? "Importing..." : "Import"}
              </button>
            </div>
          </div>
        </div>
      )}
    </>
  );
}
