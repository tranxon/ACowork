/**
 * useDocRequestStore — 待审核更新请求（PR 式审核流，设计 §5）。
 *
 * 人类在 docs 视图顶部查看 pending 队列 → approve/reject。
 * - approve 成功：服务端合并入库（doc version+1）；本地移除该请求，
 *   并记 `lastReviewed`（DocsView 据此 toast + 若目标文档正打开，
 *   推合并内容给编辑器 `applyMergedUpdate`）。
 * - reject 成功：本地移除；`lastReviewed` 记录拒绝原因供展示。
 *
 * ⚠️ Selector 契约：同 pm store（稳定引用 + useMemo 派生）。
 */

import { create } from "zustand";
import * as docApi from "../../lib/doc-api";
import { log } from "../../lib/logger";
import type { UpdateRequest } from "../../lib/doc-types";

interface ReviewedEvent {
  requestId: string;
  docId: string;
  action: "approved" | "rejected";
  /** approve 后合并的文档版本（reject 为 null） */
  docVersion: number | null;
  docName: string;
}

interface DocRequestState {
  /** pending 队列（按创建时间升序） */
  requests: UpdateRequest[];
  loading: boolean;
  error: string | null;
  /** 最近一次审核结果（DocsView 消费后 clear） */
  lastReviewed: ReviewedEvent | null;

  loadPending: () => Promise<void>;
  approve: (request: UpdateRequest, note?: string) => Promise<boolean>;
  reject: (request: UpdateRequest, note?: string) => Promise<boolean>;
  clearLastReviewed: () => void;
}

export const useDocRequestStore = create<DocRequestState>((set, get) => ({
  requests: [],
  loading: false,
  error: null,
  lastReviewed: null,

  loadPending: async () => {
    if (get().loading) return;
    set({ loading: true, error: null });
    try {
      const requests = await docApi.listRequests("pending");
      // 服务端按 created_at 排序，保持升序展示
      set({ requests, loading: false });
    } catch (e) {
      log.warn("[doc:request] loadPending failed:", e);
      set({ loading: false, error: e instanceof Error ? e.message : String(e) });
    }
  },

  approve: async (request, note) => {
    try {
      const res = await docApi.approveRequest(request.request_id, { note });
      set((s) => ({
        requests: s.requests.filter((r) => r.request_id !== request.request_id),
        lastReviewed: {
          requestId: request.request_id,
          docId: request.doc_id,
          action: "approved",
          docVersion: res.doc_version,
          docName: request.path,
        },
      }));
      return true;
    } catch (e) {
      log.warn("[doc:request] approve failed:", e);
      set({ error: e instanceof Error ? e.message : String(e) });
      return false;
    }
  },

  reject: async (request, note) => {
    try {
      await docApi.rejectRequest(request.request_id, { note });
      set((s) => ({
        requests: s.requests.filter((r) => r.request_id !== request.request_id),
        lastReviewed: {
          requestId: request.request_id,
          docId: request.doc_id,
          action: "rejected",
          docVersion: null,
          docName: request.path,
        },
      }));
      return true;
    } catch (e) {
      log.warn("[doc:request] reject failed:", e);
      set({ error: e instanceof Error ? e.message : String(e) });
      return false;
    }
  },

  clearLastReviewed: () => set({ lastReviewed: null }),
}));
