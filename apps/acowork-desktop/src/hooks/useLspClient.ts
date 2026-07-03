/**
 * Re-exports from the shared LSP utility module.
 *
 * The original `useLspClient` single-language hook has been removed (it was
 * unused).  All shared utilities now live in `src/lib/lspUtils.ts`, and the
 * connection lifecycle is managed by `LspConnection` (`src/lib/lspConnection.ts`).
 */

export {
    type LspStatus,
    adaptWebSocket,
    buildAbsoluteUri,
    buildLspWsUrl,
    ensureVscodeApiInitialized,
} from "../lib/lspUtils";
