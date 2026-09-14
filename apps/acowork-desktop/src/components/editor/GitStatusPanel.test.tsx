/**
 * ADR-078 frontend tests — GitStatusPanel.
 *
 * Flat list of uncommitted changes with a right-click menu. Covers:
 *   1. Row rendering: path, status icon by state, staged badge, oldPath.
 *   2. Click a normal row → openFile in the editor.
 *   3. Click a deleted row → redirect to Show Diff (ADR-078 decision 6).
 *   4. Right-click menu → Show Diff / Show Log / Open in editor actions.
 *   5. Empty states: clean, notRepo, gitUnavailable, loading, error.
 */

import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";

// ── Mocks ────────────────────────────────────────────────────────────────

const translations: Record<string, string> = {
  "gitStatus.showDiff": "Show Diff",
  "gitStatus.showLog": "Show Log",
  "gitStatus.openInEditor": "Open in Editor",
  "gitStatus.loading": "Loading…",
  "gitStatus.notRepo": "Not a Git repository",
  "gitStatus.gitUnavailable": "Git unavailable",
  "gitStatus.clean": "No changes",
  "gitStatus.staged": "staged",
  "gitStatus.renamed": "renamed",
  "gitStatus.added": "added",
  "gitStatus.conflicted": "conflicted",
  "gitStatus.deleted": "deleted",
  "gitStatus.untracked": "untracked",
  "gitStatus.modified": "modified",
  "gitStatus.noCommits": "No commits yet",
};

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({
    t: (key: string) => translations[key] ?? key,
  }),
}));

/** Mutable gitStore mock — both the selector form and getState() form. */
const gitStoreMocks = {
  status: {} as Record<string, unknown>,
  fetchDiff: vi.fn(),
  fetchLog: vi.fn(),
};

vi.mock("../../stores/gitStore", () => ({
  gitGroupKey: (a: string, w: string) => `${a}\u0000${w}`,
  useGitStore: Object.assign(
    (selector: (s: typeof gitStoreMocks) => unknown) => selector(gitStoreMocks),
    { getState: () => gitStoreMocks },
  ),
}));

const fileEditorMocks = {
  openFile: vi.fn(),
  openVirtualFile: vi.fn(),
};

vi.mock("../../stores/fileEditorStore", () => ({
  useFileEditorStore: { getState: () => fileEditorMocks },
}));

// ── SUT ──────────────────────────────────────────────────────────────────

import { GitStatusPanel } from "./GitStatusPanel";
import type { GitStatusResponse } from "../../stores/gitStore";

const KEY = "a1\u0000ws1";

function setEntry(entry: Record<string, unknown> | undefined) {
  gitStoreMocks.status[KEY] = entry;
}

function okResponse(body: Record<string, unknown>) {
  return Promise.resolve(body);
}

beforeEach(() => {
  setEntry(undefined);
  gitStoreMocks.fetchDiff.mockReset();
  gitStoreMocks.fetchLog.mockReset();
  fileEditorMocks.openFile.mockReset();
  fileEditorMocks.openVirtualFile.mockReset();
});

describe("GitStatusPanel", () => {
  it("renders change rows with path and staged badge", () => {
    setEntry({
      data: {
        isRepo: true,
        branch: "main",
        error: null,
        truncated: false,
        changes: [
          {
            path: "src/a.ts",
            oldPath: null,
            index: "modified",
            worktree: "modified",
            staged: true,
          },
        ],
      },
      loading: false,
    });
    render(<GitStatusPanel agentId="a1" workspaceId="ws1" />);
    expect(screen.getByText("src/a.ts")).toBeTruthy();
    expect(screen.getByText("staged")).toBeTruthy();
  });

  it("shows oldPath for renames", () => {
    setEntry({
      data: {
        isRepo: true,
        branch: "main",
        error: null,
        truncated: false,
        changes: [
          {
            path: "src/b.ts",
            oldPath: "src/a.ts",
            index: "renamed",
            worktree: "modified",
            staged: true,
          },
        ],
      },
      loading: false,
    });
    render(<GitStatusPanel agentId="a1" workspaceId="ws1" />);
    expect(screen.getByText("← src/a.ts")).toBeTruthy();
  });

  it("clicking a normal row opens the file in the editor", () => {
    setEntry({
      data: {
        isRepo: true,
        branch: "main",
        error: null,
        truncated: false,
        changes: [
          {
            path: "src/a.ts",
            oldPath: null,
            index: "modified",
            worktree: "modified",
            staged: false,
          },
        ],
      },
      loading: false,
    });
    render(<GitStatusPanel agentId="a1" workspaceId="ws1" />);
    fireEvent.click(screen.getByTestId("git-status-row"));
    expect(fileEditorMocks.openFile).toHaveBeenCalledWith("a1", "ws1", "src/a.ts");
  });

  it("clicking a deleted row redirects to Show Diff (decision 6)", async () => {
    setEntry({
      data: {
        isRepo: true,
        branch: "main",
        error: null,
        truncated: false,
        changes: [
          {
            path: "gone.ts",
            oldPath: null,
            index: "deleted",
            worktree: "deleted",
            staged: false,
          },
        ],
      },
      loading: false,
    });
    gitStoreMocks.fetchDiff.mockReturnValue(
      okResponse({ kind: "deleted", original: "HEAD content", modified: "" }),
    );
    render(<GitStatusPanel agentId="a1" workspaceId="ws1" />);
    fireEvent.click(screen.getByTestId("git-status-row"));

    await vi.waitFor(() => {
      expect(gitStoreMocks.fetchDiff).toHaveBeenCalledWith("a1", "ws1", "gone.ts", 0);
    });
    await vi.waitFor(() => {
      expect(fileEditorMocks.openVirtualFile).toHaveBeenCalledWith(
        expect.objectContaining({
          agentId: "a1",
          workspaceId: "ws1",
          kind: "diff",
          relPath: "gone.ts",
          content: "",
          original: "HEAD content",
          gitDiffKind: "deleted",
        }),
      );
    });
    // Deleted rows must NOT try to open the (missing) file.
    expect(fileEditorMocks.openFile).not.toHaveBeenCalled();
  });

  it("right-click opens the context menu with Show Diff / Show Log / Open", async () => {
    setEntry({
      data: {
        isRepo: true,
        branch: "main",
        error: null,
        truncated: false,
        changes: [
          {
            path: "src/a.ts",
            oldPath: null,
            index: "modified",
            worktree: "modified",
            staged: false,
          },
        ],
      },
      loading: false,
    });
    gitStoreMocks.fetchDiff.mockReturnValue(
      okResponse({ kind: "modified", original: "old", modified: "new" }),
    );
    gitStoreMocks.fetchLog.mockReturnValue(
      okResponse({
        commits: [
          {
            hash: "abc123",
            shortHash: "abc123",
            author: "me",
            date: "2025-01-01",
            subject: "init",
          },
        ],
      }),
    );

    render(<GitStatusPanel agentId="a1" workspaceId="ws1" />);
    fireEvent.contextMenu(screen.getByTestId("git-status-row"), {
      clientX: 10,
      clientY: 20,
    });

    expect(screen.getByText("Show Diff")).toBeTruthy();
    expect(screen.getByText("Show Log")).toBeTruthy();
    expect(screen.getByText("Open in Editor")).toBeTruthy();

    // Show Diff action → fetchDiff + openVirtualFile(kind: diff).
    fireEvent.click(screen.getByText("Show Diff"));
    await vi.waitFor(() => {
      expect(gitStoreMocks.fetchDiff).toHaveBeenCalledWith("a1", "ws1", "src/a.ts", 0);
    });
    await vi.waitFor(() => {
      expect(fileEditorMocks.openVirtualFile).toHaveBeenCalledWith(
        expect.objectContaining({
          agentId: "a1",
          workspaceId: "ws1",
          kind: "diff",
          relPath: "src/a.ts",
          gitDiffKind: "modified",
        }),
      );
    });
  });

  it("right-click Show Log opens a log virtual tab", async () => {
    setEntry({
      data: {
        isRepo: true,
        branch: "main",
        error: null,
        truncated: false,
        changes: [
          {
            path: "src/a.ts",
            oldPath: null,
            index: "modified",
            worktree: "modified",
            staged: false,
          },
        ],
      },
      loading: false,
    });
    gitStoreMocks.fetchLog.mockReturnValue(
      okResponse({
        commits: [
          {
            hash: "abc123",
            shortHash: "abc123",
            author: "me",
            date: "2025-01-01",
            subject: "init",
          },
        ],
      }),
    );

    render(<GitStatusPanel agentId="a1" workspaceId="ws1" />);
    fireEvent.contextMenu(screen.getByTestId("git-status-row"), {
      clientX: 10,
      clientY: 20,
    });
    fireEvent.click(screen.getByText("Show Log"));

    await vi.waitFor(() => {
      expect(gitStoreMocks.fetchLog).toHaveBeenCalledWith("a1", "ws1", "src/a.ts", 50);
    });
    await vi.waitFor(() => {
      expect(fileEditorMocks.openVirtualFile).toHaveBeenCalledTimes(1);
    });
    const call = fileEditorMocks.openVirtualFile.mock.calls[0][0];
    expect(call.kind).toBe("log");
    expect(call.relPath).toBe("src/a.ts");
    expect(call.language).toBe("plaintext");
    expect(call.content).toContain("abc123");
    expect(call.content).toContain("init");
  });

  it("renders the clean empty state", () => {
    setEntry({
      data: { isRepo: true, branch: "main", error: null, truncated: false, changes: [] },
      loading: false,
    });
    render(<GitStatusPanel agentId="a1" workspaceId="ws1" />);
    expect(screen.getByText("No changes")).toBeTruthy();
  });

  it("renders notRepo / gitUnavailable states", () => {
    setEntry({
      data: { isRepo: false, branch: null, error: "not_a_repo", truncated: false, changes: [] },
      loading: false,
    });
    const { unmount } = render(<GitStatusPanel agentId="a1" workspaceId="ws1" />);
    expect(screen.getByText("Not a Git repository")).toBeTruthy();
    unmount();

    setEntry({
      data: { isRepo: false, branch: null, error: "git_unavailable", truncated: false, changes: [] },
      loading: false,
    });
    render(<GitStatusPanel agentId="a1" workspaceId="ws1" />);
    expect(screen.getByText("Git unavailable")).toBeTruthy();
  });

  it("renders loading and error states", () => {
    setEntry({ data: null, loading: true, error: null });
    const { unmount } = render(<GitStatusPanel agentId="a1" workspaceId="ws1" />);
    expect(screen.getAllByText("Loading…").length).toBeGreaterThan(0);
    unmount();

    setEntry({ data: null, loading: false, error: "boom 500" });
    render(<GitStatusPanel agentId="a1" workspaceId="ws1" />);
    expect(screen.getByText("boom 500")).toBeTruthy();
  });

  it("maps status states to icons (untracked / deleted / staged renamed / conflicted)", () => {
    const changes: GitStatusResponse["changes"] = [
      { path: "u.txt", oldPath: null, index: "unmodified", worktree: "untracked", staged: false },
      { path: "d.txt", oldPath: null, index: "deleted", worktree: "deleted", staged: false },
      { path: "r.txt", oldPath: "old.txt", index: "renamed", worktree: "modified", staged: true },
      // Unmerged paths arrive as conflicted on both columns — must render as
      // a conflict icon, never as clean/modified (ADR-078 invariant 5).
      { path: "c.txt", oldPath: null, index: "conflicted", worktree: "conflicted", staged: false },
    ];
    setEntry({
      data: { isRepo: true, branch: "main", error: null, truncated: false, changes },
      loading: false,
    });
    const { container } = render(<GitStatusPanel agentId="a1" workspaceId="ws1" />);
    expect(container.querySelector(".lucide-file-plus")).toBeTruthy();
    expect(container.querySelector(".lucide-file-minus")).toBeTruthy();
    expect(container.querySelector(".lucide-arrow-right-left")).toBeTruthy();
    expect(container.querySelector(".lucide-git-merge")).toBeTruthy();
    // Conflicted rows must not show a staged badge.
    const cRow = screen.getByText("c.txt").closest("li");
    expect(cRow?.textContent).not.toContain("staged");
  });
});
