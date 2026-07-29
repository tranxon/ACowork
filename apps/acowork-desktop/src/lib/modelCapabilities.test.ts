import { describe, it, expect } from "vitest";
import { resolveDefaultReasoningEffort } from "./modelCapabilities";
import type { ModelEntry } from "./types";

// resolveDefaultReasoningEffort is the single source of truth used by both
// `setCurrentModel` (user picks a model from the dropdown) and the MQTT
// `model_confirmed` handler (Runtime confirms a model switch). Previously
// each call site did its own `find + ?? null` one-liner — easy to drift.
// These tests pin down the lookup rules so the two paths stay in sync.

const MODELS: ModelEntry[] = [
  {
    name: "claude-opus-4-7",
    provider: "anthropic",
    reasoning: true,
    default_reasoning_effort: "medium",
  },
  {
    name: "claude-3-5-sonnet",
    provider: "anthropic",
    reasoning: true,
    default_reasoning_effort: "high",
  },
  {
    name: "claude-3-5-sonnet",
    provider: "bedrock",
    reasoning: true,
    default_reasoning_effort: "low",
  },
  {
    name: "gpt-4o",
    provider: "openai",
    reasoning: false,
    // No default_reasoning_effort -- model doesn't support reasoning.
  },
];

describe("resolveDefaultReasoningEffort", () => {
  it("returns the entry's default_reasoning_effort when present", () => {
    expect(
      resolveDefaultReasoningEffort(MODELS, "claude-opus-4-7", "anthropic"),
    ).toBe("medium");
    expect(
      resolveDefaultReasoningEffort(MODELS, "claude-3-5-sonnet", "anthropic"),
    ).toBe("high");
  });

  it("disambiguates by provider when the same model name exists under multiple providers", () => {
    expect(
      resolveDefaultReasoningEffort(MODELS, "claude-3-5-sonnet", "bedrock"),
    ).toBe("low");
    // Sanity check: the anthropic variant should NOT leak into the bedrock
    // lookup. This is the exact reason lookup goes through name + provider
    // (not name alone).
    expect(
      resolveDefaultReasoningEffort(MODELS, "claude-3-5-sonnet", "anthropic"),
    ).not.toBe("low");
  });

  it("returns null when the model is not in the list", () => {
    expect(
      resolveDefaultReasoningEffort(MODELS, "gpt-99-turbo", "openai"),
    ).toBeNull();
  });

  it("returns null when the model is in the list but has no default_reasoning_effort", () => {
    // gpt-4o is in MODELS but the entry has no default_reasoning_effort.
    // The toggle button visibility check in ChatPanel treats null as
    // "don't show the button", which is correct for a model that
    // doesn't support reasoning.
    expect(
      resolveDefaultReasoningEffort(MODELS, "gpt-4o", "openai"),
    ).toBeNull();
  });

  it("returns null when the model exists but provider is wrong", () => {
    // claude-opus-4-7 is registered under anthropic, not openai.
    expect(
      resolveDefaultReasoningEffort(MODELS, "claude-opus-4-7", "openai"),
    ).toBeNull();
  });

  it("handles an empty model list gracefully", () => {
    expect(resolveDefaultReasoningEffort([], "claude-opus-4-7", "anthropic"))
      .toBeNull();
  });

  it("treats empty-string provider as a real value (does not match missing entries)", () => {
    // Regression guard: the MQTT model_confirmed handler used to pass
    // `confirmedProvider ?? ""`. resolveDefaultReasoningEffort must
    // NOT fall back to the first matching-name entry when provider is
    // "" — it should return null.
    expect(
      resolveDefaultReasoningEffort(MODELS, "claude-opus-4-7", ""),
    ).toBeNull();
  });
});