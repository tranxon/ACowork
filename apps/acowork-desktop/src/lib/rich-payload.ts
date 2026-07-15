// ADR-034 Phase 5: Rich chat payload type for params_json.
// Provides a shared TypeScript interface to prevent frontend/backend schema drift.

export interface RichChatPayload {
  document_ids?: string[];
  content_parts?: Array<{
    type: "text" | "image_url";
    text?: string;
    image_url?: { url: string; width?: number; height?: number };
  }>;
  attached_context?: Array<{
    absPath: string;
    type: "file" | "selection";
    startLine?: number;
    endLine?: number;
  }>;
}
