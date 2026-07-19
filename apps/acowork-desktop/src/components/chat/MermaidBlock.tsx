import { useEffect, useRef, useLayoutEffect, useState } from "react";
import mermaid from "mermaid";
import Panzoom, { PanzoomObject } from "@panzoom/panzoom";
import { ZoomIn, ZoomOut, Maximize2 } from "lucide-react";

const SCALE_MIN = 0.2;
const SCALE_MAX = 8;
const SCALE_STEP = 1.2;

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
      useMaxWidth: false,
      htmlLabels: true,
      curve: "basis",
      padding: 6,
      nodeSpacing: 35,
      rankSpacing: 35,
    },
    sequence: {
      useMaxWidth: false,
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

export function isPlausibleMermaid(code: string): boolean {
  const trimmed = code.trim();
  if (!trimmed) return false;

  const lines = trimmed.split("\n");
  if (lines.length < 2) return false;

  const firstLine = lines[0].trim();
  const supported = [
    "flowchart", "graph", "sequenceDiagram", "classDiagram",
    "stateDiagram", "stateDiagram-v2", "erDiagram", "gantt",
    "pie", "gitGraph", "mindmap", "timeline", "quadrantChart",
    "xyChart", "block", "architecture", "kanban", "sankey", "xychart",
  ];
  if (!supported.some((t) => firstLine.startsWith(t))) return false;

  const lastNonEmpty = [...lines].reverse().find((l) => l.trim().length > 0);
  if (lastNonEmpty) {
    const endsWithPartial = /(?:-->|->|==>|=>|-\.->|--x|--o)$/.test(lastNonEmpty.trim());
    if (endsWithPartial) return false;
  }

  if (firstLine.startsWith("flowchart") || firstLine.startsWith("graph")) {
    let depth = 0;
    for (const line of lines) {
      const t = line.trim();
      if (/^subgraph\b/i.test(t)) depth++;
      if (/^end(\s|$)/.test(t)) depth--;
    }
    if (depth > 0) return false;
  } else if (
    firstLine.startsWith("stateDiagram") ||
    firstLine.startsWith("classDiagram")
  ) {
    let depth = 0;
    for (const line of lines) {
      for (const ch of line) {
        if (ch === "{") depth++;
        else if (ch === "}") depth--;
      }
    }
    if (depth > 0) return false;
  }

  return true;
}

interface MermaidBlockProps {
  chart: string;
}

export function MermaidBlock({ chart }: MermaidBlockProps) {
  const instanceIdRef = useRef(`m-${Math.random().toString(36).slice(2, 8)}`);
  const [svgContent, setSvgContent] = useState<string | null>(null);
  const [renderFailed, setRenderFailed] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);
  /** Content area wrapper — has overflow:hidden + bg-chat-body.
   *  Its height is set to match the SVG's aspect ratio scaled to fit width.
   *  Panzoom target lives inside this, so panned content is clipped here. */
  const contentAreaRef = useRef<HTMLDivElement>(null);
  /** The div that wraps the SVG. Panzoom is bound to this. */
  const panzoomTargetRef = useRef<HTMLDivElement>(null);
  const lastLoggedFailRef = useRef<number | null>(null);

  // ---- Pan & zoom ----
  const panzoomRef = useRef<PanzoomObject | null>(null);
  const svgNaturalSizeRef = useRef<{ width: number; height: number } | null>(null);
  const containerWidthRef = useRef(0);
  const [transformVersion, setTransformVersion] = useState(0);
  const [currentScale, setCurrentScale] = useState(1);
  /** The height of the content area (px), derived from SVG aspect ratio + container width. */
  const [contentHeight, setContentHeight] = useState<number | null>(null);

  const clampScale = (s: number) =>
    Math.max(SCALE_MIN, Math.min(SCALE_MAX, s));

  const computeFitScale = (): number | null => {
    const size = svgNaturalSizeRef.current;
    const w = containerWidthRef.current;
    if (!size || !size.width || w <= 0) return null;
    return clampScale(w / size.width);
  };

  const zoomIn = () => {
    const pz = panzoomRef.current;
    if (!pz) return;
    pz.zoom(clampScale(pz.getScale() * SCALE_STEP), { animate: false });
  };
  const zoomOut = () => {
    const pz = panzoomRef.current;
    if (!pz) return;
    pz.zoom(clampScale(pz.getScale() / SCALE_STEP), { animate: false });
  };
  const resetZoom = () => {
    const pz = panzoomRef.current;
    const fit = computeFitScale();
    if (!pz || fit == null) return;
    pz.zoom(fit, { animate: false, force: true });
    pz.pan(0, 0, { animate: false, force: true });
  };

  // Debounced mermaid render
  useEffect(() => {
    const timer = setTimeout(() => {
      if (!isPlausibleMermaid(chart)) return;
      ensureInit();
      let cancelled = false;
      const id = `${instanceIdRef.current}-${hashStr(chart)}`;
      const failHash = hashStr(chart);
      (async () => {
        try {
          const { svg } = await mermaid.render(id, chart);
          if (!cancelled) {
            setSvgContent(svg);
            setRenderFailed(false);
          }
        } catch (err) {
          if (lastLoggedFailRef.current !== failHash) {
            console.error("[MermaidBlock] render failed:", err);
            console.error("[MermaidBlock] chart content:", chart.slice(0, 500));
            lastLoggedFailRef.current = failHash;
          }
          if (!cancelled) {
            setSvgContent(null);
            setRenderFailed(true);
          }
        }
      })();
    }, 200);
    return () => { clearTimeout(timer); };
  }, [chart]);

  // Measure SVG, set content area height, create panzoom, fit to container
  useLayoutEffect(() => {
    if (!containerRef.current || !contentAreaRef.current || !panzoomTargetRef.current || !svgContent) {
      setContentHeight(null);
      return;
    }

    const target = panzoomTargetRef.current;
    const contentArea = contentAreaRef.current;
    const wrap = containerRef.current;

    // Fix the SVG: make it a block element with explicit dimensions
    // so it sits properly at (0,0) inside the panzoom target div.
    const svg = target.querySelector("svg") as SVGSVGElement | null;
    if (!svg) return;

    // Remove max-width constraint that mermaid injects (e.g. style="max-width: 1235px")
    svg.style.maxWidth = "none";
    // Block layout eliminates inline vertical-align gaps
    svg.style.display = "block";

    // Read natural size from viewBox (mermaid SVGs typically don't have width/height attrs)
    const vb = (svg.getAttribute("viewBox") || "").split(/\s+/).map(Number);
    let naturalW: number;
    let naturalH: number;
    if (vb.length === 4 && vb.every(Number.isFinite)) {
      naturalW = vb[2];
      naturalH = vb[3];
      // Set explicit dimensions so the SVG has a well-defined layout box
      svg.setAttribute("width", String(naturalW));
      svg.setAttribute("height", String(naturalH));
    } else {
      naturalW = parseFloat(svg.getAttribute("width") || "");
      naturalH = parseFloat(svg.getAttribute("height") || "");
      if (!Number.isFinite(naturalW) || !Number.isFinite(naturalH)) return;
    }

    // Make the panzoom target div match the SVG's natural size.
    // Without this, the div is constrained by the content area width (~600px),
    // but the SVG is 1235px wide — the SVG overflows and positioning breaks.
    target.style.width = `${naturalW}px`;

    svgNaturalSizeRef.current = { width: naturalW, height: naturalH };
    const containerW = wrap.clientWidth;
    containerWidthRef.current = containerW;

    // Set content area height to maintain aspect ratio at current container width
    const h = (naturalH / naturalW) * containerW;
    setContentHeight(h);

    // Panzoom 4.x sets transform-origin to 50% 50% for non-SVG elements (DIVs),
    // which causes the element to shift right and down when scaled.
    // Passing origin: '0 0' keeps the top-left corner anchored.
    const pz = Panzoom(target, {
      animate: false,
      cursor: "grab",
      maxScale: SCALE_MAX,
      minScale: SCALE_MIN,
      startScale: 1,
      startX: 0,
      startY: 0,
      origin: "0 0",
    });
    panzoomRef.current = pz;

    // Fit to container width before paint
    const fit = clampScale(containerW / naturalW);
    pz.zoom(fit, { animate: false, force: true });
    pz.pan(0, 0, { animate: false, force: true });
    setCurrentScale(pz.getScale());

    // Sync React state on pan/zoom changes
    const onPanzoomChange = (e: Event) => {
      const s = (e as CustomEvent<{ scale: number }>).detail.scale;
      setCurrentScale((prev) => (Math.abs(prev - s) < 1e-3 ? prev : s));
      setTransformVersion((v) => v + 1);
    };
    target.addEventListener("panzoomchange", onPanzoomChange);

    const onPanzoomStart = () => { target.style.cursor = "grabbing"; };
    const onPanzoomEnd = () => { target.style.cursor = "grab"; };
    target.addEventListener("panzoomstart", onPanzoomStart);
    target.addEventListener("panzoomend", onPanzoomEnd);

    // Crisp text under non-integer scales
    svg.querySelectorAll("text").forEach((t) => {
      t.setAttribute("text-rendering", "geometricPrecision");
    });

    // Custom wheel zoom — only when Ctrl/Cmd is held (like browser zoom),
    // so normal wheel scrolls the page instead of zooming the diagram.
    const WHEEL_DENOMINATOR = 100;
    const onWheel = (e: WheelEvent) => {
      if (!e.ctrlKey && !e.metaKey) return;
      e.preventDefault();
      const oldScale = pz.getScale();
      const factor = Math.pow(SCALE_STEP, -e.deltaY / WHEEL_DENOMINATOR);
      const newScale = clampScale(oldScale * factor);
      if (newScale === oldScale) return;
      pz.zoomToPoint(newScale, e, { animate: false });
    };
    contentArea.addEventListener("wheel", onWheel, { passive: false });

    // Resize observer: update container width and re-zoom
    const ro = new ResizeObserver((entries) => {
      for (const entry of entries) {
        const newW = entry.contentRect.width;
        containerWidthRef.current = newW;
        // Re-zoom to fit + update content area height
        const newFit = clampScale(newW / naturalW);
        const newH = (naturalH / naturalW) * newW;
        setContentHeight(newH);
        pz.zoom(newFit, { animate: false, force: true });
        pz.pan(0, 0, { animate: false, force: true });
        setCurrentScale(pz.getScale());
      }
    });
    ro.observe(wrap);

    return () => {
      contentArea.removeEventListener("wheel", onWheel);
      target.removeEventListener("panzoomchange", onPanzoomChange);
      target.removeEventListener("panzoomstart", onPanzoomStart);
      target.removeEventListener("panzoomend", onPanzoomEnd);
      ro.disconnect();
      pz.destroy();
      panzoomRef.current = null;
    };
  }, [svgContent]);

  return (
    <div
      ref={containerRef}
      className="my-2 w-full rounded-md border border-chat-border"
    >
      {/* Title bar — matches CodeBlock pattern */}
      <div className="flex items-center justify-between border-b border-chat-border bg-chat-title px-3 py-1.5">
        <div className="flex items-center gap-1.5">
          <span className="text-xs font-medium text-zinc-500 dark:text-zinc-400">
            mermaid
          </span>
        </div>
        <div className="flex items-center gap-1.5">
          <button
            type="button"
            onClick={zoomOut}
            disabled={currentScale <= SCALE_MIN + 1e-6}
            aria-label="缩小"
            title="缩小"
            data-version={transformVersion}
            className="flex items-center justify-center rounded p-0.5 text-zinc-500 hover:text-zinc-700 hover:bg-zinc-200 dark:text-zinc-400 dark:hover:text-zinc-200 dark:hover:bg-zinc-700 disabled:opacity-40 disabled:cursor-not-allowed"
          >
            <ZoomOut className="h-3.5 w-3.5" />
          </button>
          <button
            type="button"
            onClick={resetZoom}
            aria-label="重置缩放（撑满显示区域）"
            title="重置缩放（撑满显示区域）"
            data-version={transformVersion}
            className="flex items-center justify-center rounded p-0.5 text-zinc-500 hover:text-zinc-700 hover:bg-zinc-200 dark:text-zinc-400 dark:hover:text-zinc-200 dark:hover:bg-zinc-700"
          >
            <Maximize2 className="h-3.5 w-3.5" />
          </button>
          <button
            type="button"
            onClick={zoomIn}
            disabled={currentScale >= SCALE_MAX - 1e-6}
            aria-label="放大"
            title="放大"
            data-version={transformVersion}
            className="flex items-center justify-center rounded p-0.5 text-zinc-500 hover:text-zinc-700 hover:bg-zinc-200 dark:text-zinc-400 dark:hover:text-zinc-200 dark:hover:bg-zinc-700 disabled:opacity-40 disabled:cursor-not-allowed"
          >
            <ZoomIn className="h-3.5 w-3.5" />
          </button>
        </div>
      </div>

      {/* Content area — clips panned SVG content, never overlaps title bar */}
      {svgContent ? (
        <div
          ref={contentAreaRef}
          className="relative overflow-hidden bg-chat-body"
          style={{ height: contentHeight ?? undefined }}
        >
          <div
            ref={panzoomTargetRef}
            dangerouslySetInnerHTML={{ __html: svgContent }}
          />
        </div>
      ) : renderFailed ? (
        <pre className="m-0 whitespace-pre-wrap bg-chat-body p-3 font-mono text-xs leading-relaxed text-zinc-500 dark:text-zinc-400">
          {chart}
        </pre>
      ) : (
        <div className="flex min-h-[140px] items-center justify-center bg-chat-body">
          <div className="flex items-center gap-2 text-zinc-300 select-none dark:text-zinc-500">
            <svg
              className="h-4 w-4 animate-spin"
              xmlns="http://www.w3.org/2000/svg"
              fill="none"
              viewBox="0 0 24 24"
            >
              <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" />
              <path
                className="opacity-75"
                fill="currentColor"
                d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 014 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z"
              />
            </svg>
            <span className="text-xs">Rendering diagram...</span>
          </div>
        </div>
      )}
    </div>
  );
}