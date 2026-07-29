import { describe, it, expect } from "vitest";
import { sessionConfigToPatch } from "./sessionConfigMapper";

// These tests pin down the single source of truth for the HTTP `fetchSessionConfig`
// path and the MQTT `session_config` retained-event path. They previously
// existed as duplicated, hand-rolled copies of the production mapping inside
// chatStore.test.ts — which is exactly how the two implementations drifted
// and silently hid the reasoning-effort toggle button. If you find yourself
// adding a "and what about clearOnNull: false" duplicate, add a new case
// here instead.

describe("sessionConfigToPatch", () => {
  // ── HTTP path (clearOnNull: true) ──────────────────────────────
  // ADR-047 P1: HTTP fetchSessionConfig runs on cold-load / session-switch.
  // A null field MUST wipe the previous session's value from the UI,
  // otherwise stale config from the prior session leaks through until the
  // next retained MQTT push.
  describe("clearOnNull: true (HTTP fetchSessionConfig)", () => {
    it("sets model and provider when both are non-empty strings", () => {
      const patch = sessionConfigToPatch(
        {
          model: "claude-3",
          provider: "anthropic",
          reasoning_effort: "high",
          temperature: 0.5,
        },
        { clearOnNull: true },
      );
      expect(patch).toEqual({
        model: "claude-3",
        provider: "anthropic",
        reasoningEffort: "high",
        temperature: 0.5,
      });
    });

    it("clears model and provider to null when backend returns null", () => {
      const patch = sessionConfigToPatch(
        {
          model: null,
          provider: null,
          reasoning_effort: null,
          temperature: null,
        },
        { clearOnNull: true },
      );
      expect(patch.model).toBeNull();
      expect(patch.provider).toBeNull();
      expect(patch.temperature).toBeNull();
    });

    it("treats empty-string model and provider as null and clears them", () => {
      const patch = sessionConfigToPatch(
        { model: "", provider: "", reasoning_effort: "", temperature: 0 },
        { clearOnNull: true },
      );
      // "" fails `typeof === "string" && truthy`, falls into clearOnNull branch.
      expect(patch.model).toBeNull();
      expect(patch.provider).toBeNull();
      expect(patch.temperature).toBe(0); // 0 is a valid temperature
    });

    it("treats NaN temperature as null and clears it", () => {
      const patch = sessionConfigToPatch(
        { temperature: NaN },
        { clearOnNull: true },
      );
      expect(patch.temperature).toBeNull();
    });
  });

  // ── The headline test: the bug that motivated this refactor ──
  // Before refactor, `fetchSessionConfig` cleared reasoning_effort to null
  // on every cold load, which made ChatPanel hide the toggle button (its
  // visibility condition is `currentReasoningEffort != null`). The toggle
  // is gated on model capability, not on whether the backend has emitted
  // an explicit value yet — so this rule is independent of clearOnNull.
  describe("reasoning_effort preserve-on-null (both modes)", () => {
    it("does NOT clear reasoningEffort to null when clearOnNull: true", () => {
      // The whole point: HTTP cold-load on a fresh session (no
      // reasoning_effort yet) must NOT hide the toggle button.
      const patch = sessionConfigToPatch(
        { model: "claude-3", provider: "anthropic", reasoning_effort: null },
        { clearOnNull: true },
      );
      expect("reasoningEffort" in patch).toBe(false);
      expect(patch.reasoningEffort).toBeUndefined();
      // But model/provider ARE cleared-as-set when present:
      expect(patch.model).toBe("claude-3");
      expect(patch.provider).toBe("anthropic");
    });

    it("does NOT clear reasoningEffort to null when clearOnNull: false", () => {
      const patch = sessionConfigToPatch(
        { reasoning_effort: null },
        { clearOnNull: false },
      );
      expect("reasoningEffort" in patch).toBe(false);
    });

    it("sets reasoningEffort only when a non-empty string is present", () => {
      const patch = sessionConfigToPatch(
        { reasoning_effort: "medium" },
        { clearOnNull: true },
      );
      expect(patch.reasoningEffort).toBe("medium");
    });

    it("does not set reasoningEffort on empty string", () => {
      const patch = sessionConfigToPatch(
        { reasoning_effort: "" },
        { clearOnNull: true },
      );
      expect("reasoningEffort" in patch).toBe(false);
    });

    it("does not set reasoningEffort on undefined", () => {
      const patch = sessionConfigToPatch({}, { clearOnNull: true });
      expect("reasoningEffort" in patch).toBe(false);
    });
  });

  // ── MQTT path (clearOnNull: false) ─────────────────────────────
  // The retained `session_config` envelope only carries fields the Runtime
  // explicitly emitted. An absent field means "not in this payload", which
  // must NOT clobber the existing UI value — a reasoning_effort change
  // shouldn't blank the model.
  describe("clearOnNull: false (MQTT session_config)", () => {
    it("only emits fields that were present in the payload", () => {
      const patch = sessionConfigToPatch(
        { model: "gpt-4o", provider: "openai" },
        { clearOnNull: false },
      );
      expect(patch).toEqual({ model: "gpt-4o", provider: "openai" });
      expect("reasoningEffort" in patch).toBe(false);
      expect("temperature" in patch).toBe(false);
    });

    it("ignores null fields entirely (does not set them to null)", () => {
      const patch = sessionConfigToPatch(
        { model: null, provider: null, reasoning_effort: null, temperature: null },
        { clearOnNull: false },
      );
      expect(patch).toEqual({});
    });

    it("propagates reasoning_effort change without touching model", () => {
      // Simulates Runtime emitting a partial update (only reasoning_effort).
      const patch = sessionConfigToPatch(
        { reasoning_effort: "high" },
        { clearOnNull: false },
      );
      expect(patch).toEqual({ reasoningEffort: "high" });
    });

    it("treats NaN temperature as absent (prost 'no override' sentinel)", () => {
      const patch = sessionConfigToPatch(
        { model: "gpt-4o", provider: "openai", temperature: NaN },
        { clearOnNull: false },
      );
      expect(patch).toEqual({ model: "gpt-4o", provider: "openai" });
    });
  });

  // ── Idempotence / cross-mode sanity ───────────────────────────
  it("returns the same patch shape for HTTP and MQTT when values are equal", () => {
    const config = {
      model: "claude-3",
      provider: "anthropic",
      reasoning_effort: "high",
      temperature: 0.7,
    };
    const httpPatch = sessionConfigToPatch(config, { clearOnNull: true });
    // MQTT caller normalizes "" → null before calling.
    const mqttPatch = sessionConfigToPatch(
      { ...config },
      { clearOnNull: false },
    );
    expect(httpPatch).toEqual(mqttPatch);
  });

  it("returns an empty patch when given empty input (clearOnNull: true)", () => {
    // clearOnNull: true means "absent values should be propagated as
    // explicit null" — so an empty input DOES produce {model: null,
    // provider: null, temperature: null}. reasoning_effort is the
    // exception (preserve-on-null, never cleared).
    expect(sessionConfigToPatch({}, { clearOnNull: true })).toEqual({
      model: null,
      provider: null,
      temperature: null,
    });
  });

  it("returns an empty patch when given empty input (clearOnNull: false)", () => {
    expect(sessionConfigToPatch({}, { clearOnNull: false })).toEqual({});
  });
});