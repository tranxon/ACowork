import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "../../i18n/useTranslation";
import type { NodeInfo } from "../../lib/types";
import { fetchNodes } from "../../lib/gateway-api";
import { cn } from "../../lib/utils";

/**
 * Node management tab (ADR-055 §6.13.3 / Phase 3g).
 *
 * Renders the Gateway's node topology from `GET /api/nodes` — the
 * daemon's in-memory `NodeRegistry` (LWT online state + retained
 * `NodeInfo` metadata). Read-only: node lifecycle commands (rename /
 * leave / service) stay on the `acowork-node` CLI by design (§6.13.5).
 */
export function NodesTab() {
  const { t } = useTranslation();
  const [nodes, setNodes] = useState<NodeInfo[]>([]);
  const [loading, setLoading] = useState(false);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      setNodes(await fetchNodes());
    } catch {
      // Gateway unreachable — show the empty state.
      setNodes([]);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  return (
    <div className="max-w-3xl space-y-3">
      <div className="flex items-center justify-between">
        <h2 className="text-xs font-medium">{t("settings.nodesTitle")}</h2>
        <button
          onClick={() => void load()}
          disabled={loading}
          className="rounded btn-solid px-3 py-[var(--ui-btn-py)] text-xs font-medium disabled:opacity-50"
        >
          {t("settings.nodesRefresh")}
        </button>
      </div>

      <div className="overflow-hidden rounded-md border border-zinc-200 bg-modal-surface dark:border-zinc-700">
        {nodes.length === 0 ? (
          <p className="p-4 text-xs text-zinc-400">
            {loading ? t("settings.loading") : t("settings.nodesEmpty")}
          </p>
        ) : (
          <table className="w-full text-left text-xs">
            <thead className="border-b border-zinc-200 text-zinc-500 dark:border-zinc-700">
              <tr>
                <th className="px-3 py-2 font-medium">{t("settings.nodesColNodeId")}</th>
                <th className="px-3 py-2 font-medium">{t("settings.nodesColStatus")}</th>
                <th className="px-3 py-2 font-medium">{t("settings.nodesColOs")}</th>
                <th className="px-3 py-2 font-medium">{t("settings.nodesColArch")}</th>
                <th className="px-3 py-2 font-medium">{t("settings.nodesColVersion")}</th>
                <th className="px-3 py-2 font-medium">{t("settings.nodesColHostname")}</th>
                <th className="px-3 py-2 text-right font-medium">{t("settings.nodesColAgents")}</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-zinc-100 dark:divide-zinc-800">
              {nodes.map((node) => (
                <tr key={node.node_id}>
                  <td className="px-3 py-2 font-medium">{node.node_id}</td>
                  <td className="px-3 py-2">
                    <span className="inline-flex items-center gap-1.5">
                      <span
                        className={cn(
                          "h-2 w-2 rounded-full",
                          node.online ? "bg-[var(--color-accent)]" : "bg-zinc-400",
                        )}
                      />
                      <span className={node.online ? "text-[var(--color-accent)]" : "text-zinc-500"}>
                        {node.online ? t("settings.nodesOnline") : t("settings.nodesOffline")}
                      </span>
                    </span>
                  </td>
                  <td className="px-3 py-2 text-zinc-600 dark:text-zinc-300">{node.os ?? "—"}</td>
                  <td className="px-3 py-2 text-zinc-600 dark:text-zinc-300">{node.arch ?? "—"}</td>
                  <td className="px-3 py-2 text-zinc-600 dark:text-zinc-300">{node.node_version ?? "—"}</td>
                  <td className="px-3 py-2 text-zinc-600 dark:text-zinc-300">{node.hostname ?? "—"}</td>
                  <td className="px-3 py-2 text-right text-zinc-600 dark:text-zinc-300">
                    {node.agent_count ?? "—"}
                    {node.max_agents !== undefined && <span className="text-zinc-400">/{node.max_agents}</span>}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
    </div>
  );
}
