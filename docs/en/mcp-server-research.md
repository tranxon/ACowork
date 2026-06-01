# MCP (Model Context Protocol) Server Research Report

**Date**: May 2026  
**Purpose**: Comprehensive reference for integrating MCP server support into a desktop AI application

---

## 1. Official MCP Registry & Directories

| Resource | URL | Description |
|----------|-----|-------------|
| **Official Registry API** | https://registry.modelcontextprotocol.io/ | The canonical registry — "an app store for MCP servers". API at v0.1 freeze. Built in Go + PostgreSQL. |
| **Registry GitHub** | https://github.com/modelcontextprotocol/registry | Source code for the registry. Publisher CLI available. Server IDs use reverse-domain naming (e.g. `io.github.username/server-name`). |
| **Registry Browser** | https://registry.mcpservers.org/ | Third-party browser for the official registry |
| **MCPAlign Directory** | https://www.mcpalign.com/registry | Browse by category, language, publisher |
| **Official Examples Page** | https://modelcontextprotocol.io/examples | Reference servers listed by Anthropic |
| **Official Servers Repo** | https://github.com/modelcontextprotocol/servers | Reference implementations + official integrations + community servers |
| **Awesome MCP Servers** | https://github.com/punkpeye/awesome-mcp-servers | Curated community list |
| **MCPBundles** | https://www.mcpbundles.com/ | Hosted MCP endpoint — 700+ providers behind one URL |
| **LobeHub MCP** | https://lobehub.com/mcp | Community directory with per-server config guides |

### Registry Publishing Model
- Server IDs: `io.github.<username>/<server-name>` or `me.<domain>/<server-name>`
- Auth for publishing: GitHub OAuth, GitHub OIDC (CI/CD), DNS verification, HTTP verification
- Live API docs: https://registry.modelcontextprotocol.io/docs

---

## 2. Top 20 Most Popular MCP Servers (Developer Productivity Focus)

### Category: File System & Local Tools

| # | Server | What It Does | Package / Install | Transport | Required Config |
|---|--------|-------------|-------------------|-----------|-----------------|
| 1 | **Filesystem** | Secure local file read/write/search with configurable access controls | `npx -y @modelcontextprotocol/server-filesystem <path>` | stdio | Path to allowed directories as CLI arg |
| 2 | **Memory** | Knowledge-graph-based persistent memory across conversations | `npx -y @modelcontextprotocol/server-memory` | stdio | None |
| 3 | **Sequential Thinking** | Dynamic problem-solving through structured thought sequences | `npx -y @modelcontextprotocol/server-sequentialthinking` | stdio | None |
| 4 | **Desktop Commander** | Local filesystem + terminal interactions, launch apps, manage windows | `npx -y @wonderwhy-er/desktop-commander` | stdio | None |

### Category: Web Search & Fetch

| # | Server | What It Does | Package / Install | Transport | Required Config |
|---|--------|-------------|-------------------|-----------|-----------------|
| 5 | **Fetch** | Web content fetching and HTML-to-markdown conversion for LLMs | `npx -y @modelcontextprotocol/server-fetch` | stdio | None |
| 6 | **Brave Search** | Privacy-first web & local search via Brave's Search API | `npx -y @brave/brave-search-mcp-server` | stdio | `BRAVE_API_KEY` env var |
| 7 | **Tavily** | Real-time search, content extraction, web crawling (1K free credits/mo) | `npx -y tavily-mcp` | stdio | `TAVILY_API_KEY` env var |
| 8 | **Context7** | Fetches version-specific docs from official sources, prevents API hallucination | `npx -y @upstash/context7-mcp` | stdio | None |

### Category: Git & Version Control

| # | Server | What It Does | Package / Install | Transport | Required Config |
|---|--------|-------------|-------------------|-----------|-----------------|
| 9 | **Git** | Read, search, and manipulate Git repositories | `uvx mcp-server-git --repository <path>` | stdio | Repository path as CLI arg |
| 10 | **GitHub** | PRs, issues, code search, actions, releases | `npx -y @modelcontextprotocol/server-github` | stdio | `GITHUB_PERSONAL_ACCESS_TOKEN` env var |

### Category: Database

| # | Server | What It Does | Package / Install | Transport | Required Config |
|---|--------|-------------|-------------------|-----------|-----------------|
| 11 | **PostgreSQL** | Read-only SQL queries, schema exploration, table inspection | `npx -y @modelcontextprotocol/server-postgres <connection-string>` | stdio | PostgreSQL connection string as CLI arg |
| 12 | **SQLite** | Database interaction, business intelligence queries | `npx -y @modelcontextprotocol/server-sqlite <db-path>` | stdio | Database file path as CLI arg |
| 13 | **Supabase** | Postgres queries + auth + storage + edge functions (20+ tools) | `npx -y supabase-mcp-server` | stdio | `SUPABASE_ACCESS_TOKEN` + `SUPABASE_PROJECT_ID` env vars |

### Category: Browser Automation

| # | Server | What It Does | Package / Install | Transport | Required Config |
|---|--------|-------------|-------------------|-----------|-----------------|
| 14 | **Playwright** | Browser automation via accessibility snapshots, supports all major browsers | `npx -y @playwright/mcp@latest` | stdio | Optional: `--browser chromium\|firefox\|webkit` |

### Category: Cloud Services

| # | Server | What It Does | Package / Install | Transport | Required Config |
|---|--------|-------------|-------------------|-----------|-----------------|
| 15 | **Cloudflare** | DNS, CDN, Workers, KV, R2, D1, security rules management | `npx -y cloudflare-mcp-server` | stdio | `CLOUDFLARE_API_TOKEN` + `CLOUDFLARE_ACCOUNT_ID` env vars |
| 16 | **AWS** | S3, EC2, Lambda, CloudWatch management | Multiple per-service packages | stdio | AWS credentials via env vars or IAM |

### Category: Communication

| # | Server | What It Does | Package / Install | Transport | Required Config |
|---|--------|-------------|-------------------|-----------|-----------------|
| 17 | **Slack** | Channel search, message history, thread context, posting | `npx -y @modelcontextprotocol/server-slack` | stdio | `SLACK_BOT_TOKEN` + `SLACK_TEAM_ID` env vars |
| 18 | **Discord** | Server/channel management, message history, member lookup | Community-built (multiple options) | stdio | Discord bot token |

### Category: Knowledge & Project Management

| # | Server | What It Does | Package / Install | Transport | Required Config |
|---|--------|-------------|-------------------|-----------|-----------------|
| 19 | **Notion** | Read/write Notion pages, databases, knowledge base search | Official: `npx -y @notion/mcp-server` | stdio | OAuth or `NOTION_API_KEY` env var |
| 20 | **Linear** | Issue tracking, project management, cycle reporting | `npx -y linear-mcp-server` | stdio | `LINEAR_API_KEY` env var |

---

## 3. Complete Configuration Examples (Claude Desktop JSON format)

### Minimal Config (no auth required)
```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/allowed/files"]
    },
    "memory": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-memory"]
    },
    "fetch": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-fetch"]
    },
    "sequential-thinking": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-sequentialthinking"]
    }
  }
}
```

### Config with Authentication
```json
{
  "mcpServers": {
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": {
        "GITHUB_PERSONAL_ACCESS_TOKEN": "<YOUR_TOKEN>"
      }
    },
    "brave-search": {
      "command": "npx",
      "args": ["-y", "@brave/brave-search-mcp-server"],
      "env": {
        "BRAVE_API_KEY": "<YOUR_KEY>"
      }
    },
    "postgres": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-postgres", "postgresql://user:pass@localhost/mydb"]
    }
  }
}
```

### Remote/SSE Server Config (different clients)
```json
// Claude Desktop
{
  "mcpServers": {
    "my-server": {
      "url": "https://api.example.com/mcp",
      "headers": {
        "x-api-key": "your-api-key"
      }
    }
  }
}

// Cursor (same as Claude Desktop for remote)
{
  "mcpServers": {
    "my-server": {
      "url": "https://api.example.com/mcp",
      "headers": {
        "x-api-key": "your-api-key"
      }
    }
  }
}

// Windsurf (NOTE: uses "serverUrl" not "url")
{
  "mcpServers": {
    "my-server": {
      "serverUrl": "https://api.example.com/mcp",
      "headers": {
        "x-api-key": "your-api-key"
      }
    }
  }
}
```

### Windows-Specific Config
On Windows, `npx` commands must be wrapped with `cmd /c`:
```json
{
  "mcpServers": {
    "memory": {
      "command": "cmd",
      "args": ["/c", "npx", "-y", "@modelcontextprotocol/server-memory"]
    }
  }
}
```

---

## 4. How AI Desktop Apps Handle MCP Configuration

### Claude Desktop
- **Config method**: File-only (no in-app UI for adding servers)
- **Config file**: 
  - macOS: `~/Library/Application Support/Claude/claude_desktop_config.json`
  - Windows: `%APPDATA%\Claude\claude_desktop_config.json`
- **Format**: JSON with `mcpServers` top-level key
- **Remote server key**: `url`
- **Status indicator**: Hammer icon in chat input area; click to see tool list
- **Restart**: Full restart required after config changes (quit from dock/system tray)

### Cursor
- **Config method**: File-based JSON + Settings UI
- **Config files**:
  - Global: `~/.cursor/mcp.json`
  - Project-specific: `.cursor/mcp.json` at project root
- **UI**: Cursor Settings → Tools & MCP section
- **One-click install**: MCP directory has "Add to Cursor" buttons
- **Remote server key**: `url`
- **Status indicator**: Green dot per server in MCP settings
- **Unique feature**: Dual-config system (global + project-level)

### Windsurf
- **Config method**: File-based JSON + MCP Marketplace
- **Config file**: `~/.codeium/windsurf/mcp_config.json`
- **UI**: MCP Marketplace accessible from Cascade panel (MCPs icon top right)
- **MUST enable MCP first**: Settings → Cascade → Advanced → Enable "Model Context Protocol (MCP)"
- **Remote server key**: `serverUrl` (NOT `url` — critical difference!)
- **One-click install**: Marketplace for browsing and installing without editing config

### Vinkius Desktop (Third-Party Management App)
- **Purpose**: Unified control surface for managing MCP across ALL AI clients
- **Architecture**: Tauri 2 (Rust) + Vue frontend
- **Key features**:
  - Auto-detects all MCP-compatible AI clients installed on system
  - Add/edit/remove server once → propagates to all clients
  - Multi-format output (JSON, YAML, TOML per client)
  - Correct key structure per client (handles `url` vs `serverUrl` etc.)
  - Server matrix view showing which servers are active in which clients
  - Full-text search across 3,400+ servers
  - Capability introspection (inspect tools/prompts before installing)
- **Supported clients**: VS Code, JetBrains, Cursor, Windsurf, Cline, Roo Code, Claude Desktop, Claude Code, Codex, Gemini CLI, and more
- **Install formats**: .msi (Windows), .dmg (macOS), .AppImage/.deb (Linux)

---

## 5. MCP Server Preset/Recommended List Best Practices

### The 80/20 Rule
Research consistently shows **3-5 servers cover 80% of typical developer workflows**. A recommended preset should:

1. **Filesystem** — foundational file access
2. **Memory** — persistent context across sessions
3. **GitHub** — version control + code review
4. **Fetch** — web content retrieval
5. **Sequential Thinking** — structured reasoning

### Preset Design Principles (from MCP Best Practices guide)

| Principle | Application |
|-----------|-------------|
| **Single Responsibility** | Each server should do one thing well |
| **Contracts-First** | Define clear tool interfaces before implementation |
| **Additive Change** | Non-breaking changes only; don't remove tools |
| **Stateless Defaults** | Prefer stateless server designs |
| **Least-Privilege Integrations** | Only request minimum required permissions |
| **Observability from Day One** | Log tool calls, errors, and performance |

### Recommended Preset Categories for Developer-Focused App

| Tier | Category | Servers | Rationale |
|------|----------|---------|-----------|
| **Core** (ship by default) | File System | Filesystem | Essential for any local work |
| **Core** | Knowledge | Memory | Cross-session persistence |
| **Core** | Web | Fetch | URL content retrieval |
| **Core** | Reasoning | Sequential Thinking | Structured problem-solving |
| **Recommended** (one-click enable) | VCS | Git, GitHub | Version control workflows |
| **Recommended** | Search | Brave Search | Web search capability |
| **Recommended** | Database | SQLite, PostgreSQL | Data querying |
| **Recommended** | Browser | Playwright | Web automation/testing |
| **Extended** (manual setup) | Cloud | Cloudflare, AWS | Infrastructure management |
| **Extended** | Communication | Slack, Discord | Team workflows |
| **Extended** | PM | Notion, Linear | Project tracking |

---

## 6. MCP Security Considerations for Desktop Apps

### Threat Landscape (2025-2026)

| Metric | Value |
|--------|-------|
| Total MCP CVEs (Jan-Apr 2026) | 40+ |
| CVE filing rate | ~1 every 4 days |
| Third-party tool downloads affected | 150 million+ |
| MCP marketplaces affected by poisoning | 9 of 11 |
| Estimated vulnerable servers | 200,000 |
| Implementations vulnerable to path traversal | 82% (of 2,614 surveyed) |
| Implementations with injection risk | 67% |
| Servers with SSRF vulnerabilities | 36.7% |
| Servers with no authentication | 41% |
| Servers relying only on static API keys | 53% |
| Servers using OAuth | Only 8.5% |

### Top 10 Vulnerability Categories

| Rank | Category | Share | Description |
|------|----------|-------|-------------|
| 1 | **Shell/exec injection** | 43% | Command execution via unsanitized inputs in STDIO transport |
| 2 | **Tooling infrastructure flaws** | 20% | Weaknesses in MCP tool design and implementation |
| 3 | **Authentication bypass** | 13% | Circumventing auth mechanisms |
| 4 | **Path traversal** | 10% | Directory traversal attacks |
| 5 | **Tool Poisoning** | — | Malicious tool descriptions that manipulate LLM behavior |
| 6 | **SSRF** | — | Server-side request forgery via MCP tools |
| 7 | **Prompt Injection** | — | Injecting instructions via tool responses/descriptions |
| 8 | **Supply chain / Marketplace poisoning** | — | Malicious MCP packages uploaded to marketplaces |
| 9 | **Cross-tenant exposure** | — | Data leakage between different users/organizations |
| 10 | **Zero-click IDE exploitation** | — | Exploiting AI-assisted IDEs without user interaction |

### Root Cause: STDIO Architecture
The fundamental issue is that **MCP uses STDIO as its primary transport without sanitizing spawned command strings**. The subprocess-based architecture makes command execution the default interface, inherited by every implementation.

### Security Checklist for Desktop Apps

#### Pre-Installation
- [ ] **Verify server provenance** — Only install from trusted registries or verified publishers
- [ ] **Check for known CVEs** — Validate against NIST NVD before enabling
- [ ] **Review requested permissions** — Show users what filesystem paths, env vars, and network access a server requires
- [ ] **Sandbox by default** — Run MCP servers in isolated environments (containers, restricted filesystem)

#### At Runtime
- [ ] **Input validation** — Sanitize all inputs before passing to MCP tools
- [ ] **Behavioral monitoring** — Track tool call sequences against baseline patterns to detect data exfiltration
- [ ] **Rate limiting** — Prevent runaway tool calls
- [ ] **User confirmation for dangerous operations** — Use tool annotations (`dangerous: true`, `requiresConfirmation: true`)
- [ ] **Least-privilege file access** — Only grant access to explicitly approved directories
- [ ] **Environment variable isolation** — Don't expose host env vars to MCP servers unless explicitly configured

#### Configuration Security
- [ ] **Encrypt stored credentials** — API keys and tokens should be encrypted at rest, not stored in plaintext JSON
- [ ] **Separate read/write permissions** — Consider splitting servers by permission level (read-only vs. write)
- [ ] **Treat external config as untrusted** — Validate all inputs and configurations from external sources
- [ ] **Block public IP access** — MCP services should not be exposed to the internet

#### Tool Annotation Support
Tools can declare metadata for the host to enforce:
```typescript
server.tool("delete_user", { userId: z.string() }, async (params) => { /* ... */ }, {
  description: "Permanently deletes a user and all associated data",
  dangerous: true,
  requiresConfirmation: true
});
```

### Anthropic's Stance
Anthropic has **declined architectural modifications** to address STDIO security, characterizing the vulnerable behavior as "expected" and placing sanitization responsibility on developers. This means desktop apps must implement their own security layers.

---

## 7. Transport Types Summary

| Transport | Use Case | Config Format | Security |
|-----------|----------|---------------|----------|
| **stdio** | Local development, CLI tools, desktop apps | `command` + `args` + `env` | Process runs locally; inherits user permissions |
| **Streamable HTTP** (formerly SSE) | Remote/cloud deployments | `url` + `headers` | Requires HTTPS; auth via headers |
| **stdio with cmd wrapper** | Windows compatibility | `command: "cmd"`, `args: ["/c", "npx", ...]` | Same as stdio |

**Note for desktop apps**: stdio is the primary and most common transport. Streamable HTTP is used for hosted/remote servers. A desktop app should support both.

---

## 8. Additional Popular MCP Servers by Category

### Official Integrations (Company-Maintained)
| Server | Company | Repo |
|--------|---------|------|
| Axiom | Axiom | `axiomhq/mcp-server-axiom` |
| Browserbase | Browserbase | `browserbase/mcp-server-browserbase` |
| Cloudflare | Cloudflare | `cloudflare/mcp-server-cloudflare` |
| E2B | E2B | `e2b-dev/mcp-server` |
| Neon | Neon | `neondatabase/mcp-server-neon` |
| Obsidian | Community | `calclavia/mcp-obsidian` |
| Qdrant | Qdrant | `qdrant/mcp-server-qdrant` |
| Raygun | MindscapeHQ | `MindscapeHQ/mcp-server-raygun` |
| Tinybird | Tinybird | `tinybirdco/mcp-tinybird` |

### Community Highlights
| Server | Category | Repo |
|--------|----------|------|
| Docker | DevOps | `ckreiling/mcp-server-docker` |
| Kubernetes | DevOps | `Flux159/mcp-server-kubernetes` |
| Linear | PM | `jerhadf/linear-mcp-server` |
| Snowflake | Database | `datawiz168/mcp-snowflake-service` |
| Todoist | Productivity | `abhiz123/todoist-mcp-server` |
| Spotify | Entertainment | `varunneal/spotify-mcp` |

### Enterprise/Business Servers
| Server | Category | Auth |
|--------|----------|------|
| Stripe | Payments | OAuth / API key |
| HubSpot | CRM | OAuth |
| Salesforce | CRM | OAuth |
| Jira | PM | OAuth / API key |
| Zendesk | Support | OAuth / API key |
| Shopify | E-commerce | API key |
| Figma | Design | OAuth |
| Gmail | Email | OAuth |
| Datadog | Monitoring | API key |
| Sentry | Error Tracking | API key |

---

## 9. Key Takeaways for Desktop App Implementation

1. **Support stdio transport first** — it's the dominant pattern; add Streamable HTTP for remote servers
2. **Use the official registry** (registry.modelcontextprotocol.io) as the source of truth for server discovery
3. **Provide a preset list** of 5-8 curated servers for one-click setup (Filesystem, Memory, Fetch, GitHub, Brave Search, Sequential Thinking, Git, Playwright)
4. **Implement a configuration UI** similar to Cursor's "Tools & MCP" section with status indicators
5. **Handle Windows wrapping** — auto-wrap npx commands with `cmd /c` on Windows
6. **Encrypt credentials at rest** — never store API keys in plaintext JSON
7. **Sandbox servers** — restrict filesystem access, validate inputs, monitor behavior
8. **Show permission reviews** — display what each server requires before installation
9. **Support tool annotations** — respect `dangerous` and `requiresConfirmation` flags
10. **Consider Vinkius Desktop's approach** — multi-client config management with format adaptation per client
