/**
 * Language-aware project root discovery for LSP.
 *
 * In a multi-language monorepo (e.g. Rust + TypeScript), the workspace root
 * does not contain language-specific project files (tsconfig.json, Cargo.toml,
 * etc.). Language servers need the correct project root as their `rootUri` to
 * pick up the right configuration (e.g. `moduleResolution: "bundler"` for
 * TypeScript).
 *
 * Discovery is delegated to the LSP Relay's HTTP API, which mirrors the
 * Rust implementation in `core/acowork-lsp-relay/src/project_root.rs`.
 * The relay reads `root_markers` from `lsp_servers.json` — no markers
 * need to be maintained on the frontend.
 */

import { getLspRelayUrl } from "./gateway-api";

/**
 * Discover the language-specific project root for a given file.
 *
 * Delegates to the LSP Relay's `POST /api/project-root/discover` endpoint.
 * The relay walks up from the file's directory to the workspace root,
 * checking for language-specific marker files (tsconfig.json, Cargo.toml,
 * etc.). The first directory containing a marker file is returned as the
 * project root. If no marker is found, falls back to the workspace root.
 *
 * @param filePath    Absolute path of the file being opened
 * @param language    Language id (e.g. "typescript", "rust")
 * @param workspaceRoot  Monorepo root (upper bound for the search)
 * @returns Project root directory (absolute path)
 */
export async function discoverProjectRoot(
    filePath: string,
    language: string,
    workspaceRoot: string,
): Promise<string> {
    const relayUrl = await getLspRelayUrl();
    if (!relayUrl) {
        console.warn("[LSP] project root discovery unavailable — relay not reachable, using workspace root");
        return workspaceRoot;
    }

    try {
        const resp = await fetch(`${relayUrl}/api/project-root/discover`, {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({
                file_path: filePath,
                language,
                workspace_root: workspaceRoot,
            }),
        });

        if (!resp.ok) {
            console.warn(`[LSP] project root discovery returned ${resp.status}, fallback to workspace root`);
            return workspaceRoot;
        }

        const data = await resp.json();
        return data.project_root;
    } catch (err) {
        console.warn("[LSP] project root discovery failed —", err, ", fallback to workspace root");
        return workspaceRoot;
    }
}
