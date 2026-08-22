// Unit tests for `nodeTypeI18n.ts`. These cover the i18n key resolution
// for the secondary sub-filter the panel offers when the primary type is
// Knowledge or Autobiographical.
//
// The tests use a stub `t()` that returns the input key, so we only
// assert the key-lookup behaviour (which is the contract the locale
// files must satisfy). Locale-content checks live in `check:i18n`.

import { describe, it, expect } from "vitest";
import {
  nodeTypeLabel,
  subTypeLabel,
  subTypeOptions,
} from "./nodeTypeI18n";

const identityT = (key: string): string => key;

describe("nodeTypeLabel", () => {
  it("maps every known PascalCase label to its i18n key", () => {
    expect(nodeTypeLabel(identityT, "Knowledge")).toBe(
      "memoryPanel.typeKnowledge",
    );
    expect(nodeTypeLabel(identityT, "Episodic")).toBe(
      "memoryPanel.typeEpisodic",
    );
    expect(nodeTypeLabel(identityT, "Procedural")).toBe(
      "memoryPanel.typeProcedural",
    );
    expect(nodeTypeLabel(identityT, "Autobiographical")).toBe(
      "memoryPanel.typeAutobiographical",
    );
  });

  it("falls back to the raw value for unknown labels", () => {
    expect(nodeTypeLabel(identityT, "FutureLabel")).toBe("FutureLabel");
  });
});

describe("subTypeLabel", () => {
  it("maps every Knowledge sub_type to its i18n key", () => {
    expect(subTypeLabel(identityT, "Knowledge", "Fact")).toBe(
      "memoryPanel.subTypeKnowledgeFact",
    );
    expect(subTypeLabel(identityT, "Knowledge", "Preference")).toBe(
      "memoryPanel.subTypeKnowledgePreference",
    );
    expect(subTypeLabel(identityT, "Knowledge", "Relation")).toBe(
      "memoryPanel.subTypeKnowledgeRelation",
    );
    expect(subTypeLabel(identityT, "Knowledge", "Procedure")).toBe(
      "memoryPanel.subTypeKnowledgeProcedure",
    );
  });

  it("maps every Autobiographical category to its i18n key", () => {
    expect(subTypeLabel(identityT, "Autobiographical", "Identity")).toBe(
      "memoryPanel.subTypeAutobiographicalIdentity",
    );
    expect(subTypeLabel(identityT, "Autobiographical", "Capability")).toBe(
      "memoryPanel.subTypeAutobiographicalCapability",
    );
    expect(subTypeLabel(identityT, "Autobiographical", "Limitation")).toBe(
      "memoryPanel.subTypeAutobiographicalLimitation",
    );
    expect(
      subTypeLabel(identityT, "Autobiographical", "Preference"),
    ).toBe("memoryPanel.subTypeAutobiographicalPreference");
    expect(subTypeLabel(identityT, "Autobiographical", "History")).toBe(
      "memoryPanel.subTypeAutobiographicalHistory",
    );
    expect(subTypeLabel(identityT, "Autobiographical", "Relationship")).toBe(
      "memoryPanel.subTypeAutobiographicalRelationship",
    );
  });

  it("returns the raw sub_type for unsupported (node_type, sub_type) pairs", () => {
    // Episodic never has a sub_type — the lookup must not silently return
    // a key from a different label.
    expect(subTypeLabel(identityT, "Episodic", "Fact")).toBe("Fact");
    // Same string under a label that doesn't carry it.
    expect(subTypeLabel(identityT, "Procedural", "Preference")).toBe(
      "Preference",
    );
  });

  it("falls back to the raw sub_type for unknown categories", () => {
    // Future schema additions must render, not blank out.
    expect(subTypeLabel(identityT, "Knowledge", "NewSubType")).toBe(
      "NewSubType",
    );
  });
});

describe("subTypeOptions", () => {
  it("returns the four Knowledge sub_types in declared order", () => {
    const opts = subTypeOptions(identityT, "Knowledge");
    expect(opts.map((o) => o.value)).toEqual([
      "Fact",
      "Preference",
      "Relation",
      "Procedure",
    ]);
    // Each value should be paired with its i18n key as the label.
    expect(opts[0].label).toBe("memoryPanel.subTypeKnowledgeFact");
    expect(opts[3].label).toBe("memoryPanel.subTypeKnowledgeProcedure");
  });

  it("returns the six Autobiographical categories in declared order", () => {
    const opts = subTypeOptions(identityT, "Autobiographical");
    expect(opts.map((o) => o.value)).toEqual([
      "Identity",
      "Capability",
      "Limitation",
      "Preference",
      "History",
      "Relationship",
    ]);
    expect(opts[5].label).toBe(
      "memoryPanel.subTypeAutobiographicalRelationship",
    );
  });

  it("returns an empty list for labels without sub-classification", () => {
    expect(subTypeOptions(identityT, "Episodic")).toEqual([]);
    expect(subTypeOptions(identityT, "Procedural")).toEqual([]);
    expect(subTypeOptions(identityT, "UnknownLabel")).toEqual([]);
  });
});