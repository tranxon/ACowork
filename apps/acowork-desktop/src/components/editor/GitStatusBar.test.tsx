/**
 * ADR-078 frontend tests — GitStatusBar.
 *
 * Collapsible version-control strip. Covers:
 *   1. Title derivation: default (pre-load), `branch · N changes`, notRepo,
 *      and branch-less (`N changes`) states.
 *   2. Loading affordance (Loader2) vs. manual-refresh (RefreshCw).
 *   3. Expand/collapse toggle → setExpanded with the correct flip.
 *   4. aria-expanded reflects state.
 */

import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";

// ── Mocks ────────────────────────────────────────────────────────────────

const translations: Record<string, string> = {
  "gitStatusBar.title": "Git",
  "gitStatusBar.notRepo": "Not a Git repository",
  "gitStatusBar.changes": "changes",
};

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({
    t: (key: string) => translations[key] ?? key,
  }),
}));

/** Fake gitStore state fed through the selector pattern the component uses. */
const mocks = {
  isExpanded: vi.fn(() => false),
  status: {} as Record<string, unknown>,
  setExpanded: vi.fn(),
  refresh: vi.fn(),
};

vi.mock("../../stores/gitStore", () => ({
  gitGroupKey: (a: string, w: string) => `${a}\u0000${w}`,
  useGitStore: (selector: (s: Record<string, unknown>) => unknown) =>
    selector(mocks as unknown as Record<string, unknown>),
}));

// ── SUT ──────────────────────────────────────────────────────────────────

import { GitStatusBar } from "./GitStatusBar";

const KEY = "a1\u0000ws1";

function setEntry(entry: Record<string, unknown> | undefined) {
  mocks.status[KEY] = entry;
}

beforeEach(() => {
  mocks.isExpanded.mockReset().mockReturnValue(false);
  mocks.setExpanded.mockReset();
  mocks.refresh.mockReset();
  setEntry(undefined);
});

describe("GitStatusBar", () => {
  it("shows the default title before data loads", () => {
    render(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    expect(screen.getByText("Git")).toBeTruthy();
    expect(screen.getByTestId("git-status-bar").getAttribute("aria-label")).toBe(
      "Git",
    );
  });

  it("shows `branch · N changes` when a repo with changes is loaded", () => {
    setEntry({
      data: {
        isRepo: true,
        branch: "main",
        error: null,
        truncated: false,
        changes: [{}, {}],
      },
      loading: false,
    });
    render(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    expect(screen.getByText("main · 2 changes")).toBeTruthy();
  });

  it("shows `N changes` when the repo has no branch name", () => {
    setEntry({
      data: { isRepo: true, branch: null, error: null, truncated: false, changes: [{}] },
      loading: false,
    });
    render(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    expect(screen.getByText("1 changes")).toBeTruthy();
  });

  it("shows the not-a-repo label when isRepo is false", () => {
    setEntry({
      data: { isRepo: false, branch: null, error: "not_a_repo", truncated: false, changes: [] },
      loading: false,
    });
    render(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    expect(screen.getByText("Not a Git repository")).toBeTruthy();
  });

  it("renders a spinner while loading (no refresh button)", () => {
    setEntry({ data: null, loading: true });
    const { container } = render(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    expect(container.querySelector(".animate-spin")).toBeTruthy();
    // No manual-refresh affordance during an in-flight fetch.
    expect(screen.queryByTestId("git-refresh")).toBeNull();
  });

  it("clicking the bar expands a collapsed group", () => {
    mocks.isExpanded.mockReturnValue(false);
    render(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    fireEvent.click(screen.getByTestId("git-status-bar"));
    expect(mocks.setExpanded).toHaveBeenCalledWith("a1", "ws1", true);
  });

  it("clicking the bar collapses an expanded group", () => {
    mocks.isExpanded.mockReturnValue(true);
    render(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    fireEvent.click(screen.getByTestId("git-status-bar"));
    expect(mocks.setExpanded).toHaveBeenCalledWith("a1", "ws1", false);
    expect(
      screen.getByTestId("git-status-bar").getAttribute("aria-expanded"),
    ).toBe("true");
  });

  it("manual refresh button triggers a status refresh without toggling", () => {
    setEntry({
      data: { isRepo: true, branch: "main", error: null, truncated: false, changes: [] },
      loading: false,
    });
    render(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    const bar = screen.getByTestId("git-status-bar");
    // RefreshCw renders inside the button; find it by class.
    const refreshIcon = bar.querySelector("svg.lucide-refresh-cw");
    expect(refreshIcon).toBeTruthy();
    fireEvent.click(refreshIcon!);
    expect(mocks.refresh).toHaveBeenCalledWith("a1", "ws1");
    // Toggle must NOT have fired (stopPropagation).
    expect(mocks.setExpanded).not.toHaveBeenCalled();
  });
});
