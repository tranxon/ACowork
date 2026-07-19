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

const wrapperClass =
  "my-2 w-full overflow-hidden rounded-md border border-chat-border bg-chat-body";

/**
 * Panzoom is bound to the inner div (HTML element). The inner div is
 * `width: 100%` (matches container width) with `min-height` set to
 * the SVG's natural height (so the div has a stable vertical extent).
 *
 * The SVG inside is absolutely positioned with `left: 50% /
 * translateX(-50%)`, so it is visually centered within the div
 * regardless of its own width vs. the container width. This means:
 *
 *   - Panzoom's `transform-origin: 50% 50%` (browser HTML default)
 *     scales the div from its own center — which matches the
 *     container's horizontal center.
 *   - The SVG sits centered inside the div, so `transform: scale(fit)`
 *     on the div zooms the SVG toward/from the panel's horizontal
 *     center, not its own center.
 *
 * Without this the SVG's own `width="1200"` HTML attribute would
 * either push the div out to 1200px (overflowing the panel) or, with
 * `w-full`, force the SVG into `width: 100%` (= container width) and
 * silently squish the diagram — losing the natural-size rendering we
 * want so pan/zoom can work in pixel units.
 */
const svgContainerClass =
  "relative w-full [&>svg]:absolute [&>svg]:left-1/2 [&>svg]:top-0 [&>svg]:-translate-x-1/2";

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
  /**
   * Inner div that wraps the rendered SVG. Panzoom is bound to THIS
   * div (an HTML element), not the SVG itself. SVG elements default
   * to `pointer-events: visiblePainted` which makes blank areas
   * unclickable — pan would never start. An HTML div receives
   * pointer events everywhere.
   */
  const panzoomTargetRef = useRef<HTMLDivElement>(null);
  const lastLoggedFailRef = useRef<number | null>(null);

  // ---- Pan & zoom ----
  const panzoomRef = useRef<PanzoomObject | null>(null);
  const svgNaturalSizeRef = useRef<{ width: number; height: number } | null>(null);
  const containerWidthRef = useRef(0);
  const [transformVersion, setTransformVersion] = useState(0);
  /**
   * Mirrors the current scale so we can keep the inner div's layout
   * height in sync with its visual height. Without this the div's
   * `minHeight` (set to the natural SVG height) would be larger than
   * the scaled visual height, leaving empty space around the diagram.
   */
  const [currentScale, setCurrentScale] = useState(1);

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
    // With the inner div at width:100% and the SVG centered inside it,
    // pan (0, 0) + zoom(fit) centers the diagram correctly because the
    // div's center matches the container's center.
    pz.zoom(fit, { animate: false, force: true });
    pz.pan(0, 0, { animate: false, force: true });
  };

  /**
   * Keeps the inner div's layout height in sync with its visual height
   * after `transform: scale()`. Without this the div's height would
   * stay at the SVG's natural pixel height, producing empty space
   * above and below the scaled diagram.
   */
  const syncLayoutHeight = (target: HTMLDivElement) => {
    const size = svgNaturalSizeRef.current;
    const scale = panzoomRef.current?.getScale() ?? 1;
    if (!size) return;
    target.style.minHeight = `${size.height * scale}px`;
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

  // Measure SVG natural size once rendered
  useLayoutEffect(() => {
    if (!containerRef.current || !svgContent) return;
    const svg = containerRef.current.querySelector("svg") as SVGSVGElement | null;
    if (!svg) return;

    let naturalW = parseFloat(svg.getAttribute("width") || "");
    let naturalH = parseFloat(svg.getAttribute("height") || "");
    if (!Number.isFinite(naturalW) || !Number.isFinite(naturalH)) {
      const vb = (svg.getAttribute("viewBox") || "").split(/\s+/).map(Number);
      if (vb.length === 4 && vb.every(Number.isFinite)) {
        naturalW = vb[2];
        naturalH = vb[3];
      }
    }
    if (!Number.isFinite(naturalW) || !Number.isFinite(naturalH)) return;

    svgNaturalSizeRef.current = { width: naturalW, height: naturalH };
    containerWidthRef.current = containerRef.current.clientWidth;
  }, [svgContent]);

  // Wire up @panzoom/panzoom on the inner wrapper DIV (not the SVG).
  // This is the critical fix: SVG's `pointer-events: visiblePainted`
  // makes blank areas unclickable, so pan never started. Binding to
  // the HTML div wrapper fixes both pan and the visual transform.
  useEffect(() => {
    if (!svgContent) return;
    const wrap = containerRef.current;
    const target = panzoomTargetRef.current;
    if (!wrap || !target) return;

    const pz = Panzoom(target, {
      animate: false,
      cursor: "grab",
      maxScale: SCALE_MAX,
      minScale: SCALE_MIN,
    });
    panzoomRef.current = pz;

    // Fit to container width on load, then sync the inner div's layout
    // height to the scaled visual height so there's no empty space.
    const fit = computeFitScale();
    if (fit != null) {
      pz.zoom(fit, { animate: false, force: true });
      pz.pan(0, 0, { animate: false, force: true });
    }
    setCurrentScale(pz.getScale());
    syncLayoutHeight(target);

    // Sync React state from library events for toolbar disabled state.
    // Also update the inner div's layout height on every scale change.
    const onPanzoomChange = (e: Event) => {
      const s = (e as CustomEvent<{ scale: number }>).detail.scale;
      setCurrentScale((prev) => (Math.abs(prev - s) < 1e-3 ? prev : s));
      setTransformVersion((v) => v + 1);
      syncLayoutHeight(target);
    };
    target.addEventListener("panzoomchange", onPanzoomChange);

    const onPanzoomStart = () => { target.style.cursor = "grabbing"; };
    const onPanzoomEnd = () => { target.style.cursor = "grab"; };
    target.addEventListener("panzoomstart", onPanzoomStart);
    target.addEventListener("panzoomend", onPanzoomEnd);

    // Crisp text under non-integer scales (lib FAQ #4)
    const svg = target.querySelector("svg") as SVGSVGElement | null;
    if (svg) {
      svg.querySelectorAll("text").forEach((t) => {
        t.setAttribute("text-rendering", "geometricPrecision");
      });
    }

    // Custom wheel zoom: linear deltaY scaling (not the library's
    // fixed-step zoomWithWheel which snaps on trackpad swipes).
    // Uses pz.zoomToPoint so the library handles the focal-point
    // math correctly (including the HTML transform-origin adjustment).
    const WHEEL_DENOMINATOR = 100;
    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      const oldScale = pz.getScale();
      const factor = Math.pow(SCALE_STEP, -e.deltaY / WHEEL_DENOMINATOR);
      const newScale = clampScale(oldScale * factor);
      if (newScale === oldScale) return;
      pz.zoomToPoint(newScale, e, { animate: false });
    };
    wrap.addEventListener("wheel", onWheel, { passive: false });

    const ro = new ResizeObserver((entries) => {
      for (const entry of entries) {
        containerWidthRef.current = entry.contentRect.width;
      }
    });
    ro.observe(wrap);

    return () => {
      wrap.removeEventListener("wheel", onWheel);
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
      className={`${wrapperClass} relative ${svgContent ? "[&_.label]:text-zinc-600" : ""} p-3`}
    >
      {svgContent ? (
        <>
          <div
            ref={panzoomTargetRef}
            className={svgContainerClass}
            dangerouslySetInnerHTML={{ __html: svgContent }}
          />

          <div
            className="absolute top-2 right-2 flex items-center gap-0.5 rounded-md
                       border border-chat-border bg-white/90 p-0.5
                       shadow-sm backdrop-blur-sm
                       dark:bg-zinc-800/90"
            onMouseDown={(e) => {
              e.stopPropagation();
              e.preventDefault();
            }}
          >
            <button
              type="button"
              onClick={zoomOut}
              disabled={currentScale <= SCALE_MIN + 1e-6}
              aria-label="缩小"
              title="缩小"
              data-version={transformVersion}
              className="flex h-7 w-7 cursor-pointer items-center justify-center rounded
                         text-zinc-600 hover:bg-zinc-200
                         disabled:cursor-not-allowed disabled:opacity-40
                         dark:text-zinc-400 dark:hover:bg-zinc-700"
            >
              <ZoomOut className="h-3.5 w-3.5" />
            </button>
            <button
              type="button"
              onClick={resetZoom}
              aria-label="重置缩放（撑满显示区域）"
              title="重置缩放（撑满显示区域）"
              data-version={transformVersion}
              className="flex h-7 w-7 cursor-pointer items-center justify-center rounded
                         text-zinc-600 hover:bg-zinc-200
                         dark:text-zinc-400 dark:hover:bg-zinc-700"
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
              className="flex h-7 w-7 cursor-pointer items-center justify-center rounded
                         text-zinc-600 hover:bg-zinc-200
                         disabled:cursor-not-allowed disabled:opacity-40
                         dark:text-zinc-400 dark:hover:bg-zinc-700"
            >
              <ZoomIn className="h-3.5 w-3.5" />
            </button>
          </div>
        </>
      ) : renderFailed ? (
        <pre className="m-0 whitespace-pre-wrap font-mono text-xs leading-relaxed text-zinc-500 dark:text-zinc-400">
          {chart}
        </pre>
      ) : (
        <div className="min-h-[140px] flex items-center justify-center">
          <div className="flex items-center gap-2 text-zinc-300 dark:text-zinc-500 select-none">
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
