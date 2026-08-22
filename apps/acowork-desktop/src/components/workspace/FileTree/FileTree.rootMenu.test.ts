/**
 * Tests for the workspace-root context menu (right-click on the empty
 * area below the rows in the file tree).
 *
 * The empty area is the part of the scroller that is not occupied by any
 * row (padding, scrollbar gutter, below-the-last-row space). UX-wise the
 * user expects this to behave like right-clicking the workspace root, so
 * the same three actions that work at the root should be exposed:
 *
 *   - New File     → onContextNewItem("file", "")
 *   - New Folder   → onContextNewItem("dir", "")
 *   - Paste        → onPaste("")  (disabled when clipboard is empty)
 *
 * Operations that target a single entry (Copy / Rename / Delete / Reveal
 * / Add to Chat / Preview / Toggle Prompt File) are deliberately absent.
 *
 * The menu-item builder is a pure function extracted from `FileTree`
 * (`buildRootContextMenuItems`) so the test can drive it without spinning
 * up the full virtualised tree / i18n / Zustand store stack.
 */

import { describe, it, expect, vi } from "vitest";
import {
    buildRootContextMenuItems,
    type RootContextCopiedEntry,
} from "./FileTree";

function makeT() {
    // Minimal i18n stand-in — keys are echoed so we can assert the right
    // lookup was performed without coupling to any specific locale.
    return (key: string): string => `[t]${key}`;
}

function makeCopiedEntry(overrides: Partial<RootContextCopiedEntry> = {}): RootContextCopiedEntry {
    return {
        agentId: "com.acowork.test",
        workspaceId: "__agent_home__",
        path: "README.md",
        type: "file",
        ...overrides,
    };
}

describe("buildRootContextMenuItems", () => {
    it("returns exactly three items: New File / New Folder / Paste", () => {
        const items = buildRootContextMenuItems(makeT(), {});
        expect(items.map((i) => i.key)).toEqual(["new-file", "new-folder", "paste"]);
    });

    it("does NOT expose single-entry ops (copy / delete / rename / reveal)", () => {
        // Documented invariant: the empty-area menu is intentionally
        // a strict subset of the per-row menu. Single-entry operations
        // have no meaningful target here, so omitting them avoids the
        // confusion of "Delete what?".
        const items = buildRootContextMenuItems(makeT(), {});
        const keys = items.map((i) => i.key);
        expect(keys).not.toContain("copy");
        expect(keys).not.toContain("delete");
        expect(keys).not.toContain("rename");
        expect(keys).not.toContain("reveal");
        expect(keys).not.toContain("add-to-chat");
        expect(keys).not.toContain("preview");
    });

    it("Paste is disabled when copiedEntry is null", () => {
        const items = buildRootContextMenuItems(makeT(), { copiedEntry: null });
        const paste = items.find((i) => i.key === "paste");
        expect(paste?.disabled).toBe(true);
    });

    it("Paste is disabled when copiedEntry is omitted", () => {
        const items = buildRootContextMenuItems(makeT(), {});
        const paste = items.find((i) => i.key === "paste");
        expect(paste?.disabled).toBe(true);
    });

    it("Paste is enabled when copiedEntry is present", () => {
        const items = buildRootContextMenuItems(makeT(), {
            copiedEntry: makeCopiedEntry(),
        });
        const paste = items.find((i) => i.key === "paste");
        expect(paste?.disabled).toBe(false);
    });

    it("New File click invokes onContextNewItem('file', '')", () => {
        const onContextNewItem = vi.fn();
        const items = buildRootContextMenuItems(makeT(), { onContextNewItem });
        const newFile = items.find((i) => i.key === "new-file");
        expect(newFile).toBeDefined();
        // ContextMenuItem.onClick receives a context object; we pass {} here.
        newFile!.onClick({
            event: {} as never,
            payload: undefined,
            selectionAtOpen: "",
        });
        expect(onContextNewItem).toHaveBeenCalledTimes(1);
        expect(onContextNewItem).toHaveBeenCalledWith("file", "");
    });

    it("New Folder click invokes onContextNewItem('dir', '')", () => {
        const onContextNewItem = vi.fn();
        const items = buildRootContextMenuItems(makeT(), { onContextNewItem });
        const newFolder = items.find((i) => i.key === "new-folder");
        expect(newFolder).toBeDefined();
        newFolder!.onClick({
            event: {} as never,
            payload: undefined,
            selectionAtOpen: "",
        });
        expect(onContextNewItem).toHaveBeenCalledTimes(1);
        expect(onContextNewItem).toHaveBeenCalledWith("dir", "");
    });

    it("Paste click invokes onPaste('') when enabled", () => {
        const onPaste = vi.fn();
        const items = buildRootContextMenuItems(makeT(), {
            onPaste,
            copiedEntry: makeCopiedEntry(),
        });
        const paste = items.find((i) => i.key === "paste");
        expect(paste?.disabled).toBe(false);
        paste!.onClick({
            event: {} as never,
            payload: undefined,
            selectionAtOpen: "",
        });
        expect(onPaste).toHaveBeenCalledTimes(1);
        expect(onPaste).toHaveBeenCalledWith("");
    });

    it("Paste click is a no-op when onPaste is omitted (no throw)", () => {
        // Even though the item is disabled when copiedEntry is null,
        // the onClick should be safe to call when the parent did not
        // supply a paste handler — the optional-chaining makes the call
        // a no-op rather than a crash.
        const items = buildRootContextMenuItems(makeT(), {
            copiedEntry: makeCopiedEntry(),
        });
        const paste = items.find((i) => i.key === "paste");
        expect(() =>
            paste!.onClick({
                event: {} as never,
                payload: undefined,
                selectionAtOpen: "",
            }),
        ).not.toThrow();
    });

    it("New File click is a no-op when onContextNewItem is omitted", () => {
        const items = buildRootContextMenuItems(makeT(), {});
        const newFile = items.find((i) => i.key === "new-file");
        expect(() =>
            newFile!.onClick({
                event: {} as never,
                payload: undefined,
                selectionAtOpen: "",
            }),
        ).not.toThrow();
    });

    it("uses the provided t() for labels", () => {
        const t = makeT();
        const items = buildRootContextMenuItems(t, {});
        // Each label must round-trip through t() — guards against
        // future refactors that hard-code English strings.
        expect(items.find((i) => i.key === "new-file")?.label).toBe(
            "[t]workspace.contextMenu.newFile",
        );
        expect(items.find((i) => i.key === "new-folder")?.label).toBe(
            "[t]workspace.contextMenu.newFolder",
        );
        expect(items.find((i) => i.key === "paste")?.label).toBe(
            "[t]workspace.contextMenu.paste",
        );
    });

    it("Paste item has a divider before it (visual separation)", () => {
        // The divider-before mirrors the FileTreeNode layout, where
        // New File / New Folder form one group and Paste / Copy / etc.
        // are below a divider. Pinning the boolean so the visual contract
        // does not silently regress.
        const items = buildRootContextMenuItems(makeT(), {
            copiedEntry: makeCopiedEntry(),
        });
        const paste = items.find((i) => i.key === "paste");
        expect(paste?.dividerBefore).toBe(true);
    });
});
