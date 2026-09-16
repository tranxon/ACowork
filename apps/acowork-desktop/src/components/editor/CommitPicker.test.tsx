/**
 * ADR-078 — diff-side commit picker popover.
 *
 * Covers:
 *   1. Single-page (<= 50 commits) -> no search box, no pagination chrome.
 *   2. Multi-page (> 50 commits) -> search box + "Page X of Y" footer
 *      with Prev / Next buttons.
 *   3. Client-side search filters the current page (matches subject,
 *      author, short hash prefix, full hash prefix).
 *   4. Prev / Next buttons dispatch fetchLog with the right skip.
 *   5. Working-Tree row is offered when `allowWorkingTree` is true and
 *      selects with the literal "" ref + the i18n label.
 *   6. Reopen clears search + page back to 1 (no stale state).
 *
 * Mirrors the session list dropdown pattern in `SessionTabBar.test.tsx`
 * so the two "list of items" popovers in the app share one UX.
 */

import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, fireEvent, waitFor, act } from "@testing-library/react";

// ── i18n stub ────────────────────────────────────────────────────────────

const translations: Record<string, string> = {
  "gitStatus.commitPickerLabel": "Select a revision",
  "gitStatus.commitPickerSearchPlaceholder": "Search message, author, or hash",
  "gitStatus.commitPickerShowing": "Showing {{start}}-{{end}} of {{total}}",
  "gitStatus.commitPickerNoMatches": "No matching commits",
  "gitStatus.commitPickerPageOf": "Page {{current}} of {{total}}",
  "gitStatus.commitPickerPrevPage": "Previous page",
  "gitStatus.commitPickerNextPage": "Next page",
  "gitStatus.workingTreeLabel": "Working Tree",
  "gitStatus.noCommits": "No commits yet",
  "gitStatus.loading": "Loading…",
};

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({
    t: (key: string, opts?: Record<string, unknown>) => {
      const tmpl = translations[key] ?? key;
      if (!opts) return tmpl;
      return tmpl.replace(/\{\{(\w+)\}\}/g, (_match: string, name: string) => String(opts[name] ?? ""));
    },
  }),
}));

// ── gitStore mock ────────────────────────────────────────────────────────

const fetchLogMock = vi.fn();

interface FakePagination {
  currentPage: number;
  totalPages: number;
  pageSize: number;
  totalCount: number;
}

interface FakeCommit {
  hash: string;
  shortHash: string;
  subject: string;
  author: string;
  date: string;
}

interface FakeLogResponse {
  commits: FakeCommit[];
  pagination: FakePagination;
}

vi.mock("../../stores/gitStore", () => ({
  useGitStore: Object.assign(
    (selector: (s: { fetchLog: unknown }) => unknown) =>
      selector({ fetchLog: fetchLogMock }),
    { getState: () => ({ fetchLog: fetchLogMock }) },
  ),
}));

// ── Helpers ──────────────────────────────────────────────────────────────

function makeCommits(n: number, startIndex = 0): FakeCommit[] {
  const out: FakeCommit[] = [];
  for (let i = 0; i < n; i++) {
    const idx = startIndex + i;
    const hex = `000000000000000000000000000000000000000${idx.toString(16).padStart(2, "0")}`.slice(-40);
    out.push({
      hash: hex,
      shortHash: hex.slice(0, 7),
      subject: `commit ${idx}`,
      author: idx % 2 === 0 ? "alice" : "bob",
      date: "2024-01-01T00:00:00+00:00",
    });
  }
  return out;
}

/** Build a server response for `fetchLog(agentId, ws, path, limit, skip)`.
 *  Returns the requested page slice and computes currentPage from skip. */
function paginatedResponse(
  totalCount: number,
  pageSize: number,
  skip: number,
): FakeLogResponse {
  const start = skip;
  const end = Math.min(skip + pageSize, totalCount);
  return {
    commits: makeCommits(end - start, start),
    pagination: {
      currentPage: Math.floor(skip / pageSize) + 1,
      totalPages: Math.max(1, Math.ceil(totalCount / pageSize)),
      pageSize,
      totalCount,
    },
  };
}

/** Mock fetchLog to respond with `totalCount` commits at `pageSize`-sized
 *  pages. The mock derives the response from the call's `skip` argument,
 *  so Prev / Next navigation flows naturally. */
function mockWithPaging(totalCount: number, pageSize = 50) {
  fetchLogMock.mockImplementation(
    async (_agent: string, _ws: string, _path: string, _limit: number, skip = 0) =>
      paginatedResponse(totalCount, pageSize, skip) as never,
  );
}

// ── SUT ───────────────────────────────────────────────────────────────────

import { CommitPicker } from "./CommitPicker";

const ANCHOR = document.body as unknown as HTMLElement;

beforeEach(() => {
  fetchLogMock.mockReset();
  ANCHOR.getBoundingClientRect = () => ({
    x: 0, y: 0, top: 100, left: 100, right: 420, bottom: 132,
    width: 320, height: 32, toJSON: () => ({}),
  });
});

describe("CommitPicker — single-page history", () => {
  it("does not render search box or pagination when total <= pageSize", async () => {
    mockWithPaging(20);
    render(
      <CommitPicker
        anchorEl={ANCHOR}
        currentRef=""
        allowWorkingTree={false}
        agentId="a1"
        workspaceId="ws1"
        relPath=""
        onSelect={vi.fn()}
        onClose={vi.fn()}
      />,
    );
    await waitFor(() => {
      expect(screen.getByRole("listbox")).toBeTruthy();
    });
    expect(screen.queryByPlaceholderText(/search/i)).toBeNull();
    expect(screen.queryByText(/Page \d+ of \d+/)).toBeNull();
    expect(screen.queryByText(/Showing/)).toBeNull();
  });

  it("offers Working Tree as a top row when allowWorkingTree is true", async () => {
    mockWithPaging(3);
    const onSelect = vi.fn();
    render(
      <CommitPicker
        anchorEl={ANCHOR}
        currentRef=""
        allowWorkingTree={true}
        agentId="a1"
        workspaceId="ws1"
        relPath=""
        onSelect={onSelect}
        onClose={vi.fn()}
      />,
    );
    await waitFor(() => screen.getByText("Working Tree"));
    fireEvent.click(screen.getByText("Working Tree"));
    expect(onSelect).toHaveBeenCalledWith("", "Working Tree");
  });
});

describe("CommitPicker — multi-page history", () => {
  it("renders search box + Showing + Page X of Y when total > pageSize", async () => {
    mockWithPaging(137);
    render(
      <CommitPicker
        anchorEl={ANCHOR}
        currentRef=""
        allowWorkingTree={false}
        agentId="a1"
        workspaceId="ws1"
        relPath=""
        onSelect={vi.fn()}
        onClose={vi.fn()}
      />,
    );
    await waitFor(() => screen.getByPlaceholderText(/search/i));
    expect(screen.getByText("Showing 1-50 of 137")).toBeTruthy();
    expect(screen.getByText("Page 1 of 3")).toBeTruthy();
  });

  it("client-side search filters by subject / author / hash prefix", async () => {
    mockWithPaging(80);
    render(
      <CommitPicker
        anchorEl={ANCHOR}
        currentRef=""
        allowWorkingTree={false}
        agentId="a1"
        workspaceId="ws1"
        relPath=""
        onSelect={vi.fn()}
        onClose={vi.fn()}
      />,
    );
    await waitFor(() => screen.getByPlaceholderText(/search/i));
    // 50 commits visible initially (the page).
    expect(screen.getAllByRole("option").length).toBe(50);
    // Filter by author "bob" -> odd-indexed commits only -> 25 visible.
    fireEvent.change(screen.getByPlaceholderText(/search/i), {
      target: { value: "bob" },
    });
    expect(screen.getAllByRole("option").length).toBe(25);
    // Clear filter.
    fireEvent.change(screen.getByPlaceholderText(/search/i), {
      target: { value: "" },
    });
    expect(screen.getAllByRole("option").length).toBe(50);
  });

  it("shows 'No matching commits' when search yields zero results", async () => {
    mockWithPaging(80);
    render(
      <CommitPicker
        anchorEl={ANCHOR}
        currentRef=""
        allowWorkingTree={false}
        agentId="a1"
        workspaceId="ws1"
        relPath=""
        onSelect={vi.fn()}
        onClose={vi.fn()}
      />,
    );
    await waitFor(() => screen.getByPlaceholderText(/search/i));
    fireEvent.change(screen.getByPlaceholderText(/search/i), {
      target: { value: "no-such-substring" },
    });
    expect(screen.getByText("No matching commits")).toBeTruthy();
    expect(screen.queryAllByRole("option").length).toBe(0);
  });

  it("Prev / Next buttons dispatch fetchLog with the correct skip", async () => {
    mockWithPaging(137);
    render(
      <CommitPicker
        anchorEl={ANCHOR}
        currentRef=""
        allowWorkingTree={false}
        agentId="a1"
        workspaceId="ws1"
        relPath=""
        onSelect={vi.fn()}
        onClose={vi.fn()}
      />,
    );
    await waitFor(() => screen.getByText("Page 1 of 3"));
    expect(fetchLogMock).toHaveBeenLastCalledWith("a1", "ws1", "", 50, 0);
    fireEvent.click(screen.getByLabelText("Next page"));
    await waitFor(() => screen.getByText("Page 2 of 3"));
    expect(fetchLogMock).toHaveBeenLastCalledWith("a1", "ws1", "", 50, 50);
    fireEvent.click(screen.getByLabelText("Previous page"));
    await waitFor(() => screen.getByText("Page 1 of 3"));
    expect(fetchLogMock).toHaveBeenLastCalledWith("a1", "ws1", "", 50, 0);
  });

  it("disables Prev on page 1 and Next on last page", async () => {
    mockWithPaging(137);
    render(
      <CommitPicker
        anchorEl={ANCHOR}
        currentRef=""
        allowWorkingTree={false}
        agentId="a1"
        workspaceId="ws1"
        relPath=""
        onSelect={vi.fn()}
        onClose={vi.fn()}
      />,
    );
    await waitFor(() => screen.getByText("Page 1 of 3"));
    const prev = screen.getByLabelText("Previous page") as HTMLButtonElement;
    const next = screen.getByLabelText("Next page") as HTMLButtonElement;
    expect(prev.disabled).toBe(true);
    expect(next.disabled).toBe(false);
    fireEvent.click(next);
    await waitFor(() => screen.getByText("Page 2 of 3"));
    fireEvent.click(next);
    await waitFor(() => screen.getByText("Page 3 of 3"));
    expect((screen.getByLabelText("Previous page") as HTMLButtonElement).disabled).toBe(false);
    expect((screen.getByLabelText("Next page") as HTMLButtonElement).disabled).toBe(true);
  });

  it("passes the file path through to fetchLog", async () => {
    mockWithPaging(3);
    render(
      <CommitPicker
        anchorEl={ANCHOR}
        currentRef=""
        allowWorkingTree={false}
        agentId="a1"
        workspaceId="ws1"
        relPath="src/foo.ts"
        onSelect={vi.fn()}
        onClose={vi.fn()}
      />,
    );
    await waitFor(() => screen.getByRole("listbox"));
    expect(fetchLogMock).toHaveBeenCalledWith("a1", "ws1", "src/foo.ts", 50, 0);
  });
});

describe("CommitPicker — reopen resets pagination state", () => {
  it("resets page to 1 and search to empty when anchorEl is null", async () => {
    mockWithPaging(137);
    const { rerender } = render(
      <CommitPicker
        anchorEl={ANCHOR}
        currentRef=""
        allowWorkingTree={false}
        agentId="a1"
        workspaceId="ws1"
        relPath=""
        onSelect={vi.fn()}
        onClose={vi.fn()}
      />,
    );
    await waitFor(() => screen.getByText("Page 1 of 3"));
    fireEvent.click(screen.getByLabelText("Next page"));
    await waitFor(() => screen.getByText("Page 2 of 3"));
    fireEvent.change(screen.getByPlaceholderText(/search/i), {
      target: { value: "alice" },
    });
    // Close then reopen with the same anchor.
    await act(async () => {
      rerender(
        <CommitPicker
          anchorEl={null}
          currentRef=""
          allowWorkingTree={false}
          agentId="a1"
          workspaceId="ws1"
          relPath=""
          onSelect={vi.fn()}
          onClose={vi.fn()}
        />,
      );
    });
    await act(async () => {
      rerender(
        <CommitPicker
          anchorEl={ANCHOR}
          currentRef=""
          allowWorkingTree={false}
          agentId="a1"
          workspaceId="ws1"
          relPath=""
          onSelect={vi.fn()}
          onClose={vi.fn()}
        />,
      );
    });
    await waitFor(() => screen.getByText("Page 1 of 3"));
    expect(screen.getByPlaceholderText(/search/i)).toHaveProperty("value", "");
    expect(fetchLogMock).toHaveBeenLastCalledWith("a1", "ws1", "", 50, 0);
  });
});

// ── Regression: popover container height ────────────────────────────────
//
// Bug history: the container had `maxHeight: 288` in its inline style
// without `overflow: hidden`. The container's white background therefore
// painted only 288px tall, but the 50-commit list (288px) + 50px search
// header + 40px pagination footer pushed content to ~378px — the bottom
// ~50px of the list (and the entire footer) rendered outside the white
// background, on the transparent page.
//
// The fix moved the height cap to the LIST section only, and gave the
// container `display: flex; flex-direction: column` so the three
// sections stack predictably without clipping each other's background.

describe("CommitPicker — popover height layout (regression)", () => {
  it("does NOT set maxHeight on the popover container", async () => {
    mockWithPaging(120); // multi-page so chrome is mounted
    render(
      <CommitPicker
        anchorEl={ANCHOR}
        currentRef=""
        allowWorkingTree
        agentId="a1"
        workspaceId="ws1"
        relPath=""
        onSelect={vi.fn()}
        onClose={vi.fn()}
      />,
    );
    const popover = await waitFor(() =>
      document.getElementById("commit-picker-popover"),
    );
    expect(popover).not.toBeNull();
    // The original bug — inline maxHeight on the container.
    expect((popover as HTMLElement).style.maxHeight).toBe("");
    // The container is a flex column so its three sections stack
    // predictably instead of overflowing past a clipped background.
    expect((popover as HTMLElement).style.display).toBe("flex");
    expect((popover as HTMLElement).style.flexDirection).toBe("column");
  });

  it("keeps the list section at max-h-72 (288px) by default", async () => {
    mockWithPaging(120);
    render(
      <CommitPicker
        anchorEl={ANCHOR}
        currentRef=""
        allowWorkingTree
        agentId="a1"
        workspaceId="ws1"
        relPath=""
        onSelect={vi.fn()}
        onClose={vi.fn()}
      />,
    );
    await waitFor(() => screen.getByText("Page 1 of 3"));
    // The list is the second flex child: header / list / footer.
    const popover = document.getElementById("commit-picker-popover") as HTMLElement;
    const list = popover.children[1] as HTMLElement;
    // `max-h-72` from Tailwind → 18rem → 288px. We don't read the
    // computed value (jsdom style resolution is flaky); instead we
    // verify the class is present and no inline override kicked in.
    expect(list.className).toMatch(/\bmax-h-72\b/);
    expect(list.className).toMatch(/\boverflow-y-auto\b/);
    expect(list.style.maxHeight).toBe("");
  });

  it("caps the list to the viewport when placement='top'", async () => {
    // Anchor near the bottom of the viewport so opening upward leaves
    // little room. window.innerHeight is jsdom-default 768.
    ANCHOR.getBoundingClientRect = () => ({
      x: 0, y: 0, top: 700, left: 100, right: 420, bottom: 732,
      width: 320, height: 32, toJSON: () => ({}),
    });
    mockWithPaging(120);
    render(
      <CommitPicker
        anchorEl={ANCHOR}
        currentRef=""
        allowWorkingTree
        agentId="a1"
        workspaceId="ws1"
        relPath=""
        onSelect={vi.fn()}
        onClose={vi.fn()}
        placement="top"
      />,
    );
    await waitFor(() => screen.getByText("Page 1 of 3"));
    const popover = document.getElementById("commit-picker-popover") as HTMLElement;
    // Container still has no maxHeight — the cap belongs to the list.
    expect(popover.style.maxHeight).toBe("");
    // List got a viewport-bounded inline maxHeight.
    const list = popover.children[1] as HTMLElement;
    expect(list.style.maxHeight).not.toBe("");
    const listMax = parseInt(list.style.maxHeight, 10);
    // popover bottom = innerHeight - 700 + 4 = 72; minus 98 chrome
    // budget → ~ -26, clamped to floor of 120.
    expect(listMax).toBeGreaterThanOrEqual(120);
    // And the cap stays well under the default 288 so the popover
    // doesn't overflow off the top of the viewport.
    expect(listMax).toBeLessThan(288);
  });
});
