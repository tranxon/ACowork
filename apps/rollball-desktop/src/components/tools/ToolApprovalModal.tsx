import { usePermissionStore } from "../../stores/permissionStore";
import { AlertTriangle, Loader2 } from "lucide-react";
import { cn } from "../../lib/utils";

export function ToolApprovalModal() {
  const { currentRequest, loading, approve, dismissCurrent } = usePermissionStore();

  if (!currentRequest) {
    return null;
  }

  const handleAction = (action: "allow" | "deny" | "allow_all_session") => {
    if (loading) return;
    void approve(currentRequest.request_id, action);
  };

  const riskColor =
    currentRequest.risk_level === "High"
      ? "bg-red-100 text-red-800 dark:bg-red-900/30 dark:text-red-300"
      : currentRequest.risk_level === "Medium"
        ? "bg-amber-100 text-amber-800 dark:bg-amber-900/30 dark:text-amber-300"
        : "bg-green-100 text-green-800 dark:bg-green-900/30 dark:text-green-300";

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      {/* Backdrop */}
      <div
        className="absolute inset-0 bg-black/50"
        onClick={() => {
          if (!loading) dismissCurrent();
        }}
      />

      {/* Modal */}
      <div className="relative z-10 w-full max-w-lg rounded-lg border border-zinc-200 bg-white shadow-lg dark:border-zinc-700 dark:bg-zinc-900">
        {/* Header */}
        <div className="flex items-center gap-2 border-b border-zinc-200 px-5 py-4 dark:border-zinc-800">
          <AlertTriangle className="h-5 w-5 text-amber-500" />
          <h2 className="text-base font-semibold text-zinc-900 dark:text-zinc-100">
            Tool Approval Required
          </h2>
        </div>

        {/* Body */}
        <div className="space-y-4 px-5 py-4">
          {/* Tool info */}
          <div className="flex items-center gap-3">
            <span className="text-sm font-medium text-zinc-700 dark:text-zinc-300">
              Tool:
            </span>
            <span className="rounded bg-zinc-100 px-2 py-0.5 text-sm font-mono text-zinc-800 dark:bg-zinc-800 dark:text-zinc-200">
              {currentRequest.tool_name}
            </span>
            <span
              className={cn(
                "rounded px-2 py-0.5 text-xs font-semibold uppercase",
                riskColor,
              )}
            >
              {currentRequest.risk_level}
            </span>
          </div>

          <div className="text-sm text-zinc-600 dark:text-zinc-400">
            <span className="font-medium">Agent:</span>{" "}
            {currentRequest.agent_id}
          </div>

          {/* Shell command preview */}
          {currentRequest.shell_command && (
            <div>
              <p className="text-xs font-semibold uppercase tracking-wider text-zinc-500 dark:text-zinc-400">
                Command Preview
              </p>
              <div className="mt-1.5 rounded-md border border-zinc-200 bg-zinc-950 p-3 dark:border-zinc-700">
                <code className="block text-sm font-mono text-zinc-100">
                  {currentRequest.shell_command.preview ||
                    currentRequest.shell_command.command}
                </code>
              </div>
              {currentRequest.shell_command.risk_assessment && (
                <p className="mt-1 text-xs text-red-500 dark:text-red-400">
                  {currentRequest.shell_command.risk_assessment}
                </p>
              )}
            </div>
          )}

          {/* Parameters */}
          <div>
            <p className="text-xs font-semibold uppercase tracking-wider text-zinc-500 dark:text-zinc-400">
              Parameters
            </p>
            <pre className="mt-1.5 max-h-40 overflow-auto rounded-md border border-zinc-200 bg-zinc-50 p-3 text-xs text-zinc-700 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-300">
              {JSON.stringify(currentRequest.params, null, 2)}
            </pre>
          </div>
        </div>

        {/* Footer actions */}
        <div className="flex items-center justify-end gap-2 border-t border-zinc-200 px-5 py-4 dark:border-zinc-800">
          {loading && (
            <Loader2 className="mr-2 h-4 w-4 animate-spin text-zinc-400" />
          )}
          <button
            onClick={() => handleAction("deny")}
            disabled={loading}
            className="rounded-md border border-red-200 px-4 py-2 text-sm font-medium text-red-700 hover:bg-red-50 disabled:opacity-50 dark:border-red-900 dark:text-red-400 dark:hover:bg-red-950"
          >
            Deny
          </button>
          <button
            onClick={() => handleAction("allow")}
            disabled={loading}
            className="rounded-md bg-blue-600 px-4 py-2 text-sm font-medium text-white hover:bg-blue-700 disabled:opacity-50 dark:bg-blue-600 dark:hover:bg-blue-500"
          >
            Allow
          </button>
          <button
            onClick={() => handleAction("allow_all_session")}
            disabled={loading}
            className="rounded-md bg-green-600 px-4 py-2 text-sm font-medium text-white hover:bg-green-700 disabled:opacity-50 dark:bg-green-600 dark:hover:bg-green-500"
          >
            Allow Session
          </button>
        </div>
      </div>
    </div>
  );
}
