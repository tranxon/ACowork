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
