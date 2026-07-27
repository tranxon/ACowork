import { useState, useEffect } from "react";
import { AlertTriangle, Loader2 } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { transformHtmlPreview } from "./htmlPreviewTransform";

interface HtmlPreviewViewProps {
    /** Raw HTML content fetched via the Gateway JSON API */
    content: string;
    /** Gateway base URL (e.g. "http://localhost:19876") */
    gatewayUrl: string;
    /** Agent ID for constructing ws-files URLs */
    agentId: string;
    /** Workspace ID for resolving preview sub-resources */
    workspaceId: string;
    /** Workspace-relative path of the HTML file being previewed */
    relPath: string;
    fileName: string;
}

/**
 * Renders an HTML string in a sandboxed iframe via a Blob URL.
 *
 * Injects a `<base>` tag pointing to `{gatewayUrl}/ws-files/{agentId}/`
 * so root-relative paths (e.g. `/src/main.tsx`) resolve to the Gateway's
 * workspace file server, where sub-resources are served with correct MIME types.
 *
 * Blob lifecycle is managed via useState + useEffect (not useMemo) to avoid
 * race conditions where the URL is revoked before the iframe loads it.
 */
export function HtmlPreviewView({ content, gatewayUrl, agentId, workspaceId, relPath, fileName }: HtmlPreviewViewProps) {
    const { t } = useTranslation();
    const [loading, setLoading] = useState(true);
    const [blobUrl, setBlobUrl] = useState<string | null>(null);
    const [hasTsModuleEntry, setHasTsModuleEntry] = useState(false);

    useEffect(() => {
        if (!content) return;

        setLoading(true);

        const transformed = transformHtmlPreview({
            content,
            gatewayUrl,
            agentId,
            workspaceId,
            relPath,
        });
        setHasTsModuleEntry(transformed.hasTsModuleEntry);

        const blob = new Blob([transformed.html], { type: "text/html;charset=utf-8" });
        const url = URL.createObjectURL(blob);
        setBlobUrl(url);

        // Cleanup: revoke blob URL when content changes or component unmounts
        return () => {
            URL.revokeObjectURL(url);
            setBlobUrl((prev) => (prev === url ? null : prev));
        };
    }, [content, gatewayUrl, agentId, workspaceId, relPath]);

    const handleLoad = () => setLoading(false);

    return (
        <div className="relative h-full w-full">
            {hasTsModuleEntry && (
                <div className="absolute left-3 right-3 top-3 z-20 flex gap-2 rounded-lg border border-amber-300 bg-amber-50/95 px-3 py-2 text-xs text-amber-900 shadow-sm dark:border-amber-700/60 dark:bg-amber-950/95 dark:text-amber-100">
                    <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" />
                    <span>{t("fileEditor.htmlPreviewTsModuleWarning")}</span>
                </div>
            )}
            {loading && (
                <div className="absolute inset-0 z-10 flex flex-col items-center justify-center gap-3 bg-chat-area">
                    <Loader2 className="h-8 w-8 animate-spin text-zinc-400" />
                    <span className="text-sm text-zinc-500">{t("fileEditor.loadingUrl")}</span>
                </div>
            )}
            {blobUrl && (
                <iframe
                    src={blobUrl}
                    className="h-full w-full border-0 bg-modal-surface"
                    sandbox="allow-scripts allow-same-origin"
                    title={fileName}
                    onLoad={handleLoad}
                />
            )}
        </div>
    );
}