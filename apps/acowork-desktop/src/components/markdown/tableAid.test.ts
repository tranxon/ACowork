import { describe, expect, it } from "vitest";
import { cellAt, findTableAtLine, nextCell, parseTableRow } from "./tableAid";

/** Absolute offset of a line start (0-based line index). */
function lineStart(text: string, line: number): number {
  const lines = text.split("\n");
  let s = 0;
  for (let i = 0; i < line; i += 1) s += lines[i].length + 1;
  return s;
}

const TEXT = ["前文", "| a | b |", "| --- | --- |", "| 1 | 2 |", "后文"].join("\n");

describe("parseTableRow", () => {
  it("parses cells with and without a trailing pipe", () => {
    const ls = lineStart("| a | b |", 0);
    const cells = parseTableRow("| a | b |", ls);
    expect(cells.map((c) => c.text)).toEqual(["a", "b"]);
    expect(cells[0].start).toBe(ls + 2);
    expect(cells[1].start).toBe(ls + 6);
  });
  it("treats a missing trailing pipe as an open row", () => {
    const ls = 0;
    const cells = parseTableRow("| a | b", ls);
    expect(cells.map((c) => c.text)).toEqual(["a", "b"]);
  });
});

describe("findTableAtLine", () => {
  it("locates a GFM table and its separator row", () => {
    const lines = TEXT.split("\n");
    const block = findTableAtLine(lines, 3);
    expect(block).not.toBeNull();
    expect(block!.startLine).toBe(1);
    expect(block!.separatorLine).toBe(2);
    expect(block!.endLine).toBe(3);
  });
  it("returns null for a non-table line", () => {
    expect(findTableAtLine(TEXT.split("\n"), 0)).toBeNull();
    expect(findTableAtLine(TEXT.split("\n"), 4)).toBeNull();
  });
  it("returns null when there is no separator row (not a table)", () => {
    const noTable = ["| x | y |", "| 1 | 2 |"].join("\n");
    expect(findTableAtLine(noTable.split("\n"), 0)).toBeNull();
  });
});

describe("cellAt", () => {
  it("reports header row 0 and body row 1", () => {
    const header = cellAt(TEXT, lineStart(TEXT, 1) + 2); // "a"
    expect(header).not.toBeNull();
    expect(header!.row).toBe(0);
    expect(header!.col).toBe(0);

    const body = cellAt(TEXT, lineStart(TEXT, 3) + 2); // "1"
    expect(body).not.toBeNull();
    expect(body!.row).toBe(1);
    expect(body!.col).toBe(0);
    expect(body!.line).toBe(3);
  });
  it("snaps an offset in the pipe gap to the following cell", () => {
    const info = cellAt(TEXT, lineStart(TEXT, 1) + 4); // pipe between "a" and "b"
    expect(info!.cells[info!.col].text).toBe("b");
  });
  it("returns null on the separator row and on plain text", () => {
    expect(cellAt(TEXT, lineStart(TEXT, 2) + 2)).toBeNull();
    expect(cellAt(TEXT, lineStart(TEXT, 0) + 1)).toBeNull();
  });
});

describe("nextCell", () => {
  it("moves right into the next cell", () => {
    const from = lineStart(TEXT, 3) + 2; // start of "1"
    const move = nextCell(TEXT, from, 1);
    expect(move).not.toBeNull();
    expect(move!.targetOffset).toBe(lineStart(TEXT, 3) + 6); // start of "2"
  });
  it("moves left into the previous cell", () => {
    const from = lineStart(TEXT, 3) + 6; // start of "2"
    const move = nextCell(TEXT, from, -1);
    expect(move!.targetOffset).toBe(lineStart(TEXT, 3) + 2);
  });
  it("stays put at the leftmost cell", () => {
    const from = lineStart(TEXT, 3) + 2;
    const move = nextCell(TEXT, from, -1);
    expect(move!.targetOffset).toBe(from);
  });
  it("extends a row ending in a closing pipe", () => {
    const from = lineStart(TEXT, 3) + 6; // start of "2"
    const move = nextCell(TEXT, from, 1);
    expect(move!.insertAt).toBe(lineStart(TEXT, 3) + 8); // before closing pipe
    expect(move!.insertText).toBe(" ");
    expect(move!.targetOffset).toBe(lineStart(TEXT, 3) + 9); // after the pipe
  });
  it("extends a row without a closing pipe", () => {
    const open = "| a | b\n| --- | ---\n| 1 | 2\n";
    const ls = lineStart(open, 2);
    const from = ls + 6; // end of "2"
    const move = nextCell(open, from, 1);
    expect(move!.insertAt).toBe(ls + 7);
    expect(move!.insertText).toBe(" | ");
    expect(move!.targetOffset).toBe(ls + 10);
  });
  it("does not navigate outside a table", () => {
    expect(nextCell(TEXT, lineStart(TEXT, 0) + 1, 1)).toBeNull();
  });
});
