import { describe, it, expect } from "vitest";
import { llmAvailabilityFromWire, type LlmAvailability } from "./llmAvailability";

describe("llmAvailabilityFromWire", () => {
  it("maps protobuf integer tags to frontend projection", () => {
    const cases: Array<[number, LlmAvailability]> = [
      [0, "unspecified"],
      [1, "loading"],
      [2, "configured"],
      [3, "missing"],
    ];
    for (const [wire, expected] of cases) {
      expect(llmAvailabilityFromWire(wire)).toBe(expected);
    }
  });

  it("accepts enum string names from JSON-decoded payloads", () => {
    expect(llmAvailabilityFromWire("LLM_AVAILABILITY_LOADING")).toBe("loading");
    expect(llmAvailabilityFromWire("LLM_AVAILABILITY_MISSING")).toBe("missing");
    expect(llmAvailabilityFromWire("LLM_AVAILABILITY_CONFIGURED")).toBe("configured");
    expect(llmAvailabilityFromWire("LLM_AVAILABILITY_UNSPECIFIED")).toBe("unspecified");
  });

  it("collapses unknown / nullish payloads to unspecified", () => {
    // The previous boolean check `keys.length > 0` could never express
    // "not yet synced" — any unmappable value must default to
    // unspecified, which renders NO banner, NOT the red one.
    expect(llmAvailabilityFromWire(undefined)).toBe("unspecified");
    expect(llmAvailabilityFromWire(null)).toBe("unspecified");
    expect(llmAvailabilityFromWire(99)).toBe("unspecified");
    expect(llmAvailabilityFromWire("garbage")).toBe("unspecified");
  });
});