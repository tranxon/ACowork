// SPDX-License-Identifier: Apache-2.0
// Copyright the ACowork project contributors.
//
// Cross-workspace link/image resolver for Markdown previews.
//
// Background:
//   Markdown files can carry relative hrefs/src that escape the file's own
//   workspace — e.g. `docs/zh/protocols/http.md` lives under one workspace
//   while its source-code link targets a different one. The naive
//   single-workspace resolver used to drop such links on the floor and the
//   webview's default navigation took over, leading to a navigation that
//   Tauri's `recover_from_wake` path could mistake for a system wake.
//
// What this module does:
//   1. `resolveLocalAssetPath` — pure path arithmetic against one workspace
//      root (reused by both link and image rendering).
//   2. `resolveAssetAcrossWorkspaces` — synchronous candidate list ordered
//      by priority (current workspace → `__agent_home__` → others).
//   3. `openFirstResolved` — async HEAD-probe over the candidate list and
//      call `openFile`/`openPreview` on the first workspace that contains
//      the file. Other candidates are silently skipped on 404.
//   4. `notifyLinkNotFound` — user-visible toast when nothing matches.
//
// Splitting the resolver from the React component keeps the click handler
// synchronous path arithmetic small and lets the React tree keep using the
// pure helper for image rendering without re-running the HEAD probe on
// every render.

import { useWorkspaceStore } from "../../stores/workspaceStore";
import { useFileEditorStore } from "../../stores/fileEditorStore";
import { getGatewayUrl } from "../../lib/config";
import { log } from "../../lib/logger";
import { showToast } from "../common/ToastProvider";
import i18n from "../../i18n";

/** URL schemes that should be passed through to the webview as-is. */
export const PASSTHROUGH_SCHEMES = /^(https?:|data:|asset:|blob:|mailto:|tel:)/i;

/** Absolute path prefixes (POSIX `/...` or Windows `C:\...` / `C:/...`). */
export const ABSOLUTE_PATH = /^([/\\]|[A-Za-z]:[/\\])/;

/**
 * Resolve a markdown image/link `src` (relative or rooted) against one
 * workspace root and the directory of the markdown file. Handles `./` and
 * `../` segments. Returns a forward-slash separated absolute path suitable
 * for Tauri's `convertFileSrc` / `asset://` protocol, or `null` if `src`
 * is a non-file URL scheme or could not be resolved.
 *
 * Mirrors the legacy implementation that lived inside
 * `MarkdownPreviewView.tsx`; kept verbatim so existing callers and tests
 * still work.
 */
export function resolveLocalAssetPath(
    workspaceRoot: string,
    fileRelPath: string,
    src: string,
): string | null {
    if (!src || PASSTHROUGH_SCHEMES.test(src)) return null;
    if (ABSOLUTE_PATH.test(src)) return src;

    // Directory of the markdown file (its own relPath).
    const lastSep = Math.max(fileRelPath.lastIndexOf("/"), fileRelPath.lastIndexOf("\\"));
    const fileDir = lastSep >= 0 ? fileRelPath.substring(0, lastSep) : "";

    // Walk path segments, applying `./` and `../` against (workspaceRoot + fileDir).
    const baseParts = workspaceRoot.split(/[/\\]/).filter(Boolean);
    const fileDirParts = fileDir ? fileDir.split(/[/\\]/).filter(Boolean) : [];
    const srcParts = src.split(/[/\\]/);

    const result: string[] = [...baseParts];
    for (const part of [...fileDirParts, ...srcParts]) {
        if (part === "" || part === ".") continue;
        if (part === "..") {
            result.pop();
        } else {
            result.push(part);
        }
    }
    const joined = result.join("/");
    if (!joined) return null;

    // Re-attach the POSIX leading "/" if the workspace root was an
    // absolute path (macOS/Linux: starts with "/"). Without this,
    // split("/").filter(Boolean) above drops the empty first segment,
    // turning "/Users/foo/..." into "Users/foo/..." — a bare relative
    // path that Tauri's asset:// protocol cannot resolve (404).
    // Windows drive letters ("C:") survive filter(Boolean) intact, so
    // they don't need this treatment.
    if (workspaceRoot.startsWith("/") && !joined.startsWith("/")) {
        return `/${joined}`;
    }
    return joined;
}

/**
 * One candidate resolution of a markdown src — the absolute path (for
 * `convertFileSrc` rendering) plus the workspace-relative path (for
 * `openFile`/`openPreview`).
 */
export interface ResolvedAsset {
    workspaceId: string;
    /** Absolute filesystem path. Suitable for `convertFileSrc`. */
    absPath: string;
    /** Workspace-relative path. Suitable for `openFile(workspaceId, relPath)`. */
    relPath: string;
}

/**
 * Build the priority-ordered candidate list for resolving a markdown src
 * against all workspaces registered for the agent.
 *
 * Priority order:
 *   1. The current markdown file's workspace (`file.workspaceId`).
 *   2. `__agent_home__` (agent home directory).
 *   3. All other workspaces in the order returned by the store.
 *
 * Workspace root lookup prefers `treeRoots[agentId:workspaceId]` (the path
 * returned by the workspace tree API for an already-loaded workspace) and
 * falls back to `workspaces[].path` (the canonical root returned by the
 * workspaces list API). Both should normally agree; the fallback covers
 * the case where the tree API has not yet been called.
 *
 * Returns an empty list for non-file schemes (http, data, mailto, tel,
 * asset, blob) and pure in-page anchors (`#…`). Returns a possibly-empty
 * list when the workspace list has not loaded yet — the caller should
 * decide whether to fall back to webview navigation in that case.
 */
export function resolveAssetAcrossWorkspaces(
    agentId: string,
    currentWorkspaceId: string,
    fileRelPath: string,
    src: string,
): ResolvedAsset[] {
    if (!src) return [];
    if (PASSTHROUGH_SCHEMES.test(src)) return [];
    if (src.startsWith("#")) return [];

    const { workspaces, treeRoots } = useWorkspaceStore.getState();

    const rootFor = (workspaceId: string): string | null => {
        const fromTree = treeRoots[`${agentId}:${workspaceId}`];
        if (fromTree) return fromTree;
        const ws = workspaces.find((w) => w.id === workspaceId);
        return ws?.path ?? null;
    };

    // Build priority-ordered (workspaceId, absRoot) list, deduplicated.
    const seen = new Set<string>();
    const ordered: Array<{ workspaceId: string; absRoot: string }> = [];
    const tryAdd = (workspaceId: string) => {
        if (!workspaceId || seen.has(workspaceId)) return;
        const absRoot = rootFor(workspaceId);
        if (!absRoot) return;
        seen.add(workspaceId);
        ordered.push({ workspaceId, absRoot });
    };
    tryAdd(currentWorkspaceId);
    tryAdd("__agent_home__");
    for (const ws of workspaces) tryAdd(ws.id);

    const out: ResolvedAsset[] = [];
    for (const { workspaceId, absRoot } of ordered) {
        const abs = resolveLocalAssetPath(absRoot, fileRelPath, src);
        if (!abs) continue;
        const rel = absPathToRelPath(absRoot, abs);
        if (rel === null) continue; // Absolute path fell outside this workspace root.
        out.push({ workspaceId, absPath: abs, relPath: rel });
    }
    return out;
}

/**
 * Convert an absolute path produced by `resolveLocalAssetPath` back to a
 * workspace-relative path suitable for `openFile`/`openPreview`.
 *
 * Returns `""` if the absolute path equals the workspace root, the
 * workspace-relative path with forward slashes if it lives under the root,
 * or `null` if the path is outside the workspace root (caller should
 * skip this candidate).
 */
function absPathToRelPath(workspaceRoot: string, absPath: string): string | null {
    const normRoot = workspaceRoot.replace(/\\/g, "/").replace(/\/+$/, "");
    const normAbs = absPath.replace(/\\/g, "/");
    if (normAbs === normRoot) return "";
    if (!normAbs.startsWith(normRoot + "/")) return null;
    return normAbs.slice(normRoot.length + 1);
}

/**
 * Build the Gateway URL for a workspace file read.
 *
 * Mirrors `buildFileUrl` inside `fileEditorStore.ts` but lives here so
 * this module has no private coupling to that store.
 */
function buildFileUrl(agentId: string, workspaceId: string, relPath: string): string {
    const baseUrl = getGatewayUrl();
    const params = new URLSearchParams();
    if (workspaceId && workspaceId !== "__agent_home__") {
        params.set("workspace_id", workspaceId);
    }
    params.set("path", relPath);
    return `${baseUrl}/api/agents/${agentId}/workspaces/file?${params.toString()}`;
}

/**
 * Send a lightweight HEAD probe to the Gateway to confirm the file exists
 * in the given workspace. We use HEAD rather than GET so the resolver
 * doesn't download the file body — the subsequent `openFile`/`openPreview`
 * will do the full GET.
 *
 * Axum auto-handles HEAD for any route registered with `get`, so the
 * existing `/workspaces/file` GET handler responds to HEAD with headers
 * (status + size) and no body.
 *
 * Returns `true` for any 2xx status, `false` for 404, 403, 5xx, and
 * network errors. The caller should treat 5xx as "exists, transient
 * failure" and proceed optimistically if desired, but here we err on the
 * side of falling through to the next candidate so users don't see
 * half-broken tabs.
 */
async function fileExists(agentId: string, workspaceId: string, relPath: string): Promise<boolean> {
    try {
        const url = buildFileUrl(agentId, workspaceId, relPath);
        const resp = await fetch(url, { method: "HEAD" });
        return resp.ok;
    } catch (e) {
        log.warn("[MarkdownLinkResolver] HEAD probe failed:", { workspaceId, relPath, err: String(e) });
        return false;
    }
}

/**
 * Outcome of an `openFirstResolved` attempt.
 *
 * - `opened: true` — one of the candidates exists and has been opened as
 *   a tab via `openFile`/`openPreview`. `workspaceId` tells the caller
 *   which workspace was selected (useful for telemetry / toast).
 * - `opened: false` — every candidate either resolved to a non-existent
 *   file or `candidates` was empty. Caller should surface a "not found"
 *   toast or otherwise notify the user.
 */
export type OpenResolutionResult =
    | { opened: true; workspaceId: string }
    | { opened: false; tried: number };

/**
 * Walk the candidate list in priority order, HEAD-probe each candidate,
 * and open the first one that exists. All 404s fall through silently.
 *
 * `action` chooses between edit mode (`openFile`) and read-only preview
 * mode (`openPreview`). Markdown links to `.md`/`.markdown` open in
 * preview mode; everything else opens in edit mode — this matches the
 * convention already used by the file tab context menu.
 *
 * Concurrent calls are intentionally allowed: each call HEAD-probes
 * independently and the `openFile`/`openPreview` actions are idempotent
 * (they just activate the existing tab if one is already open for the
 * same `agentId:workspaceId:relPath`).
 */
export async function openFirstResolved(
    agentId: string,
    candidates: ResolvedAsset[],
    action: "openFile" | "openPreview",
): Promise<OpenResolutionResult> {
    if (candidates.length === 0) return { opened: false, tried: 0 };

    const store = useFileEditorStore.getState();
    for (const c of candidates) {
        const exists = await fileExists(agentId, c.workspaceId, c.relPath);
        if (!exists) continue;
        if (action === "openFile") {
            await store.openFile(agentId, c.workspaceId, c.relPath);
        } else {
            await store.openPreview(agentId, c.workspaceId, c.relPath);
        }
        return { opened: true, workspaceId: c.workspaceId };
    }
    return { opened: false, tried: candidates.length };
}

/**
 * Fire a toast informing the user that the clicked link/image target
 * could not be found in any of the agent's workspaces.
 */
export function notifyLinkNotFound(href: string): void {
    showToast({
        type: "warning",
        message: i18n.t("fileEditor.markdownLinkNotFound", { href }),
    });
}