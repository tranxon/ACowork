/**
 * usePmTaskDetailStore — 任务详情（Drawer 数据）。
 *
 * 对齐 UX 设计 §6：打开 Drawer 时拉取详情；编辑/状态变更后刷新。
 * 关闭 Drawer 不清缓存（再次打开立即显示旧值 + 后台刷新）。
 */

import { create } from "zustand";
import * as pmApi from "../../lib/pm-api";
import { log } from "../../lib/logger";
import type { PmAttachmentMeta, PmTaskResponse } from "../../lib/pm-types";

interface PmTaskDetailState {
  taskId: string | null;
  detail: PmTaskResponse | null;
  attachments: PmAttachmentMeta[];
  loading: boolean;
  /** 附件上传中（防止重复提交） */
  uploading: boolean;
  error: string | null;

  openTask: (taskId: string) => Promise<void>;
  /** 后台静默刷新（Drawer 已打开时） */
  refresh: () => Promise<void>;
  /** 上传附件 → 成功追加到列表 */
  uploadAttachment: (file: File) => Promise<PmAttachmentMeta | null>;
  /** 删除附件 → 成功从列表移除 */
  deleteAttachment: (aid: string) => Promise<boolean>;
  updateDetail: (patch: Partial<PmTaskResponse>) => void;
  clear: () => void;
}

export const usePmTaskDetailStore = create<PmTaskDetailState>((set, get) => ({
  taskId: null,
  detail: null,
  attachments: [],
  loading: false,
  uploading: false,
  error: null,

  openTask: async (taskId) => {
    set({ taskId, loading: true, error: null });
    try {
      const [detail, attachments] = await Promise.all([
        pmApi.getTask(taskId),
        pmApi.listTaskAttachments(taskId).catch(() => [] as PmAttachmentMeta[]),
      ]);
      set({ detail, attachments, loading: false, error: null });
    } catch (e) {
      set({ loading: false, error: e instanceof Error ? e.message : String(e) });
      log.warn("[pm:detail] openTask failed:", e);
    }
  },

  refresh: async () => {
    const taskId = get().taskId;
    if (!taskId) return;
    try {
      const [detail, attachments] = await Promise.all([
        pmApi.getTask(taskId),
        pmApi.listTaskAttachments(taskId).catch(() => [] as PmAttachmentMeta[]),
      ]);
      set({ detail, attachments });
    } catch (e) {
      log.warn("[pm:detail] refresh failed:", e);
    }
  },

  uploadAttachment: async (file) => {
    const taskId = get().taskId;
    if (!taskId || get().uploading) return null;
    set({ uploading: true, error: null });
    try {
      const meta = await pmApi.uploadAttachment(taskId, file);
      set((s) => ({ attachments: [...s.attachments, meta], uploading: false }));
      return meta;
    } catch (e) {
      set({
        uploading: false,
        error: e instanceof Error ? e.message : String(e),
      });
      log.warn("[pm:detail] uploadAttachment failed:", e);
      return null;
    }
  },

  deleteAttachment: async (aid) => {
    if (get().uploading) return false;
    try {
      await pmApi.deleteAttachment(aid);
      set((s) => ({
        attachments: s.attachments.filter((a) => a.id !== aid),
      }));
      return true;
    } catch (e) {
      log.warn("[pm:detail] deleteAttachment failed:", e);
      return false;
    }
  },

  updateDetail: (patch) => {
    const detail = get().detail;
    if (!detail) return;
    set({ detail: { ...detail, ...patch } });
  },

  clear: () =>
    set({
      taskId: null,
      detail: null,
      attachments: [],
      uploading: false,
      error: null,
    }),
}));
