import { describe, expect, it } from "vitest";
import {
  bold,
  bulletList,
  codeBlock,
  heading,
  image,
  inlineCode,
  italic,
  link,
  mermaid,
  orderedList,
  quote,
  rule,
  strike,
  table,
  taskList,
  wrapSelection,
} from "./snippets";

describe("wrapSelection", () => {
  it("wraps the selected text and keeps it selected", () => {
    expect(wrapSelection("**", "**", "abc")).toEqual({
      text: "**abc**",
      selectionStart: 2,
      selectionEnd: 5,
    });
  });
});

describe("heading", () => {
  it("prefixes # + space and puts the cursor after the text", () => {
    const r = heading(1, "Title");
    expect(r.text).toBe("# Title");
    expect(r.selectionStart).toBe(r.text.length);
  });
  it("clamps level to [1, 6]", () => {
    expect(heading(7, "x").text).toBe("###### x");
    expect(heading(0, "x").text).toBe("# x");
  });
});

describe("inline formatting", () => {
  it("bold / italic / strike / inlineCode wrap and select", () => {
    expect(bold("a")).toEqual({ text: "**a**", selectionStart: 2, selectionEnd: 3 });
    expect(italic("a")).toEqual({ text: "*a*", selectionStart: 1, selectionEnd: 2 });
    expect(strike("a")).toEqual({ text: "~~a~~", selectionStart: 2, selectionEnd: 3 });
    expect(inlineCode("a")).toEqual({ text: "`a`", selectionStart: 1, selectionEnd: 2 });
  });
});

describe("link / image", () => {
  it("link with selection puts the cursor on url", () => {
    const r = link("docs");
    expect(r.text).toBe("[docs](url)");
    expect(r.selectionStart).toBe(r.text.length - 1);
  });
  it("link without selection selects the [text] placeholder", () => {
    const r = link("");
    expect(r.text).toBe("[text](url)");
    expect(r.selectionStart).toBe(1);
    expect(r.selectionEnd).toBe(5);
  });
  it("image with selection keeps alt selected", () => {
    const r = image("logo");
    expect(r.text).toBe("![logo](url)");
    expect(r.selectionStart).toBe(2);
    expect(r.selectionEnd).toBe(6);
  });
  it("image without selection selects the alt placeholder", () => {
    const r = image("");
    expect(r.text).toBe("![alt](url)");
    expect(r.selectionStart).toBe(2);
    expect(r.selectionEnd).toBe(5);
  });
});

describe("quote", () => {
  it("prefixes every non-empty line", () => {
    expect(quote("a\nb").text).toBe("> a\n> b");
  });
  it("keeps blockquote continuity on empty lines (GFM '>' marker)", () => {
    expect(quote("a\n\nb").text).toBe("> a\n>\n> b");
  });
  it("empty selection gives a bare quote marker with cursor after it", () => {
    const r = quote("");
    expect(r.text).toBe("> ");
    expect(r.selectionStart).toBe(2);
  });
});

describe("lists", () => {
  it("bulletList prefixes every non-empty line", () => {
    expect(bulletList("a\nb").text).toBe("- a\n- b");
  });
  it("orderedList numbers lines 1..n", () => {
    expect(orderedList("a\nb\nc").text).toBe("1. a\n2. b\n3. c");
  });
  it("taskList emits unchecked boxes", () => {
    expect(taskList("a\nb").text).toBe("- [ ] a\n- [ ] b");
  });
  it("empty selections leave an active list marker", () => {
    const r = bulletList("");
    expect(r.text).toBe("- ");
    expect(r.selectionStart).toBe(2);
  });
});

describe("codeBlock", () => {
  it("wraps selection in a fenced block and keeps the cursor after the code", () => {
    const r = codeBlock("ts", "const a = 1;");
    expect(r.text).toBe("```ts\nconst a = 1;\n```");
    expect(r.selectionStart).toBe("```ts\n".length + "const a = 1;".length);
  });
  it("empty selection keeps cursor at the body position", () => {
    const r = codeBlock("", "");
    expect(r.text).toBe("```\n\n```");
    expect(r.selectionStart).toBe("```\n".length);
  });
});

describe("mermaid", () => {
  it("inserts a mermaid fence with a starter diagram", () => {
    const r = mermaid("");
    expect(r.text.startsWith("```mermaid\n")).toBe(true);
    expect(r.text.endsWith("\n```")).toBe(true);
    expect(r.text).toContain("graph TD");
  });
  it("wraps existing selection", () => {
    const r = mermaid("graph LR\n  A --> B");
    expect(r.text).toBe("```mermaid\ngraph LR\n  A --> B\n```");
  });
});

describe("table", () => {
  it("generates a GFM table with header, separator and body rows", () => {
    const r = table(2, 3, (i) => `H${i + 1}`);
    const lines = r.text.split("\n");
    expect(lines).toHaveLength(4);
    expect(lines[0]).toBe("| H1 | H2 | H3 |");
    expect(lines[1]).toBe("| --- | --- | --- |");
    expect(lines[2]).toBe("|   |   |   |");
    expect(lines[3]).toBe("|   |   |   |");
  });
  it("places the cursor in the first body cell", () => {
    const r = table(2, 3, (i) => `H${i + 1}`);
    const lines = r.text.split("\n");
    const firstBodyOffset = lines[0].length + 1 + lines[1].length + 1;
    expect(r.selectionStart).toBe(firstBodyOffset + 2);
    const prefix = r.text.slice(0, r.selectionStart);
    expect(prefix.endsWith("| ")).toBe(true);
  });
  it("clamps dimensions to sane bounds", () => {
    expect(table(0, 0, () => "x").text.split("\n")).toHaveLength(3); // 1 body row
    expect(table(99, 99, () => "x").text.split("\n")).toHaveLength(22); // 20 body rows
    // 12 cols → header has exactly 12 labelled columns.
    const header = table(1, 99, () => "x").text.split("\n")[0];
    expect(header).toBe(`| ${Array.from({ length: 12 }, () => "x").join(" | ")} |`);
  });
});

describe("rule", () => {
  it("emits a blank-line-delimited horizontal rule and leaves the cursor after it", () => {
    const r = rule();
    expect(r.text).toBe("\n---\n");
    expect(r.selectionStart).toBe(r.text.length);
  });
});
