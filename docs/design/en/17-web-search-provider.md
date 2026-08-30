# Web Search Provider Design

> Version: v1.0 | Last Updated: 2026-04-18

---

The `web_search` tool enables Agents to query web search engines. This document specifies how Agent Runtime integrates with multiple search providers, configuration management, and result processing.

## 1. Design Goals

- **Multi-provider support**: Allow users to choose search engine (Brave, SerpAPI, Tavily, etc.)
- **API Key isolation**: Each provider's Key independently stored in Vault, not exposed in manifest
- **Configuration-driven Opt-In**: Only when manifest declares `web_search` is search enabled
- **Hot config update**: Search provider config changes take effect via Gateway → Runtime push, no Agent restart
- **Graceful degradation**: Provider unavailable, fall back to other providers or return empty result

## 2. Supported Providers

Phase 1 supports the following providers (sorted by recommendation):

| Provider | Type | Cost | Quality | Notes |
|----------|------|------|---------|-------|
| Brave Search | API | Pay-per-query | High | Recommended, privacy-focused |
| SerpAPI | API | Pay-per-query | High | Aggregates multiple engines |
| Tavily | API | Pay-per-query | Medium-High | Optimized for AI agents |
| DuckDuckGo | Scraping | Free | Medium | No Key required, unstable |

Provider plugins follow unified interface, new providers added via Phase 4+ plugin mechanism.

## 3. Tool Declaration

```toml
[[tools]]
name = "web_search"
type = "builtin"

[tools.web_search.config]
provider = "brave"        # Default provider
max_results = 5            # Max results per query
timeout_ms = 10000         # HTTP timeout
fallback_providers = ["tavily", "duckduckgo"]  # Fallback chain
```

**Field descriptions:**

| Field | Required | Description |
|-------|----------|-------------|
| `provider` | No | Default provider name, defaults to Gateway-configured default |
| `max_results` | No | Max results per query, default 5 |
| `timeout_ms` | No | HTTP timeout, default 10000ms |
| `fallback_providers` | No | Fallback provider chain, tried in order |

## 4. Vault Integration

Each provider's API Key is stored in Vault:

```
~/.config/agent-gateway/vault/
├── search_brave_key.enc
├── search_serpapi_key.enc
├── search_tavily_key.enc
└── ...
```

Key naming convention: `search_{provider}_key`.

Runtime obtains Keys from Vault during handshake:

```protobuf
message SearchProviderConfig {
    string provider = 1;          // "brave" / "serpapi" / "tavily"
    string endpoint = 2;          // Provider API URL
    string auth_ref = 3;          // Vault key reference, e.g. "vault:search_brave_key"
    string auth_type = 4;         // "api_key" / "bearer" / "none"
    uint32 max_results = 5;
    bool enabled = 6;             // Whether enabled
}
```

Runtime fetches Key from Vault at request time, doesn't cache in process memory (reduce Key exposure risk).

## 5. Search Flow

```
LLM outputs tool_call:
  { name: "web_search", arguments: { query: "Rust async runtime comparison", top_k: 5 } }
       │
       ▼
Runtime: Tool Dispatcher parses
       │
       ├─ ① Check provider config (from AgentHelloResult.search_providers)
       │
       ├─ ② Try primary provider (e.g. "brave")
       │   ├─ From Vault get search_brave_key
       │   ├─ Construct HTTP request: GET https://api.search.brave.com/v1/web/search?q=...&key=...
       │   ├─ Send request (timeout 10s)
       │   ├─ Parse response
       │   └─ Success → return results
       │
       ├─ ③ Primary fails → try fallback_providers[0] (e.g. "tavily")
       │   ├─ Same flow
       │   └─ Success → return results
       │
       ├─ ④ All fail → return empty result with error
       │
       └─ ⑤ All providers fail → return empty results array, log warning
       │
       ▼
Construct tool_result:
  {
    "results": [
      {
        "title": "...",
        "url": "...",
        "snippet": "...",
        "published_date": "...",
        "source": "brave"
      },
      ...
    ],
    "provider_used": "brave",
    "total_results": 5
  }
```

## 6. Provider Configuration Management

### 6.1 Gateway Configuration

Gateway manages provider configurations in `search_providers.json`:

```json
{
  "version": 1,
  "providers": [
    {
      "provider": "brave",
      "endpoint": "https://api.search.brave.com/v1/web/search",
      "auth_ref": "vault:search_brave_key",
      "auth_type": "api_key",
      "max_results": 10,
      "enabled": true
    },
    {
      "provider": "serpapi",
      "endpoint": "https://serpapi.com/search",
      "auth_ref": "vault:search_serpapi_key",
      "auth_type": "api_key",
      "max_results": 10,
      "enabled": false
    },
    ...
  ],
  "default_provider": "brave"
}
```

### 6.2 HTTP API

```bash
# List all providers
GET /api/search/providers

# Add provider
POST /api/search/providers
Body: { "provider": "brave", "endpoint": "...", "auth_ref": "vault:...", ... }

# Update provider
PUT /api/search/providers/{provider}
Body: { "enabled": true, ... }

# Delete provider
DELETE /api/search/providers/{provider}

# Set default
POST /api/search/providers/{provider}/default
```

### 6.3 AgentHello Push

After Gateway `search_providers.json` changes, version increments. Next AgentHello detects version mismatch, Gateway pushes full provider list in `AgentHelloResult.search_providers`.

If Agent is already running, Gateway publishes `RuntimeConfigUpdate` via MQTT, Runtime hot-reloads search config.

## 7. Result Format

Search results uniformly use the following format (normalize differences between providers):

```typescript
interface SearchResult {
    title: string;
    url: string;
    snippet: string;           // text snippet
    published_date?: string;   // publish date (if available)
    score?: number;            // relevance score (if provided by provider)
    source: string;            // provider name (which engine returned)
}

interface SearchResults {
    results: SearchResult[];
    provider_used: string;     // which provider actually returned results
    total_results: number;     // total result count
    query: string;             // original query
}
```

LLM receives this normalized format, can uniformly process regardless of which provider returned.

## 8. Caching and Rate Limiting

### 8.1 Result Caching

Runtime maintains simple in-memory LRU cache (max 1000 entries, TTL 5 min):

```
Key: (query, provider)
Value: SearchResults

Cache hit → return directly, no HTTP call
Cache miss → HTTP call, cache result
```

Cache purposes:
- Avoid duplicate queries in same session (LLM may call same query multiple times)
- Reduce API costs
- Speed up response

### 8.2 Rate Limit Coordination

Runtime reports each `web_search` call's token usage (treated as fixed cost) to Gateway via UsageReport. Gateway Rate Limiter tracks per-Agent rate to avoid exceeding provider's RPM limit.

```
LLM → Runtime: web_search call
       │
       ▼
Runtime → Gateway: RateAcquire { provider: "brave", estimated_queries: 1 }
       │
       ▼ Gateway: check Brave Search rate limit
       ├─ Granted → Runtime proceeds with call
       └─ Limited → Runtime waits, retries
```

## 9. Privacy and Security

### 9.1 Query Privacy

Search queries may contain sensitive info (user's personal questions). Considerations:

- Queries sent over HTTPS to provider (provider-side privacy protection)
- Don't log full query text in Gateway logs (only log query hash)
- Don't cache results containing sensitive info (LLM can decide based on context)

### 9.2 Result Trust

Search results come from external sources, may contain:
- Misinformation
- Malicious links
- Phishing sites

LLM should treat search results as untrusted input, just like web_fetch results.

## 10. Cost Tracking

Each `web_search` call's cost depends on provider pricing. Runtime estimates cost based on provider configuration:

```rust
fn estimate_search_cost(provider: &str, results_count: u32) -> f64 {
    match provider {
        "brave" => results_count as f64 * 0.005,      // $0.005 per query (Brave pricing)
        "serpapi" => results_count as f64 * 0.01,     // SerpAPI pricing
        "tavily" => results_count as f64 * 0.008,     // Tavily pricing
        "duckduckgo" => 0.0,                          // free
        _ => 0.0,
    }
}
```

Cost reported to Gateway via UsageReport, aggregated in Budget Tracker.

## 11. Fallback Strategy

When primary provider fails (timeout, 5xx, Key invalid), Runtime tries fallback providers in order:

```
Primary fails → fallback[0]
       │
       ├─ Success → return results, log info
       │
       └─ Fail → fallback[1]
              │
              ├─ Success → return results
              │
              └─ Fail → ...all fail
                     │
                     └─ Return empty results array
                        (LLM decides next step based on empty results)
```

Don't silently fallback without informing LLM — include `provider_used` in result so LLM knows which engine returned.

## 12. Design Decision Records

| Decision | Choice | Reason |
|----------|--------|--------|
| Multi-provider support | Plugin-based, multiple providers configured simultaneously | User choice, avoid single point of failure |
| API Key storage | Vault unified management | Consistent with LLM Key management |
| Default provider | Configurable per Agent | Different Agents have different needs |
| Fallback chain | Manifest declaration | Explicit configuration, LLM can know which providers are tried |
| Result caching | Runtime LRU cache | Avoid duplicate queries, reduce cost |
| Cost tracking | Estimate per call | Provider pricing varies, simplified estimation |
| Query privacy | Don't log full text | User privacy protection |
| Result trust | Treat as untrusted | LLM responsible for verification |

## 13. Cross-references

| Document | Relationship |
|----------|-------------|
| [03-agent-runtime.md](./03-agent-runtime.md) | Tool dispatcher routes `web_search` to this provider |
| [04-gateway.md](./04-gateway.md) | Gateway manages `search_providers.json` and Vault Keys |
| [12-tool-system.md](./12-tool-system.md) | `web_search` is a Built-in Tool |
| [06-communication.md](./06-communication.md) | AgentHello pushes `search_providers` |
| [18-user-identity-simplified.md](./18-user-identity-simplified.md) | Same ResourceCache + version-driven diff sync pattern |