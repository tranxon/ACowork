/**
 * tableAid — pure GFM table cell navigation helpers for the Doc editor.
 *
 * Works on plain text + offsets (no Monaco): the caller maps offsets to
 * positions with `model.getPositionAt` and applies the returned edits via
 * `editor.executeEdits`, which keeps everything on the undo/redo stack.
 *
 * Scope (deliberately narrow, KISS):
 *   - Tab / Shift+Tab style horizontal cell-to-cell movement,
 *   - auto-extend the row when moving past the last cell (append a cell),
 *   - separator rows (`| --- |`) are never navigated into,
 *   - vertical movement is out of scope for v1.
 */

export interface CellRange {
  /** Cell text with surrounding whitespace trimmed. */
  text: string;
  /** Absolute offset of the trimmed cell content start. */
  start: number;
  /** Absolute offset of the trimmed cell content end (exclusive). */
  end: number;
}

export interface TableBlock {
  /** 0-based line index of the table start (first header line). */
  startLine: number;
  /** 0-based line index of the separator row. */
  separatorLine: number;
  /** 0-based line index of the table end (inclusive). */
  endLine: number;
}

export interface TableCellInfo {
  /** 0 = header row, 1+ = body rows (separator never reported). */
  row: number;
  /** 0-based column index within the row. */
  col: number;
  /** 0-based line index in the document. */
  line: number;
  /** Absolute offset of the line start. */
  lineStart: number;
  /** Absolute offset of the line end (exclusive, before `\n`). */
  lineEnd: number;
  /** Cells of the current row (absolute offsets). */
  cells: CellRange[];
}

export interface TableMove {
  /** Absolute cursor offset AFTER edits are applied. */
  targetOffset: number;
  /** When moving past the last cell: text to insert at `insertAt`. */
  insertAt?: number;
  insertText?: string;
}

const TABLE_LINE_RE = /^\s*\|/;
/** Separator row: only spaces / colons / pipes / dashes, with at least one dash. */
const SEPARATOR_RE = /^\s*\|?[\s:|-]+\|?\s*$/;

function isTableLine(line: string): boolean {
  return TABLE_LINE_RE.test(line);
}

function isSeparatorLine(line: string): boolean {
  return SEPARATOR_RE.test(line) && line.includes("-");
}

/** Split a table row into cells, preserving absolute content offsets. */
export function parseTableRow(line: string, lineStart: number): CellRange[] {
  const trimmedLead = line.replace(/^\s+/, "");
  const leadWs = line.length - trimmedLead.length;
  let body = trimmedLead;
  let bodyStart = lineStart + leadWs;

  // Strip one leading/trailing pipe (GFM pipes are optional at row edges).
  if (body.startsWith("|")) {
    body = body.slice(1);
    bodyStart += 1;
  }
  const endsWithPipe = body.endsWith("|");
  if (endsWithPipe) {
    body = body.slice(0, -1);
  }

  const cells: CellRange[] = [];
  let idx = 0;
  for (const seg of body.split("|")) {
    const segStart = bodyStart + idx;
    const trimmed = seg.trim();
    const leading = seg.length - seg.trimStart().length;
    const start = segStart + leading;
    cells.push({
      text: trimmed,
      start: trimmed.length === 0 ? segStart + 1 : start,
      end: trimmed.length === 0 ? segStart + 1 : start + trimmed.length,
    });
    idx += seg.length + 1;
  }
  // A trailing pipe (`| a | b |`) does NOT create an extra cell: GFM treats
  // the row as ending at `b`. Keeping the tail out of `cells` means "move
  // past the last cell" is exactly when `targetCol >= cells.length` — one
  // Tab from the last cell appends a new cell (see `nextCell`).
  return cells;
}

/** Locate the GFM table block containing `line`, or null. */
export function findTableAtLine(lines: string[], line: number): TableBlock | null {
  const last = lines.length - 1;
  if (line < 0 || line > last) return null;
  if (!isTableLine(lines[line])) return null;

  let start = line;
  while (start > 0 && isTableLine(lines[start - 1])) start -= 1;
  let end = line;
  while (end < last && isTableLine(lines[end + 1])) end += 1;

  for (let i = start; i <= end; i += 1) {
    if (isSeparatorLine(lines[i])) {
      return { startLine: start, separatorLine: i, endLine: end };
    }
  }
  return null;
}

function offsetToLine(text: string, offset: number): number {
  let line = 0;
  for (let i = 0; i < offset && i < text.length; i += 1) {
    if (text[i] === "\n") line += 1;
  }
  return line;
}

/**
 * Locate the table cell under `offset`. Returns null when the offset is not
 * inside a navigable table cell (no table / separator row / row edge gap).
 */
export function cellAt(text: string, offset: number): TableCellInfo | null {
  if (offset < 0 || offset > text.length) return null;
  const line = offsetToLine(text, offset);
  const lines = text.split("\n");
  const block = findTableAtLine(lines, line);
  if (!block) return null;
  if (line === block.separatorLine) return null;

  // Recompute the line start from the line index so it stays consistent with
  // `offsetToLine` (lastIndexOf("\n") misbehaves at an exact line boundary).
  let start = 0;
  for (let i = 0; i < line; i += 1) start += lines[i].length + 1;
  const lineEnd = start + lines[line].length;

  const cells = parseTableRow(lines[line], start);
  if (cells.length === 0) return null;

  // Pick the cell whose span covers offset; if offset sits in a gap between
  // cells (pipes / padding), snap to the nearest following cell.
  let col = cells.findIndex((c) => offset <= c.end);
  if (col === -1) col = cells.length - 1;

  const row = line <= block.separatorLine ? 0 : line - block.separatorLine;
  return { row, col, line, lineStart: start, lineEnd, cells };
}

/**
 * Horizontal cell movement. `deltaCol` is ±1 (caller binds Tab / Shift+Tab).
 * Returns the new cursor offset; when moving past the last cell the row is
 * extended with one empty cell (insertAt/insertText describes the edit).
 */
export function nextCell(text: string, offset: number, deltaCol: number): TableMove | null {
  const info = cellAt(text, offset);
  if (!info) return null;
  if (deltaCol === 0) return { targetOffset: offset };

  const targetCol = info.col + deltaCol;
  if (targetCol < 0) {
    // Already at the leftmost cell — stay put (don't leave the table).
    return { targetOffset: offset };
  }
  if (targetCol < info.cells.length) {
    return { targetOffset: info.cells[targetCol].start };
  }

  // Past the last cell: extend the row with one empty cell.
  const lineEnd = info.lineEnd;
  const lastChar = lineEnd > info.lineStart ? text[lineEnd - 1] : "";
  if (lastChar === "|") {
    // Insert a space before the closing pipe → new empty cell.
    const insertAt = lineEnd - 1;
    return { targetOffset: lineEnd, insertAt, insertText: " " };
  }
  return { targetOffset: lineEnd + 3, insertAt: lineEnd, insertText: " | " };
}
