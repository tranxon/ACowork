import { useState, useEffect } from "react";
import { Loader2 } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";

interface HtmlPreviewViewProps {
    /** Raw HTML content fetched via the Gateway JSON API */
    content: string;
    /** Gateway base URL (e.g. "http://localhost:19876") */
    gatewayUrl: string;
    /** Agent ID for constructing ws-files URLs */
    agentId: string;
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
export function HtmlPreviewView({ content, gatewayUrl, agentId, fileName }: HtmlPreviewViewProps) {
    const { t } = useTranslation();
    const [loading, setLoading] = useState(true);
    const [blobUrl, setBlobUrl] = useState<string | null>(null);

    useEffect(() => {
        if (!content) return;

        setLoading(true);

        // Inject <base> tag so root-relative URLs resolve to the Gateway
        const baseHref = `${gatewayUrl}/ws-files/${agentId}/`;
        const modified = content.replace(
            /<head[^>]*>/i,
            (match) => `${match}\n<base href="${baseHref}">`,
        );

        const blob = new Blob([modified], { type: "text/html;charset=utf-8" });
        const url = URL.createObjectURL(blob);
        setBlobUrl(url);

        // Cleanup: revoke blob URL when content changes or component unmounts
        return () => {
            URL.revokeObjectURL(url);
            setBlobUrl((prev) => (prev === url ? null : prev));
        };
    }, [content, gatewayUrl, agentId]);

    const handleLoad = () => setLoading(false);

    return (
        <div className="relative h-full w-full">
            {loading && (
                <div className="absolute inset-0 z-10 flex flex-col items-center justify-center gap-3 bg-white dark:bg-zinc-900">
                    <Loader2 className="h-8 w-8 animate-spin text-zinc-400" />
                    <span className="text-sm text-zinc-500">{t("fileEditor.loadingUrl")}</span>
                </div>
            )}
            {blobUrl && (
                <iframe
                    src={blobUrl}
                    className="h-full w-full border-0 bg-white"
                    sandbox="allow-scripts allow-same-origin"
                    title={fileName}
                    onLoad={handleLoad}
                />
            )}
        </div>
    );
}