/**
 * snippets — pure Markdown snippet generators for the Doc editor toolbar.
 *
 * Every function takes the currently-selected text (possibly "") and returns
 * a `SnippetResult`:
 *   - `text`            — full text to replace the selection with,
 *   - `selectionStart`  — 0-based offset (relative to `text`) where the
 *                         cursor should land after insertion,
 *   - `selectionEnd`    — optional end offset; when present the range
 *                         [selectionStart, selectionEnd) is selected.
 *
 * The functions are PURE — no Monaco, no DOM. The toolbar applies the result
 * via `editor.executeEdits` (so it goes through the undo/redo stack) and then
 * maps the offsets to positions with `model.getPositionAt`.
 *
 * Offsets are 0-based, matching `model.getOffsetAt` semantics.
 */

export interface SnippetResult {
  text: string;
  selectionStart: number;
  selectionEnd?: number;
}

/** Multi-line aware EOL (LF is the doc-domain convention; keep it simple). */
const EOL = "\n";

/** Prefix/suffix wrap; cursor keeps the selected text selected. */
export function wrapSelection(
  prefix: string,
  suffix: string,
  selected: string,
): SnippetResult {
  const text = prefix + selected + suffix;
  return { text, selectionStart: prefix.length, selectionEnd: prefix.length + selected.length };
}

/** Empty-input fallback used by link/image: select the placeholder. */
function placeholder(
  text: string,
  placeholderStart: number,
  placeholderEnd: number,
): SnippetResult {
  return { text, selectionStart: placeholderStart, selectionEnd: placeholderEnd };
}

/** `# ` × level prefix; cursor after the heading text. */
export function heading(level: number, selected: string): SnippetResult {
  const prefix = "#".repeat(Math.max(1, Math.min(6, level))) + " ";
  const text = prefix + selected;
  return { text, selectionStart: text.length };
}

export function bold(selected: string): SnippetResult {
  return wrapSelection("**", "**", selected);
}

export function italic(selected: string): SnippetResult {
  return wrapSelection("*", "*", selected);
}

export function strike(selected: string): SnippetResult {
  return wrapSelection("~~", "~~", selected);
}

export function inlineCode(selected: string): SnippetResult {
  return wrapSelection("`", "`", selected);
}

/** `[selected](url)` — cursor on the url when there is a selection. */
export function link(selected: string): SnippetResult {
  if (selected) {
    const text = `[${selected}](url)`;
    return { text, selectionStart: text.length - 1, selectionEnd: text.length - 1 };
  }
  return placeholder("[text](url)", 1, 5);
}

/** `![alt](url)` — cursor on the alt text (the part you most often edit). */
export function image(selected: string): SnippetResult {
  if (selected) {
    const text = `![${selected}](url)`;
    return { text, selectionStart: 2, selectionEnd: 2 + selected.length };
  }
  return placeholder("![alt](url)", 2, 5);
}

/** Prefix every (non-empty) line with `> `; empty input → bare `> `. */
export function quote(selected: string): SnippetResult {
  const body = selected.split(EOL);
  const text = body.map((line) => (line ? `> ${line}` : ">")).join(EOL);
  if (selected) return { text, selectionStart: text.length };
  return { text: "> ", selectionStart: 2 };
}

function listPrefix(selected: string, prefix: (index: number) => string): SnippetResult {
  if (!selected) {
    const p = prefix(0);
    return { text: p + " ", selectionStart: p.length + 1 };
  }
  const body = selected.split(EOL);
  const text = body.map((line, i) => (line ? `${prefix(i)} ${line}` : line)).join(EOL);
  return { text, selectionStart: text.length };
}

export function bulletList(selected: string): SnippetResult {
  return listPrefix(selected, () => "-");
}

export function orderedList(selected: string): SnippetResult {
  return listPrefix(selected, (i) => `${i + 1}.`);
}

export function taskList(selected: string): SnippetResult {
  return listPrefix(selected, () => "- [ ]");
}

/** Fenced code block; cursor after the code when there is a selection. */
export function codeBlock(language: string, selected: string): SnippetResult {
  const lang = language.trim();
  const fence = "```";
  const text = `${fence}${lang}${EOL}${selected}${EOL}${fence}`;
  if (selected) {
    // Cursor at the end of the code body (before the closing fence EOL).
    const bodyStart = fence.length + lang.length + EOL.length;
    return { text, selectionStart: bodyStart + selected.length };
  }
  // Empty body: select the placeholder line so typing replaces it.
  const bodyStart = fence.length + lang.length + EOL.length;
  return placeholder(text, bodyStart, bodyStart);
}

export function mermaid(selected: string): SnippetResult {
  return codeBlock("mermaid", selected || "graph TD\n    A[Start] --> B[End]");
}

/**
 * GFM table with `rows` body rows and `cols` columns; cursor lands in the
 * first body cell. Header cells are labelled 列 1..cols (localized later by
 * the caller — see `table`), separator row is `---`.
 */
export function table(rows: number, cols: number, headerLabel: (i: number) => string): SnippetResult {
  const r = Math.max(1, Math.min(20, rows));
  const c = Math.max(1, Math.min(12, cols));
  const header = Array.from({ length: c }, (_, i) => headerLabel(i)).join(" | ");
  const separator = Array.from({ length: c }, () => "---").join(" | ");
  const bodyRow = Array.from({ length: c }, () => " ").join(" | ");
  const lines = [`| ${header} |`, `| ${separator} |`, ...Array.from({ length: r }, () => `| ${bodyRow} |`)];
  const text = lines.join(EOL);
  // Cursor: first body row, first cell → offset after `| ` of the first body line.
  const firstBodyLineStart = lines[0].length + EOL.length + lines[1].length + EOL.length;
  return { text, selectionStart: firstBodyLineStart + 2 };
}

/** Horizontal rule on its own line (blank line before/after for GFM). */
export function rule(): SnippetResult {
  const text = `${EOL}---${EOL}`;
  return { text, selectionStart: text.length };
}
