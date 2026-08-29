/**
 * markdownTable — regression tests for empty `<th>` cells.
 *
 * Why this file exists:
 *   When every title cell in a markdown table is blank/whitespace
 *   (`|     |     |     |`), `remark-gfm` strips the whitespace and
 *   emits truly empty `<th></th>` elements. Combined with
 *   `white-space: nowrap` (set on `.prose th`), the cell's line-box
 *   has nothing to size against, so the title row renders at roughly
 *   half the height of a populated row.
 *
 *   The fix lives in globals.css: `.prose th:empty::before { content:
 *   "\200b" }` injects a zero-width space, guaranteeing a line-box
 *   regardless of cell content. This file pins the regression in two
 *   ways:
 *     1. The CSS rule is present (parses globals.css via jsdom's
 *        `document.styleSheets` API — catches selector or property
 *        removal, survives whitespace/comment refactors).
 *     2. The DOM structure is correct (the bug surface — empty
 *        `<th>` cells exist in the rendered output — is preserved).
 *
 * Why we don't assert on rendered pixel height:
 *   jsdom doesn't compute layout. Asserting on the CSS rule + DOM
 *   shape is sufficient: any future regression would either remove
 *   the rule (test 1 fails) or change how react-markdown emits empty
 *   `<th>` (test 2 catches it).
 */
import React from "react";
import { render } from "@testing-library/react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { describe, expect, it } from "vitest";

const __dirname = dirname(fileURLToPath(import.meta.url));
const GLOBALS_CSS_PATH = resolve(__dirname, "../../styles/globals.css");

/** Mirror of the table override used by MessageBubble /
 *  MarkdownPreviewView / CompactionCard. If you change one, change all. */
const tableWrapper = {
  table: ({ children, ...rest }: React.TableHTMLAttributes<HTMLTableElement>) => (
    <div className="prose-table-scroll">
      <table {...rest}>{children}</table>
    </div>
  ),
};

/** Load globals.css into the test document so jsdom's styleSheets API
 *  can resolve rules. Mirrors what `main.tsx` does at runtime via the
 *  `import "./styles/globals.css"` side-effect. */
function loadGlobalsCss(): void {
  const css = readFileSync(GLOBALS_CSS_PATH, "utf8");
  const style = document.createElement("style");
  style.textContent = css;
  document.head.appendChild(style);
}

/** Walk every loaded stylesheet and return rules whose selectorText
 *  matches the given selector. jsdom exposes rules via
 *  `document.styleSheets[i].cssRules`. */
function findRule(selector: string): CSSStyleRule | undefined {
  for (const sheet of Array.from(document.styleSheets)) {
    const rules = Array.from((sheet as CSSStyleSheet).cssRules ?? []);
    const hit = rules.find((r) => (r as CSSStyleRule).selectorText === selector);
    if (hit) return hit as CSSStyleRule;
  }
  return undefined;
}

describe("markdownTable — empty <th> height regression", () => {
  it("CSS: declares a `::before` content rule for empty `.prose th`", () => {
    loadGlobalsCss();
    const rule = findRule(".prose th:empty::before");
    expect(rule, "expected `.prose th:empty::before` rule in globals.css").toBeDefined();
    // jsdom preserves CSS escape sequences literally (does NOT decode
    // `\200b` to the zero-width space char like a browser does), so we
    // match the 6-char escape sequence wrapped in double quotes — the
    // exact form the browser will decode at render time. Any visible
    // character (e.g. `" "`) or `none`/`normal` would either defeat
    // the purpose or re-introduce the bug.
    expect(rule!.style.content).toBe(`"\\200b"`);
  });

  it("DOM: renders all-empty-header markdown with truly empty <th> cells", () => {
    const { container } = render(
      <div className="prose">
        <ReactMarkdown remarkPlugins={[remarkGfm]} components={tableWrapper}>
          {`
|     |     |     |
| --- | --- | --- |
| a   | b   | c   |
`}
        </ReactMarkdown>
      </div>,
    );

    const ths = Array.from(container.querySelectorAll("th"));
    expect(ths).toHaveLength(3);
    // react-markdown strips the whitespace between pipes, so the bug
    // surface is a genuinely empty <th>. Pin this so a future
    // upgrade that preserves whitespace (or doesn't) doesn't silently
    // shift which case the CSS rule needs to handle.
    for (const th of ths) {
      expect(th.textContent).toBe("");
      expect(th.children).toHaveLength(0);
    }
  });

  it("DOM: preserves populated <th> alongside empty ones (mixed table)", () => {
    const { container } = render(
      <div className="prose">
        <ReactMarkdown remarkPlugins={[remarkGfm]} components={tableWrapper}>
          {`
|     | URL | OK |
| --- | --- | --- |
| a   | b   | c |
`}
        </ReactMarkdown>
      </div>,
    );

    const ths = Array.from(container.querySelectorAll("th"));
    expect(ths).toHaveLength(3);
    expect(ths.map((t) => t.textContent)).toEqual(["", "URL", "OK"]);
  });
});
