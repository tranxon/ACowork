/**
 * useDocHealthStore — doc 服务健康检查（离线降级）。
 *
 * ⚠️ Selector 契约（强制）：selector 必须返回稳定引用（store 字段或函数引用），
 * 禁止在 selector 内创建新对象（zustand v5 useSyncExternalStore 陷阱），
 * 派生数据请 `useMemo`。
 *
 * 探测端点：`{gw}/api/doc/health`（doc_proxy 透明转发到 doc 进程 /health）。
 * 启动 + 每 30s 轮询；连续 3 次失败 → healthy=false（触发离线降级 UI）。
 */

import { create } from "zustand";
import { checkHealth } from "../../lib/doc-api";

interface DocHealthState {
  healthy: boolean | null; // null = 未探测
  consecutiveFailures: number;
  checking: boolean;

  check: () => Promise<boolean>;
  markOffline: () => void;
}

const MAX_CONSECUTIVE_FAILURES = 3;

export const useDocHealthStore = create<DocHealthState>((set, get) => ({
  healthy: null,
  consecutiveFailures: 0,
  checking: false,

  check: async () => {
    if (get().checking) return get().healthy ?? false;
    set({ checking: true });
    const ok = await checkHealth();
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
  },

  markOffline: () => set({ healthy: false, consecutiveFailures: MAX_CONSECUTIVE_FAILURES }),
}));
