// MCP preset registry — curated list of practical MCP servers
//
// Selection principle: only include servers that fill capability gaps
// NOT covered by RollBall built-in tools (file IO, shell, web_fetch,
// http_request, memory/grafeo, doc_reader basic).
//
// These presets are frontend-only definitions that help users quickly
// add common MCP servers to their catalog. Each preset defines the
// server config template, required env vars (API keys), and install hints.

import type { McpPresetDef } from "./types";

export const MCP_PRESETS: McpPresetDef[] = [
  // ── Browser Automation ────────────────────────────────────────────
  {
    id: "playwright",
    name: "Playwright",
    description:
      "Browser automation — navigate pages, take screenshots, fill forms, extract structured content. Supports Chromium, Firefox, WebKit.",
    category: "browser",
    transport: "stdio",
    command: "npx",
    args: ["-y", "@playwright/mcp@latest"],
    requiredEnv: [],
    optionalEnv: {},
    installHint:
      "npx auto-installs. First run downloads browser binaries (~300MB). No API key required.",
    icon: "Monitor",
  },

  // ── Web Search ─────────────────────────────────────────────────────
  {
    id: "brave-search",
    name: "Brave Search",
    description:
      "Privacy-first web & local search via Brave Search API. Good for general knowledge, news, and factual lookups.",
    category: "search",
    transport: "stdio",
    command: "npx",
    args: ["-y", "@modelcontextprotocol/server-brave-search"],
    requiredEnv: ["BRAVE_API_KEY"],
    optionalEnv: {},
    installHint:
      "Get free API key at https://brave.com/search/api/ — generous free tier (2K queries/month).",
    icon: "Search",
  },
  {
    id: "exa-search",
    name: "Exa Search",
    description:
      "AI-native semantic search engine. Returns clean, structured content optimized for LLM consumption. Supports neural search, auto-crawl, and content extraction.",
    category: "search",
    transport: "stdio",
    command: "npx",
    args: ["-y", "exa-mcp-server"],
    requiredEnv: ["EXA_API_KEY"],
    optionalEnv: {},
    installHint:
      "Get API key at https://exa.ai — free tier includes 1000 searches/month. Superior to keyword search for complex queries.",
    icon: "Globe",
  },
  {
    id: "context7",
    name: "Context7",
    description:
      "Fetch up-to-date library documentation and code examples. Prevents LLM hallucinations by grounding responses in real, version-specific docs.",
    category: "search",
    transport: "stdio",
    command: "npx",
    args: ["-y", "@upstash/context7-mcp@latest"],
    requiredEnv: [],
    optionalEnv: {},
    installHint:
      "npx auto-installs. No API key required. Covers 20K+ libraries (React, Next.js, Python, Rust crates, etc.).",
    icon: "BookOpen",
  },

  // ── Document Processing ────────────────────────────────────────────
  {
    id: "docling",
    name: "Docling",
    description:
      "Advanced document understanding: OCR for image-based PDFs, layout analysis, table extraction, and legacy Office format support (.doc/.ppt/.xls). Fills RollBall doc_reader gaps.",
    category: "document",
    transport: "stdio",
    command: "uvx",
    args: ["docling-mcp"],
    requiredEnv: [],
    optionalEnv: {},
    installHint:
      "Requires uv (Python package manager). Install: `pip install uv` or see https://docs.astral.sh/uv/. First run downloads ML models (~500MB). No API key required — fully local.",
    icon: "FileText",
  },

  // ── Knowledge & Collaboration ──────────────────────────────────────
  {
    id: "notion",
    name: "Notion",
    description:
      "Read, write, and search Notion pages and databases. Agents can manage knowledge bases, create docs, query structured data.",
    category: "knowledge",
    transport: "stdio",
    command: "npx",
    args: ["-y", "@notionhq/notion-mcp-server"],
    requiredEnv: ["NOTION_API_KEY"],
    optionalEnv: {},
    installHint:
      "Create integration at https://www.notion.so/my-integrations — then share target pages with the integration.",
    icon: "FileText",
  },

  // ── Design ─────────────────────────────────────────────────────────
  {
    id: "figma",
    name: "Figma",
    description:
      "Read Figma designs — extract layouts, styles, components, and assets. Enables agents to understand UI designs and generate matching code.",
    category: "design",
    transport: "stdio",
    command: "npx",
    args: ["-y", "figma-developer-mcp", "--figma-api-key=$FIGMA_API_KEY"],
    requiredEnv: ["FIGMA_API_KEY"],
    optionalEnv: {},
    installHint:
      "Generate personal access token at https://www.figma.com/developers/api#access-tokens — then share Figma files with the token owner.",
    icon: "PenTool",
  },
];

/** Get presets grouped by category */
export function getPresetsByCategory(): Record<string, McpPresetDef[]> {
  const groups: Record<string, McpPresetDef[]> = {};
  for (const preset of MCP_PRESETS) {
    if (!groups[preset.category]) {
      groups[preset.category] = [];
    }
    groups[preset.category].push(preset);
  }
  return groups;
}

/** Find a preset by ID */
export function getPresetById(id: string): McpPresetDef | undefined {
  return MCP_PRESETS.find((p) => p.id === id);
}

/** Convert a preset to an McpServerConfigDef (for adding to catalog) */
export function presetToServerConfig(
  preset: McpPresetDef,
  envOverrides?: Record<string, string>,
): import("./types").McpServerConfigDef {
  const env: Record<string, string> = { ...preset.optionalEnv, ...envOverrides };
  return {
    name: preset.id,
    transport: preset.transport,
    url: preset.url,
    command: preset.command ?? "",
    args: preset.args ?? [],
    env,
    headers: {},
    tool_timeout_secs: undefined,
  };
}
