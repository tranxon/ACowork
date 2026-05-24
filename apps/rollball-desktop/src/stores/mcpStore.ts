//! MCP catalog and per-agent activation state management
//!
//! Manages two concerns:
//! 1. Global MCP catalog — server definitions + credentials (analogous to Vault for providers)
//! 2. Per-agent MCP activation — which servers are active for each agent

import { create } from "zustand";
import { getGatewayUrl } from "../lib/config";
import { emitAgentConfigRefresh } from "../lib/refresh";
import type {
  McpCatalogEntryResponse,
  McpServerConfigDef,
  AgentMcpServersResponse,
} from "../lib/types";

// ── Catalog types ────────────────────────────────────────────────────

interface McpCatalogState {
  /** Server entries from the global catalog */
  catalog: McpCatalogEntryResponse[];
  /** Loading state */
  loading: boolean;
  /** Error message */
  error: string | null;
}

interface McpCatalogActions {
  /** Load the global MCP catalog from Gateway */
  loadCatalog: () => Promise<void>;
  /** Add a single server entry to the catalog */
  addServer: (config: McpServerConfigDef) => Promise<void>;
  /** Update a single server entry in the catalog */
  updateServer: (name: string, config: McpServerConfigDef) => Promise<void>;
  /** Remove a server entry from the catalog */
  removeServer: (name: string) => Promise<void>;
  /** Replace the entire catalog */
  replaceCatalog: (servers: McpServerConfigDef[]) => Promise<void>;
}

// ── Per-agent activation types ───────────────────────────────────────

interface McpActivationState {
  /** Currently selected agent ID */
  agentId: string | null;
  /** Active MCP server names for the current agent */
  activeServers: string[];
  /** Loading state for activation */
  activationLoading: boolean;
}

interface McpActivationActions {
  /** Load active MCP server names for an agent */
  loadActiveServers: (agentId: string) => Promise<void>;
  /** Set active MCP servers for an agent (replaces the entire list) */
  setActiveServers: (agentId: string, serverNames: string[]) => Promise<void>;
  /** Toggle a single MCP server on/off for the current agent */
  toggleServer: (serverName: string) => Promise<void>;
}

// ── Combined store ───────────────────────────────────────────────────

export type McpStore = McpCatalogState &
  McpCatalogActions &
  McpActivationState &
  McpActivationActions;

export const useMcpStore = create<McpStore>((set, get) => ({
  // ── Catalog state ──
  catalog: [],
  loading: false,
  error: null,

  // ── Activation state ──
  agentId: null,
  activeServers: [],
  activationLoading: false,

  // ── Catalog actions ──

  loadCatalog: async () => {
    set({ loading: true, error: null });
    try {
      const resp = await fetch(`${getGatewayUrl()}/api/mcp-catalog`);
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      const data = (await resp.json()) as { servers: McpCatalogEntryResponse[] };
      set({ catalog: data.servers, loading: false });
    } catch (e: unknown) {
      const message = e instanceof Error ? e.message : String(e);
      set({ error: message, loading: false });
    }
  },

  addServer: async (config: McpServerConfigDef) => {
    set({ loading: true, error: null });
    try {
      const resp = await fetch(`${getGatewayUrl()}/api/mcp-catalog`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ ...config }),
      });
      if (!resp.ok) {
        const err = await resp.json().catch(() => ({ error: `HTTP ${resp.status}` }));
        throw new Error(err.error || `HTTP ${resp.status}`);
      }
      // Reload catalog after adding
      await get().loadCatalog();
      emitAgentConfigRefresh();
    } catch (e: unknown) {
      const message = e instanceof Error ? e.message : String(e);
      set({ error: message, loading: false });
    }
  },

  updateServer: async (name: string, config: McpServerConfigDef) => {
    set({ loading: true, error: null });
    try {
      const resp = await fetch(`${getGatewayUrl()}/api/mcp-catalog/${encodeURIComponent(name)}`, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ ...config }),
      });
      if (!resp.ok) {
        const err = await resp.json().catch(() => ({ error: `HTTP ${resp.status}` }));
        throw new Error(err.error || `HTTP ${resp.status}`);
      }
      // Reload catalog after updating
      await get().loadCatalog();
      emitAgentConfigRefresh();
    } catch (e: unknown) {
      const message = e instanceof Error ? e.message : String(e);
      set({ error: message, loading: false });
    }
  },

  removeServer: async (name: string) => {
    set({ loading: true, error: null });
    try {
      const resp = await fetch(`${getGatewayUrl()}/api/mcp-catalog/${encodeURIComponent(name)}`, {
        method: "DELETE",
      });
      if (!resp.ok) {
        const err = await resp.json().catch(() => ({ error: `HTTP ${resp.status}` }));
        throw new Error(err.error || `HTTP ${resp.status}`);
      }
      // Reload catalog after removing
      await get().loadCatalog();
      emitAgentConfigRefresh();
    } catch (e: unknown) {
      const message = e instanceof Error ? e.message : String(e);
      set({ error: message, loading: false });
    }
  },

  replaceCatalog: async (servers: McpServerConfigDef[]) => {
    set({ loading: true, error: null });
    try {
      const resp = await fetch(`${getGatewayUrl()}/api/mcp-catalog`, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(servers),
      });
      if (!resp.ok) {
        const err = await resp.json().catch(() => ({ error: `HTTP ${resp.status}` }));
        throw new Error(err.error || `HTTP ${resp.status}`);
      }
      // Reload catalog after replacing
      await get().loadCatalog();
      emitAgentConfigRefresh();
    } catch (e: unknown) {
      const message = e instanceof Error ? e.message : String(e);
      set({ error: message, loading: false });
    }
  },

  // ── Activation actions ──

  loadActiveServers: async (agentId: string) => {
    set({ agentId, activationLoading: true, error: null });
    try {
      const resp = await fetch(`${getGatewayUrl()}/api/agents/${encodeURIComponent(agentId)}/mcp-servers`);
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      const data = (await resp.json()) as AgentMcpServersResponse;
      set({ activeServers: data.active_servers, activationLoading: false });
    } catch (e: unknown) {
      const message = e instanceof Error ? e.message : String(e);
      set({ error: message, activationLoading: false, activeServers: [] });
    }
  },

  setActiveServers: async (agentId: string, serverNames: string[]) => {
    set({ activationLoading: true, error: null });
    try {
      const resp = await fetch(`${getGatewayUrl()}/api/agents/${encodeURIComponent(agentId)}/mcp-servers`, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ servers: serverNames }),
      });
      if (!resp.ok) {
        const err = await resp.json().catch(() => ({ error: `HTTP ${resp.status}` }));
        throw new Error(err.error || `HTTP ${resp.status}`);
      }
      set({ activeServers: serverNames, activationLoading: false });
    } catch (e: unknown) {
      const message = e instanceof Error ? e.message : String(e);
      set({ error: message, activationLoading: false });
    }
  },

  toggleServer: async (serverName: string) => {
    const { agentId, activeServers } = get();
    if (!agentId) return;

    const isActive = activeServers.includes(serverName);
    const newServers = isActive
      ? activeServers.filter((s) => s !== serverName)
      : [...activeServers, serverName];

    await get().setActiveServers(agentId, newServers);
  },
}));
