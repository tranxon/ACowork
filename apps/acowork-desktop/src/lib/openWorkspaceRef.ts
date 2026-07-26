// SPDX-License-Identifier: Apache-2.0
// Copyright the ACowork project contributors.
//
// Single dispatch entry point for "open this workspace attachment in the
// in-app fileTab" — used by the chat-bubble and input-bar chips created
// from "Add to Chat" references.
//
// Why this exists:
//   - `MessageBubble.tsx` and `AttachedContextChips.tsx` both render three
//     workspace-ref variants (`attached_file` / `attached_selection` /
//     `attached_folder`) as clickable chips. Their click handlers should
//     converge on **one** piece of logic that knows how to map an absolute
//     filesystem path to a `(workspaceId, relPath)` pair, pick the right
//     fileTab action, and report failures uniformly.
//   - `markdownLinkResolver.ts` already owns the path → workspace mapping
//     (`resolveAssetAcrossWorkspaces` + `openFirstResolved`) and the
//     "not found" toast (`notifyLinkNotFound`). We reuse those — no
//     parallel machinery, no duplicated HEAD-probe code.
//
// Three entry shapes are accepted so callers don't have to normalise first:
//
//   | ChatStore `attachedContext` shape (`type: "file" | "directory" | "selection"`)
//   | Wire `AttachedItem` shape     (`type: "attached_file" | "attached_folder" | "attached_selection"`)
//
// `file_upload` / `image_upload` are **not** accepted here on purpose:
// ADR-046 §2.5 says uploaded files are not clickable workspace refs (the
// runtime owns the body; the UI just shows provenance).

import {
    notifyLinkNotFound,
    openFirstResolved,
    resolveAssetAcrossWorkspaces,
} from "../components/editor/markdownLinkResolver";
import { log } from "./logger";
import type { AttachedItem } from "./types";

/** Chat-store "pending attachment" shape — the pre-send form of a workspace
 *  ref. `startLine`/`endLine` are 1-based and inclusive for selection. */
export interface AttachedContextLike {
    type: "file" | "directory" | "selection";
    absPath: string;
    name: string;
    startLine?: number;
    endLine?: number;
}

/** Union of every workspace-ref shape this dispatcher accepts. */
export type WorkspaceRefItem = AttachedItem | AttachedContextLike;

interface OpenAttachedRefArgs {
    /** The chip item. */
    item: WorkspaceRefItem;
    /** Selected agent ID. The dispatch is a no-op when this is falsy. */
    agentId: string | null;
    /** Workspace ID for the current session — used as the highest-priority
     *  candidate root in `resolveAssetAcrossWorkspaces`. */
    currentWorkspaceId: string;
}

/**
 * Open a workspace attachment in the in-app fileTab.
 *
 * Behaviour by `item.type`:
 *   - `file` / `attached_file`         → `openPreview` (read-only).
 *   - `selection` / `attached_selection` → `openFile(..., startLine)`. The
 *       cursor jumps to `startLine`; multi-line selection highlight is
 *       intentionally **not** implemented yet (deferred — needs
 *       `selectionRange` on `fileEditorStore`).
 *   - `directory` / `attached_folder`  → silent no-op (fileTab doesn't open
 *       folders yet; waiting for folder-preview support).
 *
 * Returns a `Promise<void>` so callers can `await` and surface errors if
 * desired; current behaviour swallows all errors and emits a toast on
 * "not found in any workspace" via `notifyLinkNotFound`.
 */
export async function openAttachedRef({
    item,
    agentId,
    currentWorkspaceId,
}: OpenAttachedRefArgs): Promise<void> {
    if (!agentId) return;

    // ── Branch on the user's "what does a click mean" intent ──────────
    switch (item.type) {
        case "file":
        case "attached_file":
            await openAsPreview(item.absPath, agentId, currentWorkspaceId);
            return;
        case "selection":
        case "attached_selection": {
            // 1-based, inclusive. Fall back to 1 if upstream forgot to set them.
            const startLine = item.startLine ?? 1;
            await openAtLine(item.absPath, agentId, currentWorkspaceId, startLine);
            return;
        }
        case "directory":
        case "attached_folder":
            // fileTab doesn't support folder preview yet — silent no-op per
            // product direction. (Future: navigate the workspace tree to
            // the folder instead of opening a tab.)
            return;
        case "file_upload":
        case "image_upload":
            // ADR-046: uploads are not clickable workspace refs. Defensive
            // branch — callers shouldn't reach here.
            log.warn("[openAttachedRef] upload items are not clickable:", item);
            return;
        default: {
            // Exhaustiveness guard.
            const _exhaustive: never = item;
            log.warn("[openAttachedRef] unknown item type:", _exhaustive);
            return;
        }
    }
}

// ── Internals ─────────────────────────────────────────────────────────

async function openAsPreview(
    absPath: string,
    agentId: string,
    currentWorkspaceId: string,
): Promise<void> {
    const candidates = resolveAssetAcrossWorkspaces(agentId, currentWorkspaceId, "", absPath);
    if (candidates.length === 0) {
        notifyLinkNotFound(absPath);
        return;
    }
    const result = await openFirstResolved(agentId, candidates, "openPreview");
    if (!result.opened) {
        notifyLinkNotFound(absPath);
    }
}

async function openAtLine(
    absPath: string,
    agentId: string,
    currentWorkspaceId: string,
    startLine: number,
): Promise<void> {
    const candidates = resolveAssetAcrossWorkspaces(agentId, currentWorkspaceId, "", absPath);
    if (candidates.length === 0) {
        notifyLinkNotFound(absPath);
        return;
    }
    const result = await openFirstResolved(agentId, candidates, "openFile", startLine);
    if (!result.opened) {
        notifyLinkNotFound(absPath);
    }
}