/**
 * usePmProjectStore — 项目列表 + 选中项。
 *
 * 对齐 UX 设计 §6.1：进入 /projects 路由触发加载；创建/删除/改名后主动 reload。
 * 数据常驻 store（离开路由不清空），后台 reload() 静默刷新。
 */

import { create } from "zustand";
import * as pmApi from "../../lib/pm-api";
import { log } from "../../lib/logger";
import type { PmProject, UpdateProjectInput } from "../../lib/pm-types";

interface PmProjectState {
  projects: PmProject[];
  selected: PmProject | null;
  loading: boolean;
  error: string | null;
  /** 上次成功加载时间戳（用于 30s 缓存） */
  updatedAt: number | null;
  /** 项目任务计数（侧边栏徽章）：pid → { total, submitted } */
  counts: Record<string, { total: number; submitted: number }>;

  loadProjects: (opts?: { silent?: boolean }) => Promise<void>;
  selectProject: (pid: string | null) => void;
  createProject: (title: string, description?: string) => Promise<PmProject | null>;
  updateProjectMeta: (pid: string, patch: UpdateProjectInput) => Promise<boolean>;
  deleteProject: (pid: string) => Promise<boolean>;
  refreshCounts: () => Promise<void>;
  clear: () => void;
}

const CACHE_MS = 30_000;

export const usePmProjectStore = create<PmProjectState>((set, get) => ({
  projects: [],
  selected: null,
  loading: false,
  error: null,
  updatedAt: null,
  counts: {},

  loadProjects: async ({ silent = false } = {}) => {
    // 缓存命中（非强制刷新且距上次 < 30s）→ 静默跳过
    if (!silent && get().updatedAt && Date.now() - get().updatedAt! < CACHE_MS) {
      return;
    }
    if (get().loading) return;
    set({ loading: !silent, error: null });
    try {
      const projects = await pmApi.listProjects();
      // 保留选中项（若仍存在）；否则自动选中第一个
      const selectedId = get().selected?.id;
      const selected =
        projects.find((p) => p.id === selectedId) ?? projects[0] ?? null;
      set({ projects, selected, loading: false, error: null, updatedAt: Date.now() });
      // 异步刷新计数徽章（不阻塞主流程）
      void get().refreshCounts();
    } catch (e) {
      set({ loading: false, error: e instanceof Error ? e.message : String(e) });
      log.warn("[pm:project] loadProjects failed:", e);
    }
  },

  selectProject: (pid) => {
    if (!pid) {
      set({ selected: null });
      return;
    }
    const found = get().projects.find((p) => p.id === pid) ?? null;
    set({ selected: found });
  },

  createProject: async (title, description = "") => {
    try {
      const project = await pmApi.createProject({ title, description });
      set((s) => ({
        projects: [...s.projects, project],
        selected: project,
        updatedAt: Date.now(),
      }));
      void get().refreshCounts();
      return project;
    } catch (e) {
      log.warn("[pm:project] createProject failed:", e);
      set({ error: e instanceof Error ? e.message : String(e) });
      return null;
    }
  },

  updateProjectMeta: async (pid, patch) => {
    try {
      const updated = await pmApi.updateProject(pid, patch);
      set((s) => ({
        projects: s.projects.map((p) => (p.id === pid ? updated : p)),
        selected: s.selected?.id === pid ? updated : s.selected,
        updatedAt: Date.now(),
      }));
      return true;
    } catch (e) {
      log.warn("[pm:project] updateProjectMeta failed:", e);
      set({ error: e instanceof Error ? e.message : String(e) });
      return false;
    }
  },

  deleteProject: async (pid) => {
    try {
      await pmApi.deleteProject(pid);
      set((s) => {
        const projects = s.projects.filter((p) => p.id !== pid);
        const selected =
          s.selected?.id === pid ? (projects[0] ?? null) : s.selected;
        const counts = { ...s.counts };
        delete counts[pid];
        return { projects, selected, counts, updatedAt: Date.now() };
      });
      return true;
    } catch (e) {
      log.warn("[pm:project] deleteProject failed:", e);
      set({ error: e instanceof Error ? e.message : String(e) });
      return false;
    }
  },

  refreshCounts: async () => {
    const projects = get().projects;
    const counts: Record<string, { total: number; submitted: number }> = {};
    await Promise.all(
      projects.map(async (p) => {
        try {
          const tasks = await pmApi.listProjectTasks(p.id);
          counts[p.id] = {
            total: tasks.length,
            submitted: tasks.filter((t) => t.status === "submitted").length,
          };
        } catch {
          // 单项目拉取失败不阻塞整体
          counts[p.id] = { total: 0, submitted: 0 };
        }
      }),
    );
    set({ counts });
  },

  clear: () => set({ projects: [], selected: null, counts: {}, updatedAt: null }),
}));
