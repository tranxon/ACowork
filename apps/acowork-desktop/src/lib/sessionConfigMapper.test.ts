import { describe, it, expect } from "vitest";
import { sessionConfigToPatch } from "./sessionConfigMapper";

// These tests pin down the single source of truth for the HTTP
// `fetchSessionConfig` path and the MQTT `session_config` retained-event
// path. Both paths use `clearOnNull: true` because both deliver a full
// snapshot of the session config - an absent/null field means "the
// session has no value" and must clear any stale UI value.
//
// Design principle: backend is the source of truth. The frontend uses
// the backend value directly - no preserve-on-null, no caching, no state
// stitching.

describe("sessionConfigToPatch", () => {
  // -- Both paths use clearOnNull: true (full snapshot) --

  describe("clearOnNull: true", () => {
    it("sets all fields when all are present", () => {
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

    it("clears all fields to null when backend returns null", () => {
      const patch = sessionConfigToPatch(
        {
          model: null,
          provider: null,
          reasoning_effort: null,
          temperature: null,
        },
        { clearOnNull: true },
      );
      expect(patch).toEqual({
        model: null,
        provider: null,
        reasoningEffort: null,
        temperature: null,
      });
    });

    it("treats empty-string fields as null and clears them", () => {
      const patch = sessionConfigToPatch(
        { model: "", provider: "", reasoning_effort: "", temperature: 0 },
        { clearOnNull: true },
      );
      expect(patch.model).toBeNull();
      expect(patch.provider).toBeNull();
      expect(patch.reasoningEffort).toBeNull();
      expect(patch.temperature).toBe(0); // 0 is a valid temperature
    });

    it("treats NaN temperature as null and clears it", () => {
      const patch = sessionConfigToPatch(
        { temperature: NaN },
        { clearOnNull: true },
      );
      expect(patch.temperature).toBeNull();
    });

    it("clears reasoningEffort to null when backend returns null", () => {
      // Backend sends null when model doesn't support reasoning.
      // Frontend must clear any stale value from a previous model.
      const patch = sessionConfigToPatch(
        { model: "glm-5.2", provider: "volcengine", reasoning_effort: null },
        { clearOnNull: true },
      );
      expect(patch.reasoningEffort).toBeNull();
      expect(patch.model).toBe("glm-5.2");
      expect(patch.provider).toBe("volcengine");
    });

    it("clears reasoningEffort to null on empty string (proto sentinel)", () => {
      // prost encodes Option<String> as "" for None. The MQTT caller
      // converts "" to null before calling, but verify the mapper
      // handles it correctly via clearOnNull.
      const patch = sessionConfigToPatch(
        { reasoning_effort: "" },
        { clearOnNull: true },
      );
      expect(patch.reasoningEffort).toBeNull();
    });

    it("sets reasoningEffort when a non-empty string is present", () => {
      const patch = sessionConfigToPatch(
        { reasoning_effort: "medium" },
        { clearOnNull: true },
      );
      expect(patch.reasoningEffort).toBe("medium");
    });

    it("clears all fields for empty input", () => {
      expect(sessionConfigToPatch({}, { clearOnNull: true })).toEqual({
        model: null,
        provider: null,
        reasoningEffort: null,
        temperature: null,
      });
    });
  });

  describe("clearOnNull: false (not used by current callers, tested for completeness)", () => {
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
      const patch = sessionConfigToPatch(
        { reasoning_effort: "high" },
        { clearOnNull: false },
      );
      expect(patch).toEqual({ reasoningEffort: "high" });
    });

    it("returns an empty patch for empty input", () => {
      expect(sessionConfigToPatch({}, { clearOnNull: false })).toEqual({});
    });
  });

  // -- Cross-mode sanity --
  it("returns the same patch shape for HTTP and MQTT when values are equal", () => {
    const config = {
      model: "claude-3",
      provider: "anthropic",
      reasoning_effort: "high",
      temperature: 0.7,
    };
    const httpPatch = sessionConfigToPatch(config, { clearOnNull: true });
    const mqttPatch = sessionConfigToPatch(
      { ...config },
      { clearOnNull: true },
    );
    expect(httpPatch).toEqual(mqttPatch);
  });
});
