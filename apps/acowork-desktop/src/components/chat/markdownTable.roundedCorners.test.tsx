/**
 * markdownTable — regression tests for the rounded outer frame.
 *
 * Why this file exists:
 *   User-visible feature: the four corners of a markdown table should
 *   be rounded, matching `.prose pre` (code blocks) for visual parity.
 *
 *   Before the fix, `.prose table` set `border-collapse: collapse`
 *   which every current engine treats as a hard veto on `border-radius`
 *   — the rule was declared but rendered as square corners. The
 *   original CSS comment in globals.css explicitly documented this as a
 *   known limitation that was deferred ("rejected for this pass").
 *
 *   The fix flips `border-collapse` to `separate` (with `border-spacing:
 *   0` to preserve the flush cell grid) so the radius renders, and adds
 *   `overflow: hidden` so each `<th>`'s colored background is clipped
 *   to the rounded shape (cells themselves still have square corners in
 *   `separate` mode — without the clip, the th shade would peek out past
 *   the table's rounded outer border at the four corners).
 *
 *   This file pins the new behavior:
 *     1. `.prose table` has `border-collapse: separate` (NOT collapse),
 *        `overflow: hidden`, and `border-radius` referencing the same
 *        `--radius-md` token as `.prose pre`.
 *     2. `.prose pre` and `.prose table` both reference `--radius-md`
 *        so the two visual languages stay locked together.
 *
 * Why we don't assert on rendered pixels:
 *   jsdom doesn't compute layout. The CSS source IS the spec a future
 *   reader of globals.css needs to honor — we parse it via jsdom's
 *   `document.styleSheets` API (same trick the empty-header test uses)
 *   so any change that drops the rule, flips back to `collapse`, or
 *   decouples the radius from `.prose pre` will turn this test red.
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { describe, expect, it } from "vitest";

const __dirname = dirname(fileURLToPath(import.meta.url));
const GLOBALS_CSS_PATH = resolve(__dirname, "../../styles/globals.css");

/** Load globals.css into the test document. Same bootstrap as the
 *  empty-header test — mirrors what `main.tsx` does at runtime via
 *  the `import "./styles/globals.css"` side-effect. */
function loadGlobalsCss(): void {
  const css = readFileSync(GLOBALS_CSS_PATH, "utf8");
  const style = document.createElement("style");
  style.textContent = css;
  document.head.appendChild(style);
}

/** Walk every loaded stylesheet and return the rule whose selectorText
 *  matches. Throws if no rule is found — fail-loud is the right default
 *  for a regression test (missing rule = broken UI). */
function findRuleOrThrow(selector: string): CSSStyleRule {
  for (const sheet of Array.from(document.styleSheets)) {
    const rules = Array.from((sheet as CSSStyleSheet).cssRules ?? []);
    const hit = rules.find((r) => (r as CSSStyleRule).selectorText === selector);
    if (hit) return hit as CSSStyleRule;
  }
  throw new Error(`CSS rule not found: ${selector}`);
}

/** Read a single CSS property's *raw* value from a rule's cssText.
 *
 *  jsdom quirk: when a property is declared via shorthand (e.g.
 *  `border-radius: var(--radius-md)`) or via a value that requires
 *  resolving custom properties, the `rule.style[name]` accessors stay
 *  empty even though the rule parsed fine. The cssText, by contrast,
 *  always contains the original source — so we regex-extract from it.
 *  This is exactly what the test wants to pin: the value a designer
 *  wrote, not the value jsdom could compute.
 *
 *  Returns `undefined` if the property is not declared. */
function readProperty(rule: CSSStyleRule, property: string): string | undefined {
  // Match `prop: value` (up to the next `;` or `}`). Tolerant of
  // arbitrary whitespace; the property must follow a declaration
  // boundary (`{`, `;`, or whitespace after one of those) so we
  // never accidentally match a longhand when looking for a shorthand
  // prefix (e.g. `border` should not match `border-radius`).
  const re = new RegExp(`(?:[{;\\s])${escapeRegExp(property)}\\s*:\\s*([^;}]+)`);
  const match = rule.cssText.match(re);
  return match?.[1]?.trim();
}

function escapeRegExp(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

describe("markdownTable — rounded outer frame", () => {
  it("CSS: `.prose table` uses border-collapse: separate so border-radius renders", () => {
    loadGlobalsCss();
    const rule = findRuleOrThrow(".prose table");

    // The whole fix hinges on this property. If anyone flips it back
    // to `collapse`, every engine silently ignores border-radius on
    // <table> and the user sees square corners again.
    expect(readProperty(rule, "border-collapse")).toBe("separate");

    // border-spacing: 0 keeps the cell grid flush — required because
    // `separate` introduces inter-cell spacing by default.
    expect(readProperty(rule, "border-spacing")).toBe("0");
  });

  it("CSS: `.prose table` declares border-radius via the shared --radius-md token", () => {
    loadGlobalsCss();
    const rule = findRuleOrThrow(".prose table");

    // jsdom preserves the var() reference literally in cssText (it does
    // not resolve custom properties when accessed via CSSStyleDeclaration),
    // so we can match the exact token used by the design system. This
    // pins visual parity with `.prose pre` — if someone changes one
    // without the other, the test goes red on both sides (see the
    // parity check below).
    expect(readProperty(rule, "border-radius")).toBe("var(--radius-md)");
  });

  it("CSS: `.prose table` clips cell backgrounds with overflow: hidden", () => {
    loadGlobalsCss();
    const rule = findRuleOrThrow(".prose table");

    // Without this, `<th>`'s colored title background would extend
    // past the table's rounded border at the four corners (cells
    // themselves have square corners in `separate` mode).
    expect(readProperty(rule, "overflow")).toBe("hidden");
  });

  it("CSS: `.prose pre` and `.prose table` share the same --radius-md token", () => {
    loadGlobalsCss();
    const preRule = findRuleOrThrow(".prose pre");
    const tableRule = findRuleOrThrow(".prose table");

    // Visual language lock: code blocks and tables must round with the
    // same radius. If a designer wants to tweak the radius, they edit
    // --radius-md once and both surfaces follow. If someone bypasses
    // the token on either side, this assertion fires.
    const preRadius = readProperty(preRule, "border-radius");
    const tableRadius = readProperty(tableRule, "border-radius");
    expect(preRadius).toBe("var(--radius-md)");
    expect(tableRadius).toBe(preRadius);
  });
});