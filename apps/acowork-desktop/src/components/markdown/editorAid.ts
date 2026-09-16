/**
 * editorAid — Monaco editor integration for markdown editing aids.
 *
 * Contains the pieces that bind the pure helpers (`./tableAid`) to a live
 * Monaco editor, shared by the doc editor (DocEditor) and the workspace file
 * editor (FileEditorPanel) so .md files get the same table navigation.
 *
 * The registration returns a dispose function; in practice Monaco disposes
 * commands when the editor instance is disposed, but callers that mount the
 * editor once and reuse it across models (FileEditorPanel) should keep the
 * handle for symmetry.
 */

import type { editor } from "monaco-editor";
import { nextCell } from "./tableAid";

export interface MarkdownTableNavOptions {
  /**
   * When set, navigation only activates while the ACTIVE MODEL's language id
   * matches (e.g. "markdown"). Use this for editors that host one Monaco
   * instance across many file types (FileEditorPanel). DocEditor always
   * edits markdown and can omit it.
   */
  languageId?: string;
}

function handleTableNav(
  ed: editor.IStandaloneCodeEditor,
  delta: number,
  languageId: string | undefined,
): void {
  const model = ed.getModel();
  const pos = ed.getPosition();
  if (!model || !pos) return;
  if (languageId && model.getLanguageId() !== languageId) {
    // Not a markdown model — preserve Monaco's default Tab behavior.
    ed.trigger("keyboard", "tab", {});
    return;
  }
  const move = nextCell(model.getValue(), model.getOffsetAt(pos), delta);
  if (!move) {
    // Cursor is not inside a GFM table — default Tab behavior.
    ed.trigger("keyboard", "tab", {});
    return;
  }
  if (move.insertAt !== undefined && move.insertText !== undefined) {
    const insertPos = model.getPositionAt(move.insertAt);
    ed.executeEdits("acowork.table-aid", [
      {
        range: {
          startLineNumber: insertPos.lineNumber,
          startColumn: insertPos.column,
          endLineNumber: insertPos.lineNumber,
          endColumn: insertPos.column,
        },
        text: move.insertText,
        forceMoveMarkers: true,
      },
    ]);
  }
  ed.setPosition(model.getPositionAt(move.targetOffset));
  ed.focus();
}

/**
 * Register Tab / Shift+Tab cell navigation for GFM tables.
 *
 * - Inside a table cell: Tab moves right, Shift+Tab moves left; past the
 *   last cell the row is extended with a new empty cell.
 * - Outside a table (or non-markdown model when `languageId` is set): the
 *   default Monaco Tab behavior is restored via `trigger("keyboard","tab")`.
 *
 * No dispose handle is returned: `addCommand` returns a command id and Monaco
 * 0.55 exposes no `removeCommand` on the standalone editor. Commands live and
 * die with the editor instance — DocEditor recreates the instance per doc
 * (keepCurrentModel=false), FileEditorPanel's singleton lives for the panel's
 * lifetime, so nothing leaks in either case.
 */
export function registerMarkdownTableNavigation(
  editor: editor.IStandaloneCodeEditor,
  monaco: typeof import("monaco-editor"),
  options: MarkdownTableNavOptions = {},
): void {
  const { languageId } = options;
  editor.addCommand(monaco.KeyCode.Tab, () => handleTableNav(editor, 1, languageId));
  editor.addCommand(monaco.KeyMod.Shift | monaco.KeyCode.Tab, () => handleTableNav(editor, -1, languageId));
}
