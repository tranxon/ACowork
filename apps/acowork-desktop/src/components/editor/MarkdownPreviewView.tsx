import React, { createElement, useCallback, useLayoutEffect, useMemo, useRef, Children, isValidElement } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeRaw from "rehype-raw";
import { convertFileSrc } from "@tauri-apps/api/core";
import { Loader2 } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { CodeBlock } from "../chat/CodeBlock";
import { useAgentStore } from "../../stores/agentStore";
import { useFileEditorStore, type OpenFile } from "../../stores/fileEditorStore";
import { useWorkspaceStore } from "../../stores/workspaceStore";
import { cn } from "../../lib/utils";

/** ReactMarkdown component overrides — code blocks with title bar (mirrors ChatPanel). */
const markdownComponents = {
    pre: ({ children }: { children?: React.ReactNode }) => {
        const childArray = Children.toArray(children);
        const codeEl = childArray.find(
            (child): child is React.ReactElement<{ className?: string; children?: React.ReactNode }> =>
                isValidElement(child) && child.type === "code",
        );
        if (codeEl) {
            const { className, children: codeContent } = codeEl.props;
            const language = className?.replace(/^language-/, "") || "";
            const code = Children.toArray(codeContent).join("");
            return <CodeBlock language={language} code={code} />;
        }
        return <pre>{children}</pre>;
    },
    /** Intercept link clicks: open URLs in a file block tab instead of navigating the webview. */
    a: ({ href, children, ...rest }: React.AnchorHTMLAttributes<HTMLAnchorElement>) => {
        const handleClick = (e: React.MouseEvent) => {
            if (!href) return;
            if (!/^https?:\/\//i.test(href)) return;
            e.preventDefault();
            const agentId = useAgentStore.getState().selectedAgentId;
            if (agentId) {
                useFileEditorStore.getState().openUrl(agentId, href);
            }
        };
        return (
            <a href={href} onClick={handleClick} {...rest}>
                {children}
            </a>
        );
    },
};

/** URL schemes that should be passed through to the webview as-is. */
const PASSTHROUGH_SCHEMES = /^(https?:|data:|asset:|blob:|mailto:|tel:)/i;

/** Absolute path prefixes (POSIX `/...` or Windows `C:\...` / `C:/...`). */
const ABSOLUTE_PATH = /^([/\\]|[A-Za-z]:[/\\])/;

/**
 * Resolve a markdown image `src` (relative or rooted) against the workspace
 * root and the directory of the markdown file. Handles `./` and `../`
 * segments. Returns a forward-slash separated path suitable for
 * `convertFileSrc`, or `null` if `src` is a non-file URL scheme (http, data,
 * asset, etc.).
 */
function resolveLocalAssetPath(workspaceRoot: string, fileRelPath: string, src: string): string | null {
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
    return result.join("/");
}

interface MarkdownPreviewViewProps {
    file: OpenFile;
}

/**
 * Creates a ReactMarkdown component override that reads deprecated
 * `align` and `valign` attributes from the raw HAST node and applies
 * them via `useLayoutEffect` directly on the DOM element. This
 * approach bypasses React's prop filtering AND CSS specificity —
 * `el.style.setProperty(…, 'important')` overrides any stylesheet rule.
 */
function toAlignComponent(tag: string): React.ComponentType<any> {
    const AlignComponent = (props: any) => {
        const ref = useRef<HTMLElement>(null!);
        const nodeAlign = props.node?.properties?.align as string | undefined;
        const nodeValign = props.node?.properties?.valign as string | undefined;

        useLayoutEffect(() => {
            const el = ref.current;
            if (!el) return;
            if (nodeAlign === "center" || nodeAlign === "right" || nodeAlign === "left") {
                el.style.setProperty("text-align", nodeAlign, "important");
                el.setAttribute("data-align", nodeAlign);
            }
            if (nodeValign === "top" || nodeValign === "middle" || nodeValign === "bottom") {
                el.style.setProperty("vertical-align", nodeValign, "important");
                el.setAttribute("data-valign", nodeValign);
            }
        }, [nodeAlign, nodeValign]);

        const { node: _, style: __, ...rest } = props;
        return createElement(tag, { ...rest, ref });
    };
    return AlignComponent;
}

export function MarkdownPreviewView({ file }: MarkdownPreviewViewProps) {
    const { t } = useTranslation();
    const openFile = useFileEditorStore((s) => s.openFile);
    const treeRoots = useWorkspaceStore((s) => s.treeRoots);
    const workspaceRoot = treeRoots[`${file.agentId}:${file.workspaceId}`];

    /** Switch the current tab from preview mode back to edit mode. */
    const handleOpenAsEditor = useCallback(() => {
        void openFile(file.agentId, file.workspaceId, file.relPath);
    }, [openFile, file.agentId, file.workspaceId, file.relPath]);

    /**
     * Component overrides for the preview's ReactMarkdown instance.
     * Re-created only when the workspace root or the markdown file path
     * changes — these are the only inputs the `img` resolver depends on.
     */
    const previewComponents = useMemo(() => ({
        ...markdownComponents,
        /**
         * React-markdown strips deprecated HTML attributes (align, valign, etc.)
         * from React props. We read them from the raw HAST node injected by
         * react-markdown (`node`) and translate them into inline styles, which
         * are guaranteed to work across all rendering paths.
         */
        p: toAlignComponent("p"),
        h1: toAlignComponent("h1"),
        h2: toAlignComponent("h2"),
        h3: toAlignComponent("h3"),
        td: toAlignComponent("td"),
        th: toAlignComponent("th"),
        img: ({ src, alt, ...rest }: React.ImgHTMLAttributes<HTMLImageElement>) => {
            if (!src) return <img alt={alt} {...rest} />;
            // Pass-through schemes (http/https/data/asset/...) — render as-is.
            if (PASSTHROUGH_SCHEMES.test(src)) {
                return <img src={src} alt={alt} {...rest} />;
            }
            // No workspace root available yet — fall back to original src.
            if (!workspaceRoot) {
                return <img src={src} alt={alt} {...rest} />;
            }
            const absPath = resolveLocalAssetPath(workspaceRoot, file.relPath, src);
            if (!absPath) return <img src={src} alt={alt} {...rest} />;
            return <img src={convertFileSrc(absPath)} alt={alt} {...rest} />;
        },
    }), [workspaceRoot, file.relPath]);

    if (file.loading) {
        return (
            <div className="flex h-full items-center justify-center gap-2 text-xs text-zinc-400 dark:text-zinc-500">
                <Loader2 className="h-4 w-4 animate-spin" />
                {t("fileEditor.previewLoading")}
            </div>
        );
    }

    return (
        <div
            className={cn(
                "markdown-preview prose prose-sm prose-zinc max-w-none h-full overflow-y-auto px-5 py-4",
                "dark:prose-invert",
            )}
            onDoubleClick={handleOpenAsEditor}
            title={t("fileEditor.previewDoubleClickHint")}
        >
            {/* Injected CSS — bypasses Tailwind/LightningCSS processor to guarantee
                that deprecated HTML attributes (align, valign) are honored.
                The same rules also live in globals.css as a static fallback. */}
            <style>{`
                .markdown-preview [data-align="center"] { text-align: center !important; }
                .markdown-preview [data-align="right"]  { text-align: right  !important; }
                .markdown-preview [data-align="left"]   { text-align: left   !important; }
                .markdown-preview [data-valign="top"]    { vertical-align: top    !important; }
                .markdown-preview [data-valign="middle"] { vertical-align: middle !important; }
                .markdown-preview [data-valign="bottom"] { vertical-align: bottom !important; }
                /* Keep images inline inside paragraphs (Tailwind typography adds large margins) */
                .markdown-preview p img,
                .markdown-preview p a img { display: inline !important; margin-top: 0 !important; margin-bottom: 0 !important; vertical-align: middle; }
                /* Single-image paragraphs — center as block */
                .markdown-preview p > img:only-child { display: block !important; margin-left: auto !important; margin-right: auto !important; }
            `}</style>
            <ReactMarkdown
                remarkPlugins={[remarkGfm]}
                rehypePlugins={[rehypeRaw]}
                components={previewComponents as any}
            >
                {file.content}
            </ReactMarkdown>
        </div>
    );
}
