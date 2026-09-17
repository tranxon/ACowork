import mermaid from "mermaid";

/** (Re-)initialize mermaid global config. Safe to call multiple times. */
export function ensureInit() {
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
      fontFamily: '-apple-system, BlinkMacSystemFont, "Inter", "Segoe UI", "Noto Sans SC", "Microsoft YaHei", system-ui, sans-serif',
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
    // useMaxWidth: false → mermaid outputs SVG with explicit width/height
    // attrs (the viewBox pixel size), NOT width="100%" + max-width.
    // We need real pixel dimensions so we can measure the rendered
    // size reliably and compute the correct fit scale.
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
export function hashStr(s: string): number {
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

let renderSeq = 0;

/** Render mermaid chart → SVG string. Throws on invalid diagram.
 *  Global counter keeps render IDs unique across concurrent diagrams. */
export async function renderMermaid(chart: string): Promise<string> {
  ensureInit();
  renderSeq += 1;
  const { svg } = await mermaid.render(`md-${renderSeq}-${hashStr(chart)}`, chart);
  return svg;
}
