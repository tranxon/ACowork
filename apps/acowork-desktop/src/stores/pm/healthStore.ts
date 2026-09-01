/**
 * usePmHealthStore — PM 服务健康检查（离线降级）。
 *
 * 对齐 UX 设计 §7：启动时 + 每 30s 轮询 `/api/pm/projects`（轻量探测）。
 * 连续 3 次失败 → healthy=false（触发离线降级 UI）。
 *
 * 注意：服务端路由树无 `/api/pm/health` 端点，故用项目列表作为可达性探测
 * （200 = 服务在线；503/网络错误 = 离线）。
 */

import { create } from "zustand";
import { getGatewayUrl } from "../../lib/config";
import { log } from "../../lib/logger";

interface PmHealthState {
  healthy: boolean | null; // null = 未探测
  consecutiveFailures: number;
  checking: boolean;

  check: () => Promise<boolean>;
  markOffline: () => void;
}

const MAX_CONSECUTIVE_FAILURES = 3;

export const usePmHealthStore = create<PmHealthState>((set, get) => ({
  healthy: null,
  consecutiveFailures: 0,
  checking: false,

  check: async () => {
    if (get().checking) return get().healthy ?? false;
    set({ checking: true });
    try {
      const res = await fetch(`${getGatewayUrl()}/api/pm/projects`, {
        method: "GET",
        // 快速超时：3s 内无响应视为离线
        signal: AbortSignal.timeout(3000),
      });
      const ok = res.ok || res.status === 404; // 404 说明服务在但路由未挂全，仍视为在线
      if (ok) {
        set({ healthy: true, consecutiveFailures: 0, checking: false });
      } else {
        const failures = get().consecutiveFailures + 1;
        set({
          consecutiveFailures: failures,
          healthy: failures >= MAX_CONSECUTIVE_FAILURES ? false : get().healthy,
          checking: false,
        });
      }
      return ok;
    } catch (e) {
      const failures = get().consecutiveFailures + 1;
      set({
        consecutiveFailures: failures,
        healthy: failures >= MAX_CONSECUTIVE_FAILURES ? false : get().healthy,
        checking: false,
      });
      log.debug("[pm:health] check failed:", e);
      return false;
    }
  },

  markOffline: () => set({ healthy: false, consecutiveFailures: MAX_CONSECUTIVE_FAILURES }),
}));
