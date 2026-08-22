// Maps the raw `node_type` string returned by the backend (grafeo node
// label, PascalCase) to the i18n key used by the type filter dropdown.
// Keep this in sync with the dropdown options in MemoryPanel.tsx so the
// badge label and the dropdown option stay aligned.
//
// Falls back to the raw value for unknown types so a future backend label
// does not render as a blank badge.
import { useTranslation } from "../../i18n/useTranslation";

const NODE_TYPE_I18N_KEYS: Record<string, string> = {
  Knowledge: "memoryPanel.typeKnowledge",
  Episodic: "memoryPanel.typeEpisodic",
  Procedural: "memoryPanel.typeProcedural",
  Autobiographical: "memoryPanel.typeAutobiographical",
};

export function nodeTypeLabel(t: (key: string) => string, nodeType: string): string {
  const key = NODE_TYPE_I18N_KEYS[nodeType];
  return key ? t(key) : nodeType;
}

// Convenience hook that returns a ready-to-call label function.
export function useNodeTypeLabel(): (nodeType: string) => string {
  const { t } = useTranslation();
  return (nodeType: string) => nodeTypeLabel(t, nodeType);
}

// ── Sub-classification (secondary filter) ────────────────────────────
//
// `sub_type` is the string returned by the backend inside a Knowledge or
// Autobiographical node. It is *context-free* — the same string can
// appear under different `node_type`s with different meanings (e.g.
// "Preference" is a Knowledge sub_type AND an Autobiographical category).
// The panel therefore passes the parent `nodeType` so the i18n lookup
// stays unambiguous.
//
// Knowledge:         `Fact` | `Preference` | `Relation` | `Procedure`
// Autobiographical:  `Identity` | `Capability` | `Limitation`
//                    | `Preference` | `History` | `Relationship`

const SUB_TYPE_I18N_KEYS: Record<string, Record<string, string>> = {
  Knowledge: {
    Fact: "memoryPanel.subTypeKnowledgeFact",
    Preference: "memoryPanel.subTypeKnowledgePreference",
    Relation: "memoryPanel.subTypeKnowledgeRelation",
    Procedure: "memoryPanel.subTypeKnowledgeProcedure",
  },
  Autobiographical: {
    Identity: "memoryPanel.subTypeAutobiographicalIdentity",
    Capability: "memoryPanel.subTypeAutobiographicalCapability",
    Limitation: "memoryPanel.subTypeAutobiographicalLimitation",
    Preference: "memoryPanel.subTypeAutobiographicalPreference",
    History: "memoryPanel.subTypeAutobiographicalHistory",
    Relationship: "memoryPanel.subTypeAutobiographicalRelationship",
  },
};

/**
 * Resolve a `sub_type` string to its display label, given the parent
 * `node_type`. Returns the raw sub_type when no mapping exists, so a
 * future backend addition does not render as a blank badge.
 */
export function subTypeLabel(
  t: (key: string) => string,
  nodeType: string,
  subType: string,
): string {
  const key = SUB_TYPE_I18N_KEYS[nodeType]?.[subType];
  return key ? t(key) : subType;
}

/**
 * Return the list of selectable sub-filter options for a given
 * `node_type`. The order matches the enum declaration order in
 * `acowork-grafeo::types` so the dropdown stays stable across locales.
 */
export function subTypeOptions(
  t: (key: string) => string,
  nodeType: string,
): Array<{ value: string; label: string }> {
  const map = SUB_TYPE_I18N_KEYS[nodeType];
  if (!map) return [];
  return Object.entries(map).map(([value, key]) => ({
    value,
    label: t(key),
  }));
}

export function useSubTypeLabel(): (nodeType: string, subType: string) => string {
  const { t } = useTranslation();
  return (nodeType: string, subType: string) => subTypeLabel(t, nodeType, subType);
}
