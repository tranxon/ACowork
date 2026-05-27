// Static catalog of available web search providers.
//
// Unlike LLM providers (which use models.dev for dynamic registration),
// search provider metadata is small and stable, so we use a static list.

import type { SearchProviderDef } from "./types";

export const SEARCH_PROVIDERS: SearchProviderDef[] = [
  {
    id: "tavily",
    name: "Tavily Search",
    description: "AI-optimized real-time search API built for AI agents",
    requires_api_key: true,
    free_quota: "1,000 queries/month",
    base_url: "https://api.tavily.com",
  },
  {
    id: "brave",
    name: "Brave Search",
    description: "Privacy-first web search with independent index",
    requires_api_key: true,
    free_quota: "2,000 queries/month",
    base_url: "https://api.search.brave.com",
  },
  {
    id: "serper",
    name: "Serper.dev",
    description: "Fast Google Search API with structured results",
    requires_api_key: true,
    free_quota: "2,500 queries/month",
    base_url: "https://google.serper.dev",
  },
  {
    id: "perplexity",
    name: "Perplexity Sonar",
    description: "AI-powered search with inline citations and answers",
    requires_api_key: true,
    free_quota: "Pay-as-you-go",
    base_url: "https://api.perplexity.ai",
  },
  {
    id: "exa",
    name: "Exa.ai",
    description: "AI search engine with extracted web content for LLMs",
    requires_api_key: true,
    free_quota: "100 searches/month",
    base_url: "https://api.exa.ai",
  },
  {
    id: "google-cse",
    name: "Google CSE",
    description: "Google Custom Search Engine — requires API key + Search Engine ID (CX)",
    requires_api_key: true,
    free_quota: "100 queries/day",
    base_url: "https://www.googleapis.com",
  },
  {
    id: "firecrawl",
    name: "Firecrawl",
    description: "Web scraping and search with markdown output",
    requires_api_key: true,
    free_quota: "500 credits/month",
    base_url: "https://api.firecrawl.dev",
  },
  {
    id: "searxng",
    name: "SearXNG",
    description: "Self-hosted privacy-respecting metasearch engine",
    requires_api_key: false,
    free_quota: "Unlimited (self-hosted)",
    base_url: "",
  },
];

/** Look up a provider's static metadata by ID */
export function lookupSearchProvider(id: string): SearchProviderDef | undefined {
  return SEARCH_PROVIDERS.find((p) => p.id === id);
}

/** Get API key placeholder text for a search provider */
export function searchKeyPlaceholder(providerId: string): string {
  const map: Record<string, string> = {
    tavily: "tvly-...",
    brave: "BSA-...",
    serper: "Enter Serper API key...",
    perplexity: "pplx-...",
    exa: "Enter Exa API key...",
    "google-cse": "Enter Google API key...",
    firecrawl: "fc-...",
    searxng: "Enter SearXNG host URL (e.g. http://localhost:8888)",
  };
  return map[providerId] ?? "Enter API key...";
}
