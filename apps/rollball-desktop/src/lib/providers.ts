// Provider utility functions - derive UI control fields from provider ID
// All provider business data (id, name, baseUrl, models) comes from Gateway API

/** Whether this provider requires an API key */
export function needsApiKey(providerId: string): boolean {
  return !['ollama', 'lmstudio'].includes(providerId);
}

/** Authentication style for the provider */
export function authStyle(providerId: string): 'bearer' | 'x-api-key' | 'zhipu-jwt' {
  if (providerId === 'zhipuai') return 'zhipu-jwt';
  return 'bearer';
}

/** Placeholder text for API key input field */
export function keyPlaceholder(providerId: string): string {
  const map: Record<string, string> = {
    anthropic: 'sk-ant-...',
    google: 'AIza...',
    zhipuai: 'id.secret',
    groq: 'gsk_...',
    xai: 'xai-...',
    openrouter: 'sk-or-...',
    azure: 'Azure API key...',
  };
  return map[providerId] ?? 'sk-...';
}

/** Whether the base URL is editable (always true) */
export function editableBaseUrl(): boolean {
  return true;
}
