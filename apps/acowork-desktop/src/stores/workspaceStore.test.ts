/**
 * Workspace store tests.
 *
 * Regression coverage for setPromptFile: the Runtime responds with
 * `{ ok, ws_id }`, NOT a full WorkspaceDir. The store used to replace the
 * local entry with that response, corrupting the workspaces list
 * (id/path/access became undefined) — the same envelope-as-entity bug
 * class as the user profile PUT.
 */

import { describe, it, expect, vi, afterEach } from "vitest";
import { useWorkspaceStore } from "./workspaceStore";
import type { WorkspaceDir } from "./workspaceStore";

function makeWorkspace(overrides: Partial<WorkspaceDir> = {}): WorkspaceDir {
    return {
        id: "ws-1",
        path: "/tmp/ws",
        alias: null,
        access: "read-write",
        added_at: "2026-08-01T00:00:00Z",
        select_count: 0,
        last_selected_at: null,
        prompt_file: null,
        ...overrides,
    };
}

afterEach(() => {
    vi.unstubAllGlobals();
});

describe("workspaceStore.setPromptFile", () => {
    it("keeps the local entry intact and patches prompt_file from the sent value", async () => {
        // Runtime echoes { ok, ws_id } — NOT a WorkspaceDir.
        vi.stubGlobal(
            "fetch",
            vi.fn(() =>
                Promise.resolve({
                    ok: true,
                    status: 200,
                    json: () => Promise.resolve({ ok: true, ws_id: "ws-1" }),
                } as Response),
            ),
        );

        useWorkspaceStore.setState({ workspaces: [makeWorkspace()] });

        const result = await useWorkspaceStore.getState().setPromptFile("agent-1", "ws-1", "CLAUDE.md");

        expect(result).toBe(true);
        const ws = useWorkspaceStore.getState().workspaces[0];
        // Regression: the old code replaced the entry with { ok, ws_id },
        // leaving id/path/access undefined and breaking tree/file ops.
        expect(ws.id).toBe("ws-1");
        expect(ws.path).toBe("/tmp/ws");
        expect(ws.access).toBe("read-write");
        expect(ws.prompt_file).toBe("CLAUDE.md");
    });

    it("clears prompt_file when null is passed (toggle off)", async () => {
        vi.stubGlobal(
            "fetch",
            vi.fn(() =>
                Promise.resolve({
                    ok: true,
                    status: 200,
                    json: () => Promise.resolve({ ok: true, ws_id: "ws-1" }),
                } as Response),
            ),
        );

        useWorkspaceStore.setState({
            workspaces: [makeWorkspace({ prompt_file: "AGENTS.md" })],
        });

        await useWorkspaceStore.getState().setPromptFile("agent-1", "ws-1", null);

        const ws = useWorkspaceStore.getState().workspaces[0];
        expect(ws.prompt_file).toBeNull();
        expect(ws.id).toBe("ws-1");
    });

    it("returns false and keeps state on HTTP error", async () => {
        vi.stubGlobal(
            "fetch",
            vi.fn(() =>
                Promise.resolve({
                    ok: false,
                    status: 500,
                    statusText: "Internal Server Error",
                    text: () => Promise.resolve("boom"),
                } as Response),
            ),
        );

        useWorkspaceStore.setState({ workspaces: [makeWorkspace()] });

        const result = await useWorkspaceStore.getState().setPromptFile("agent-1", "ws-1", "CLAUDE.md");

        expect(result).toBe(false);
        expect(useWorkspaceStore.getState().workspaces[0].prompt_file).toBeNull();
    });
});
