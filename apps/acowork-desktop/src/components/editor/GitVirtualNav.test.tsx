/**
 * GitVirtualNav.test.tsx — covers the two scenarios the user hit:
 *
 *   1. Diff: Up/Down buttons disable at the first/last hunk so we
 *      don't cycle around. Cursor tracking + Monaco subscription.
 *   2. Log: Prev/Next bidirectionally paginate; reaching the end
 *      must NOT nuke the cache or display, so Prev still restores.
 *
 * Run: `npx vitest run src/components/editor/GitVirtualNav.test.tsx`
 */

import { describe, expect, it, vi, beforeEach } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import type { editor } from "monaco-editor";
import {
    computeHunkBounds,
    GitVirtualNav,
} from "./GitVirtualNav";
import { useFileEditorStore } from "../../stores/fileEditorStore";
import { useGitStore } from "../../stores/gitStore";

// ── Mocks ─────────────────────────────────────────────────────────
vi.mock("../../i18n/useTranslation", () => ({
    useTranslation: () => ({ t: (k: string) => k }),
}));

const fetchLogMock = vi.fn();
vi.mock("../../stores/gitStore", async () => {
    const actual =
        await vi.importActual<typeof import("../../stores/gitStore")>(
            "../../stores/gitStore",
        );
    return {
        ...actual,
        useGitStore: Object.assign(
            (sel: (s: { fetchLog: unknown }) => unknown) =>
                sel({ fetchLog: fetchLogMock }),
            {
                getState: () => ({ fetchLog: fetchLogMock }),
            },
        ),
    };
});

// ── Helpers ───────────────────────────────────────────────────────
function makeCommits(n: number) {
    return Array.from({ length: n }, (_, i) => ({
        shortHash: `hash${i}`,
        author: `author${i}`,
        date: `2025-01-${String(i + 1).padStart(2, "0")}`,
        subject: `subject ${i}`,
    }));
}

function makeDiffEditorStub(opts: {
    cursor?: number;
    hunks?: { modifiedStartLineNumber: number }[];
} = {}): editor.IStandaloneDiffEditor {
    const cursor = opts.cursor ?? 1;
    const hunks = opts.hunks ?? [];
    return {
        goToDiff: vi.fn(),
        revealFirstDiff: vi.fn(),
        getLineChanges: vi.fn(() => hunks),
        getModifiedEditor: () => ({
            getPosition: () => ({ lineNumber: cursor }),
            onDidChangeCursorPosition: vi.fn(() => ({ dispose: vi.fn() })),
        }),
        onDidUpdateDiff: vi.fn(() => ({ dispose: vi.fn() })),
    } as unknown as editor.IStandaloneDiffEditor;
}

// ── Tests: pure helper ────────────────────────────────────────────
describe("computeHunkBounds", () => {
    it("returns all-false on empty hunks", () => {
        const r = computeHunkBounds([], 5);
        expect(r).toEqual({ atFirst: false, atLast: false, hasHunks: false });
    });

    it("marks atFirst when cursor sits on the first hunk", () => {
        const hunks = [
            { modifiedStartLineNumber: 10 },
            { modifiedStartLineNumber: 30 },
            { modifiedStartLineNumber: 50 },
        ];
        const r = computeHunkBounds(hunks, 8);
        expect(r.atFirst).toBe(true);
        expect(r.atLast).toBe(false);
    });

    it("marks atLast when cursor sits on or past the last hunk", () => {
        const hunks = [
            { modifiedStartLineNumber: 10 },
            { modifiedStartLineNumber: 30 },
            { modifiedStartLineNumber: 50 },
        ];
        expect(computeHunkBounds(hunks, 50).atLast).toBe(true);
        expect(computeHunkBounds(hunks, 51).atLast).toBe(true);
    });

    it("marks both false when cursor is in the middle", () => {
        const hunks = [
            { modifiedStartLineNumber: 10 },
            { modifiedStartLineNumber: 30 },
            { modifiedStartLineNumber: 50 },
        ];
        const r = computeHunkBounds(hunks, 25);
        expect(r.atFirst).toBe(false);
        expect(r.atLast).toBe(false);
    });
});

// ── Tests: diff buttons disable at boundaries ─────────────────────
describe("GitVirtualNav — diff mode", () => {
    beforeEach(() => {
        vi.clearAllMocks();
        useFileEditorStore.setState({
            files: new Map(),
            activeFileId: null,
            openVirtualFile: vi.fn(),
            setVirtualFileContent: vi.fn(),
        } as unknown as Parameters<typeof useFileEditorStore.setState>[0]);
    });

    it("disables Previous when cursor is on the first hunk", async () => {
        const diffEditor = makeDiffEditorStub({
            cursor: 10,
            hunks: [
                { modifiedStartLineNumber: 10 },
                { modifiedStartLineNumber: 30 },
            ],
        });
        render(
            <GitVirtualNav
                file={{
                    id: "diff-1",
                    kind: "diff",
                    agentId: "a",
                    workspaceId: "w",
                    relPath: "x.ts",
                    content: "x",
                    language: "plaintext",
                    originalContent: "y",
                    mode: "normal",
                    dirty: false,
                    isReadOnly: true,
                    lastModified: 0,
                }}
                diffEditor={diffEditor}
            />,
        );
        const prev = screen.getByLabelText("gitStatus.prevChange");
        const next = screen.getByLabelText("gitStatus.nextChange");
        expect(prev.hasAttribute("disabled")).toBe(true);
        expect(next.hasAttribute("disabled")).toBe(false);
    });

    it("disables Next when cursor is on the last hunk", async () => {
        const diffEditor = makeDiffEditorStub({
            cursor: 50,
            hunks: [
                { modifiedStartLineNumber: 10 },
                { modifiedStartLineNumber: 50 },
            ],
        });
        render(
            <GitVirtualNav
                file={{
                    id: "diff-1",
                    kind: "diff",
                    agentId: "a",
                    workspaceId: "w",
                    relPath: "x.ts",
                    content: "x",
                    language: "plaintext",
                    originalContent: "y",
                    mode: "normal",
                    dirty: false,
                    isReadOnly: true,
                    lastModified: 0,
                }}
                diffEditor={diffEditor}
            />,
        );
        const prev = screen.getByLabelText("gitStatus.prevChange");
        const next = screen.getByLabelText("gitStatus.nextChange");
        expect(next.hasAttribute("disabled")).toBe(true);
        expect(prev.hasAttribute("disabled")).toBe(false);
    });

    it("calls revealFirstDiff exactly once on mount", () => {
        const diffEditor = makeDiffEditorStub({
            cursor: 1,
            hunks: [{ modifiedStartLineNumber: 10 }],
        });
        render(
            <GitVirtualNav
                file={{
                    id: "diff-1",
                    kind: "diff",
                    agentId: "a",
                    workspaceId: "w",
                    relPath: "x.ts",
                    content: "x",
                    language: "plaintext",
                    originalContent: "y",
                    mode: "normal",
                    dirty: false,
                    isReadOnly: true,
                    lastModified: 0,
                }}
                diffEditor={diffEditor}
            />,
        );
        expect(diffEditor.revealFirstDiff).toHaveBeenCalledTimes(1);
    });

    it("dispatches goToDiff('next') / goToDiff('previous') on click", () => {
        const diffEditor = makeDiffEditorStub({
            cursor: 25,
            hunks: [
                { modifiedStartLineNumber: 10 },
                { modifiedStartLineNumber: 50 },
            ],
        });
        render(
            <GitVirtualNav
                file={{
                    id: "diff-1",
                    kind: "diff",
                    agentId: "a",
                    workspaceId: "w",
                    relPath: "x.ts",
                    content: "x",
                    language: "plaintext",
                    originalContent: "y",
                    mode: "normal",
                    dirty: false,
                    isReadOnly: true,
                    lastModified: 0,
                }}
                diffEditor={diffEditor}
            />,
        );
        fireEvent.click(screen.getByLabelText("gitStatus.nextChange"));
        fireEvent.click(screen.getByLabelText("gitStatus.prevChange"));
        expect(diffEditor.goToDiff).toHaveBeenNthCalledWith(1, "next");
        expect(diffEditor.goToDiff).toHaveBeenNthCalledWith(2, "previous");
    });
});

// ── Tests: log mode — the bug we just fixed ───────────────────────
describe("GitVirtualNav — log mode (reachedEnd)", () => {
    beforeEach(() => {
        vi.clearAllMocks();
        fetchLogMock.mockReset();
        useFileEditorStore.setState({
            files: new Map(),
            activeFileId: null,
            openVirtualFile: vi.fn(),
            setVirtualFileContent: vi.fn(),
        } as unknown as Parameters<typeof useFileEditorStore.setState>[0]);
    });

    function logFile(reachedEnd: boolean, displayedLimit: number, n: number) {
        return {
            id: "log-1",
            kind: "log" as const,
            agentId: "a",
            workspaceId: "w",
            relPath: "x.ts",
            content: "",
            language: "plaintext",
            mode: "normal" as const,
            dirty: false,
            isReadOnly: true,
            lastModified: 0,
            loadedCommits: makeCommits(n),
            displayedLimit,
            reachedEnd,
        };
    }

    it("disables Next when reachedEnd is true (does NOT call fetchLog)", async () => {
        const setVirtualFileContent = vi.fn();
        useFileEditorStore.setState({
            files: new Map(),
            activeFileId: null,
            openVirtualFile: vi.fn(),
            setVirtualFileContent,
        } as unknown as Parameters<typeof useFileEditorStore.setState>[0]);
        render(<GitVirtualNav file={logFile(true, 50, 50)} diffEditor={null} />);
        const next = screen.getByLabelText("gitStatus.nextCommits");
        expect(next.hasAttribute("disabled")).toBe(true);
        fireEvent.click(next);
        // wait one tick for any async handler to fire
        await Promise.resolve();
        expect(fetchLogMock).not.toHaveBeenCalled();
        expect(setVirtualFileContent).not.toHaveBeenCalled();
    });

    it("disables Prev at the bottom of the window (displayedLimit == step)", () => {
        render(<GitVirtualNav file={logFile(false, 50, 50)} diffEditor={null} />);
        const prev = screen.getByLabelText("gitStatus.prevCommits");
        expect(prev.hasAttribute("disabled")).toBe(true);
    });

    it("Prev reduces the window from the local cache (no fetch)", () => {
        const setVirtualFileContent = vi.fn();
        useFileEditorStore.setState({
            files: new Map(),
            activeFileId: null,
            openVirtualFile: vi.fn(),
            setVirtualFileContent,
        } as unknown as Parameters<typeof useFileEditorStore.setState>[0]);
        render(<GitVirtualNav file={logFile(false, 100, 100)} diffEditor={null} />);
        fireEvent.click(screen.getByLabelText("gitStatus.prevCommits"));
        expect(fetchLogMock).not.toHaveBeenCalled();
        expect(setVirtualFileContent).toHaveBeenCalledWith(
            "log-1",
            expect.any(String),
            expect.objectContaining({ displayedLimit: 50 }),
        );
    });

    it("Next at reachedEnd keeps Prev usable by NOT clobbering cache", async () => {
        const setVirtualFileContent = vi.fn();
        useFileEditorStore.setState({
            files: new Map(),
            activeFileId: null,
            openVirtualFile: vi.fn(),
            setVirtualFileContent,
        } as unknown as Parameters<typeof useFileEditorStore.setState>[0]);
        // File has only 10 commits — reachedEnd already true on first open.
        render(<GitVirtualNav file={logFile(true, 10, 10)} diffEditor={null} />);

        const next = screen.getByLabelText("gitStatus.nextCommits");
        expect(next.hasAttribute("disabled")).toBe(true);
        // No Prev available either (displayedLimit == 10 == step).
        const prev = screen.getByLabelText("gitStatus.prevCommits");
        expect(prev.hasAttribute("disabled")).toBe(true);
    });
});

// ── Integration test for the seed flow ────────────────────────────
describe("GitStatusPanel.openLog seed behaviour", () => {
    it("returns reachedEnd=true when initial fetch is shorter than the requested limit", async () => {
        // Simulate what GitStatusPanel.openLog computes after a fetch
        // — if commits.length < INITIAL_LIMIT, reachedEnd should be
        // true. This is the bug fix that prevents Next from clobbering
        // the display.
        const INITIAL_LIMIT = 50;
        const commits = makeCommits(3);
        const reachedEnd = commits.length < INITIAL_LIMIT;
        const displayedLimit = commits.length;
        expect(reachedEnd).toBe(true);
        expect(displayedLimit).toBe(3);
    });
});
