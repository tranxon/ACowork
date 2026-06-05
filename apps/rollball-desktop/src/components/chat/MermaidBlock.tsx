import { useEffect, useRef, useState } from "react";
import mermaid from "mermaid";

/** (Re-)initialize mermaid global config. Safe to call multiple times. */
function ensureInit() {
  mermaid.initialize({
    startOnLoad: false,
    theme: "base",
    themeVariables: {
      background: "#ffffff",
      primaryColor: "#f8fafc",
      primaryBorderColor: "#cbd5e1",
      primaryTextColor: "#334155",
      lineColor: "#94a3b8",
      secondaryColor: "#f0fdf5",
      tertiaryColor: "#fdfaf5",
      clusterBkg: "#f8fafc",
      clusterBorder: "#d1d5db",
      edgeLabelBackground: "#ffffff",
      nodeBorder: "#cbd5e1",
      nodeTextColor: "#334155",
      fontSize: "12px",
      fontFamily: "system-ui, -apple-system, sans-serif",
      nodeBorderRadius: 12,
    },
    // Inject custom CSS for rounded corners and muted hierarchy colors
    themeCSS: [
      ".node.default > rect,",
      ".node.default > .label-container,",
      ".node > rect,",
      ".node > .label-container {",
      "  rx: 12px !important;",
      "  ry: 12px !important;",
      "}",
      ".node.default > rect,",
      ".node > rect {",
      "  fill: #f8fafc !important;",
      "  stroke: #cbd5e1 !important;",
      "}",
      ".cluster > g > .node.default > rect,",
      ".cluster > g > .node > rect {",
      "  fill: #f0fdf5 !important;",
      "  stroke: #a7c2b4 !important;",
      "}",
      ".cluster > g > .cluster > g > .node.default > rect,",
      ".cluster > g > .cluster > g > .node > rect {",
      "  fill: #fdfaf5 !important;",
      "  stroke: #c4b8a8 !important;",
      "}",
      ".cluster > g > .cluster > g > .cluster > g > .node.default > rect,",
      ".cluster > g > .cluster > g > .cluster > g > .node > rect {",
      "  fill: #f8f6fc !important;",
      "  stroke: #bdb8c8 !important;",
      "}",
      ".label-container {",
      "  border-radius: 12px !important;",
      "}",
    ].join("\n"),
    flowchart: {
      useMaxWidth: true,
      htmlLabels: true,
      curve: "basis",
      padding: 6,
      nodeSpacing: 35,
      rankSpacing: 35,
    },
    sequence: {
      useMaxWidth: true,
      showSequenceNumbers: false,
    },
  });
}

/** Simple non-crypto hash for stable mermaid IDs. */
function hashStr(s: string): number {
  let h = 0;
  for (let i = 0; i < s.length; i++) {
    h = ((h << 5) - h + s.charCodeAt(i)) | 0;
  }
  return h;
}

const wrapperClass =
  "my-2 w-full overflow-x-auto rounded-lg border border-zinc-200 bg-white p-3 dark:border-zinc-700 dark:bg-zinc-900/40";

/** Applied to the SVG-rendered container — forces SVG to fill available width */
const svgContainerClass =
  "[&_svg]:w-full [&_svg]:max-w-full";

interface MermaidBlockProps {
  chart: string;
}

export function MermaidBlock({ chart }: MermaidBlockProps) {
  const instanceIdRef = useRef(`m-${Math.random().toString(36).slice(2, 8)}`);
  const [svgContent, setSvgContent] = useState<string | null>(null);
  const [renderFailed, setRenderFailed] = useState(false);

  useEffect(() => {
    ensureInit();

    let cancelled = false;
    const id = `${instanceIdRef.current}-${hashStr(chart)}`;

    (async () => {
      try {
        const { svg } = await mermaid.render(id, chart);
        if (!cancelled) {
          setSvgContent(svg);
          setRenderFailed(false);
        }
      } catch {
        if (!cancelled) {
          setSvgContent(null);
          setRenderFailed(true);
        }
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [chart]);

  if (renderFailed) {
    return (
      <div className={wrapperClass}>
        <pre className="m-0 whitespace-pre-wrap font-mono text-xs leading-relaxed text-zinc-500 dark:text-zinc-400">
          {chart}
        </pre>
      </div>
    );
  }

  return (
    <div
      className={`${wrapperClass} ${svgContainerClass} [&_.label]:text-zinc-600`}
      dangerouslySetInnerHTML={svgContent ? { __html: svgContent } : undefined}
    />
  );
}