/**
 * useDocTreeStore.refreshVisible — 30s 轮询 / 手动刷新按钮的回归测试。
 *
 * 历史 bug：轮询调 `loadDir(root)` 未传 `force:true`，`loadDir` 在 cache
 * 命中时直接 return true，导致 mcp 工具加文件后前端永远看不到。
 * `refreshVisible` 必须强制重拉所有已展开目录。
 */

import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => {
  // 每个 dirId 对应一组可解析的 getTree 响应（resolve 后才返回，便于断言并发顺序）
  const calls: string[] = [];
  let resolvers: Record<string, ((n: unknown) => void)[]> = {};
  return {
    calls,
    reset() {
      mocks.calls.length = 0;
      resolvers = {};
    },
    /** 注册一个 getTree 的可延迟响应 */
    respond(dirId: string, node: unknown) {
      const queue = resolvers[dirId];
      if (!queue || queue.length === 0) {
        throw new Error(`no pending getTree(${dirId})`);
      }
      queue.shift()!(node);
    },
    getTreeMock(dirId?: string) {
      const id = dirId ?? "root";
      mocks.calls.push(id);
      return new Promise<unknown>((resolve) => {
        resolvers[id] = resolvers[id] ?? [];
        resolvers[id].push(resolve);
      });
    },
  };
});

vi.mock("../../lib/doc-api", () => ({
  getTree: (dirId?: string) => mocks.getTreeMock(dirId),
}));

vi.mock("../../lib/logger", () => ({
  log: { warn: () => {}, info: () => {}, debug: () => {}, error: () => {} },
}));

import { useDocTreeStore } from "./treeStore";
import { DOC_ROOT_DIR_ID } from "../../lib/doc-types";
import type { DirMeta, DocMeta, DocTreeNode } from "../../lib/doc-types";

function makeNode(dirId: string, fileNames: string[]): DocTreeNode {
  const files: DocMeta[] = fileNames.map((name, i) => ({
    doc_id: `${dirId}-doc-${i}`,
    name,
    version: 1,
    created_at: "2026-01-01T00:00:00Z",
    updated_at: "2026-01-01T00:00:00Z",
    deleted: false,
  }));
  return {
    dir_id: dirId,
    name: dirId,
    path: dirId,
    files,
    dirs: [] as DirMeta[],
  };
}

describe("useDocTreeStore.refreshVisible", () => {
  beforeEach(() => {
    useDocTreeStore.getState().reset();
    mocks.reset();
  });

  it("cache 命中时仍真实拉取（修原 bug：轮询因 cache 命中完全失效）", async () => {
    // 1) 首次加载建立 cache
    const p1 = useDocTreeStore.getState().loadDir(DOC_ROOT_DIR_ID);
    mocks.respond(DOC_ROOT_DIR_ID, makeNode(DOC_ROOT_DIR_ID, ["old.md"]));
    await p1;

    // 2) 此时第二次 loadDir（无 force）应直接命中 cache —— 不发请求
    const beforeCalls = mocks.calls.length;
    const cached = await useDocTreeStore.getState().loadDir(DOC_ROOT_DIR_ID);
    expect(cached).toBe(true);
    expect(mocks.calls.length).toBe(beforeCalls); // ← 原 bug 的症状

    // 3) refreshVisible 必须真正再发请求（即使 cache 存在）
    const p2 = useDocTreeStore.getState().refreshVisible();
    expect(mocks.calls.length).toBe(beforeCalls + 1);
    mocks.respond(DOC_ROOT_DIR_ID, makeNode(DOC_ROOT_DIR_ID, ["new.md"]));
    await p2;

    expect(useDocTreeStore.getState().nodes[DOC_ROOT_DIR_ID].files[0].name).toBe("new.md");
  });

  it("只刷已展开的目录；折叠的目录不刷", async () => {
    // 准备 root + subA 两个目录的 cache（都展开过；现在折叠 subB）
    useDocTreeStore.setState({
      expanded: { [DOC_ROOT_DIR_ID]: true, subA: true, subB: false },
    });

    const p = useDocTreeStore.getState().refreshVisible();
    // 等所有 in-flight getTree 都进入 pending 队列
    await Promise.resolve();
    await Promise.resolve();

    const called = [...mocks.calls].sort();
    expect(called).toEqual([DOC_ROOT_DIR_ID, "subA"].sort());
    expect(called).not.toContain("subB");

    // 清空 pending 让 action 正常完成
    mocks.respond(DOC_ROOT_DIR_ID, makeNode(DOC_ROOT_DIR_ID, []));
    mocks.respond("subA", makeNode("subA", []));
    await p;
  });

  it("并发调用不重复触发；期间 refreshingVisible=true", async () => {
    useDocTreeStore.setState({
      expanded: { [DOC_ROOT_DIR_ID]: true },
    });

    const p1 = useDocTreeStore.getState().refreshVisible();
    // 第一次进 pending 队列
    await Promise.resolve();
    const firstBatch = mocks.calls.length; // 应该是 1

    const p2 = useDocTreeStore.getState().refreshVisible();
    await Promise.resolve();
    // 第二次应被防重入拦住，无新请求
    expect(mocks.calls.length).toBe(firstBatch);

    expect(useDocTreeStore.getState().refreshingVisible).toBe(true);

    mocks.respond(DOC_ROOT_DIR_ID, makeNode(DOC_ROOT_DIR_ID, []));
    await Promise.all([p1, p2]);

    expect(useDocTreeStore.getState().refreshingVisible).toBe(false);
  });
});