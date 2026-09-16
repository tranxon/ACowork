import { describe, expect, it, vi } from "vitest";
import type { editor } from "monaco-editor";
import { registerMarkdownTableNavigation } from "./editorAid";

const FAKE_MONACO = {
  KeyCode: { Tab: 1 },
  KeyMod: { Shift: 4 },
} as unknown as typeof import("monaco-editor");

interface FakeModel {
  language: string;
  text: string;
  cursorOffset: number;
}

function makeEditor(model: FakeModel) {
  const trigger = vi.fn();
  const executeEdits = vi.fn();
  const setPosition = vi.fn();
  const focus = vi.fn();
  const handlers: Array<(delta: number) => void> = [];

  const ed = {
    getModel: () => ({
      getLanguageId: () => model.language,
      getValue: () => model.text,
      getOffsetAt: () => model.cursorOffset,
      getPositionAt: (offset: number) => ({ lineNumber: 1, column: offset + 1 }),
    }),
    getPosition: () => ({ lineNumber: 1, column: model.cursorOffset + 1 }),
    addCommand: (_keybinding: number, handler: () => void) => {
      // Capture the handler; the harness re-invokes it as if Tab was pressed.
      // (real delta is baked into the closure — both Tab and Shift+Tab call
      // handleTableNav with their own delta, so a shared capture is enough)
      handlers.push(handler as unknown as (delta: number) => void);
      return "fake-cmd";
    },
    trigger,
    executeEdits,
    setPosition,
    focus,
  } as unknown as editor.IStandaloneCodeEditor;

  return { ed, trigger, executeEdits, setPosition, focus, handlers };
}

const TABLE_TEXT = "| a | b |\n| --- | --- |\n| 1 | 2 |";

describe("registerMarkdownTableNavigation", () => {
  it("registers Tab and Shift+Tab commands without throwing", () => {
    const { ed } = makeEditor({
      language: "markdown",
      text: TABLE_TEXT,
      cursorOffset: 18,
    });
    expect(() => registerMarkdownTableNavigation(ed, FAKE_MONACO)).not.toThrow();
  });

  it("restores default Tab when the model is not markdown (languageId filter)", () => {
    const { ed, trigger, handlers } = makeEditor({
      language: "typescript",
      text: TABLE_TEXT,
      cursorOffset: 18,
    });
    registerMarkdownTableNavigation(ed, FAKE_MONACO, { languageId: "markdown" });
    // Invoke the captured Tab handler.
    handlers[0]?.(1);
    expect(trigger).toHaveBeenCalledWith("keyboard", "tab", {});
  });

  it("restores default Tab outside a table even in markdown", () => {
    const { ed, trigger, handlers } = makeEditor({
      language: "markdown",
      text: "plain paragraph text\nwith no pipes",
      cursorOffset: 5,
    });
    registerMarkdownTableNavigation(ed, FAKE_MONACO);
    handlers[0]?.(1);
    expect(trigger).toHaveBeenCalledWith("keyboard", "tab", {});
  });

  it("moves the cursor inside a table (markdown, no language filter)", () => {
    const { ed, setPosition, handlers } = makeEditor({
      language: "markdown",
      text: TABLE_TEXT,
      cursorOffset: 25, // body row "1" cell
    });
    registerMarkdownTableNavigation(ed, FAKE_MONACO);
    handlers[0]?.(1); // Tab → move right
    expect(setPosition).toHaveBeenCalledTimes(1);
  });
});
