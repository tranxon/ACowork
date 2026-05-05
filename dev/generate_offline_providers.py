#!/usr/bin/env python3
"""Generate offline_providers.json from ref-repo/providers.json.

Only keeps top N providers (full field structure preserved).
"""
import json
import sys
from pathlib import Path

# Paths
SCRIPT_DIR = Path(__file__).parent
ROOT_DIR = SCRIPT_DIR.parent
SOURCE = ROOT_DIR / "ref-repo" / "providers.json"
OUTPUT = ROOT_DIR / "core" / "rollball-gateway" / "src" / "http" / "offline_providers.json"

# Top 20 whitelist
TOP_PROVIDERS = [
    "openai", "anthropic", "google", "deepseek", "xai", "groq", "mistral",
    "minimax", "minimax-cn", "alibaba", "alibaba-cn", "zhipuai", "zhipuai-cn",
    "moonshotai", "moonshotai-cn", "openrouter", "ollama", "lmstudio", "azure", "together"
]


def main():
    # Read full providers.json
    with open(SOURCE, "r", encoding="utf-8") as f:
        full_data = json.load(f)

    # Only keep whitelisted providers, preserving full structure
    offline = {}
    for provider_id in TOP_PROVIDERS:
        if provider_id in full_data:
            offline[provider_id] = full_data[provider_id]
        else:
            print(f"WARNING: provider '{provider_id}' not found in source", file=sys.stderr)

    # Write output
    with open(OUTPUT, "w", encoding="utf-8") as f:
        json.dump(offline, f, indent=2, ensure_ascii=False)

    # Stats
    model_count = sum(len(p.get("models", {})) for p in offline.values())
    file_size = OUTPUT.stat().st_size
    print(f"Generated {OUTPUT.name}: {len(offline)} providers, {model_count} models, {file_size/1024:.1f} KB")


if __name__ == "__main__":
    main()
