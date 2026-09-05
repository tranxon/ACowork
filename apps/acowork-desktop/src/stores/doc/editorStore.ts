/**
 * useDocEditorStore — 当前打开文档的编辑状态。
 *
 * 乐观并发：保存必须携带上次读到的 `meta.version`（base_version）；
 * 服务端 version 不匹配 → 409 `version_conflict`，置 `conflict=true`
 * 引导用户「刷新后重试」（不静默覆盖他人/Agent 的修改）。
 *
 * 审核通过（approve）的合并内容由 ReviewQueue 经 `applyMergedUpdate`
 * 推入：编辑器无未保存修改时直接替换为新版本；有 dirty 修改时标记冲突
 * 让用户显式刷新（避免覆盖本地输入）。
 *
 * ⚠️ Selector 契约：同 pm store（稳定引用 + useMemo 派生）。
 */

import { create } from "zustand";
import * as docApi from "../../lib/doc-api";
import { log } from "../../lib/logger";
import type { DocRead } from "../../lib/doc-types";

type EditorMode = "edit" | "preview";

interface DocEditorState {
  /** 当前打开的文档（null = 空状态） */
  doc: DocRead | null;
  /** 编辑缓冲（未保存输入） */
  content: string;
  dirty: boolean;
  saving: boolean;
  loading: boolean;
  mode: EditorMode;
  /** 409 版本冲突：他人已更新，需刷新 */
  conflict: boolean;
  saveError: string | null;
  /** 最近一次保存成功时间（toast 用） */
  lastSavedAt: string | null;
  /** dirty 时请求打开的文档（等待用户确认丢弃本地修改） */
  pendingOpenDocId: string | null;

  /** 统一打开入口：同文档切回编辑；dirty 且不同文档 → 挂起待确认 */
  requestOpen: (docId: string) => Promise<"opened" | "blocked" | "failed">;
  openDoc: (docId: string) => Promise<boolean>;
  confirmPendingOpen: () => Promise<void>;
  cancelPendingOpen: () => void;
  /** 关闭当前文档 */
  closeDoc: () => void;
  setMode: (mode: EditorMode) => void;
  setContent: (content: string) => void;
  /** 保存：PUT base_version=当前版本；成功 version+1；409 → conflict */
  save: () => Promise<boolean>;
  /** 放弃本地修改，重拉服务端最新（409 引导） */
  reload: () => Promise<void>;
  /** 审核合并推送：外部更新当前文档（同 id 时生效） */
  applyMergedUpdate: (docId: string, content: string, newVersion: number) => void;
  /** 编辑框失焦/取消后清理 error */
  clearSaveError: () => void;
}

export const useDocEditorStore = create<DocEditorState>((set, get) => ({
  doc: null,
  content: "",
  dirty: false,
  saving: false,
  loading: false,
  mode: "edit",
  conflict: false,
  saveError: null,
  lastSavedAt: null,
  pendingOpenDocId: null,

  requestOpen: async (docId) => {
    const { doc, dirty } = get();
    if (doc?.meta.doc_id === docId) {
      set({ mode: "edit" });
      return "opened";
    }
    if (dirty) {
      set({ pendingOpenDocId: docId });
      return "blocked";
    }
    return (await get().openDoc(docId)) ? "opened" : "failed";
  },

  openDoc: async (docId) => {
    set({ loading: true, saveError: null, conflict: false, pendingOpenDocId: null });
    try {
      const read = await docApi.getDoc(docId);
      set({
        doc: read,
        content: read.content,
        dirty: false,
        loading: false,
        mode: "edit",
        conflict: false,
        saveError: null,
        lastSavedAt: null,
        pendingOpenDocId: null,
      });
      return true;
    } catch (e) {
      log.warn(`[doc:editor] openDoc ${docId} failed:`, e);
      set({ loading: false, saveError: e instanceof Error ? e.message : String(e) });
      return false;
    }
  },

  confirmPendingOpen: async () => {
    const target = get().pendingOpenDocId;
    if (!target) return;
    await get().openDoc(target);
  },

  cancelPendingOpen: () => set({ pendingOpenDocId: null }),

  closeDoc: () =>
    set({ doc: null, content: "", dirty: false, loading: false, mode: "edit", conflict: false, saveError: null, pendingOpenDocId: null }),

  setMode: (mode) => set({ mode }),
  setContent: (content) => set({ content, dirty: true, conflict: false }),

  save: async () => {
    const { doc, content } = get();
    if (!doc || get().saving) return false;
    const baseVersion = doc.meta.version;
    set({ saving: true, saveError: null });
    try {
      const meta = await docApi.updateDoc(doc.meta.doc_id, {
        base_version: baseVersion,
        content,
      });
      set((s) => ({
        saving: false,
        dirty: false,
        conflict: false,
        lastSavedAt: new Date().toISOString(),
        doc: s.doc ? { ...s.doc, meta, content } : s.doc,
      }));
      return true;
    } catch (e) {
      const err = e as { code?: string; status?: number; message?: string };
      const isConflict = err?.status === 409 || err?.code === "version_conflict";
      set({
        saving: false,
        conflict: isConflict,
        saveError: e instanceof Error ? e.message : String(e),
      });
      log.warn("[doc:editor] save failed:", e);
      return false;
    }
  },

  reload: async () => {
    const { doc } = get();
    if (!doc) return;
    try {
      const read = await docApi.getDoc(doc.meta.doc_id);
      set({
        doc: read,
        content: read.content,
        dirty: false,
        conflict: false,
        saveError: null,
      });
    } catch (e) {
      log.warn("[doc:editor] reload failed:", e);
      set({ saveError: e instanceof Error ? e.message : String(e) });
    }
  },

  applyMergedUpdate: (docId, content, newVersion) => {
    const { doc, dirty } = get();
    if (doc?.meta.doc_id !== docId) return;
    if (dirty) {
      // 本地有未保存修改：不覆盖，标记冲突让用户刷新（保留输入）
      set({ conflict: true, saveError: null });
      return;
    }
    set((s) => ({
      doc: s.doc
        ? {
            ...s.doc,
            content,
            meta: { ...s.doc.meta, version: newVersion, updated_at: new Date().toISOString() },
          }
        : s.doc,
      content,
      dirty: false,
      conflict: false,
      saveError: null,
    }));
  },

  clearSaveError: () => set({ saveError: null }),
}));
