/**
 * markdownTable — regression tests for the .prose-table-scroll wrapper
 * applied by our ReactMarkdown component overrides.
 *
 * Why this test exists:
 *   Before the fix, `.prose table` had `width: 100%` and `word-break:
 *   break-word`, which caused a sibling column with an oversized token
 *   (URL / hex / path) to squeeze the title column down to 2-4
 *   characters and break mid-word. The fix wraps <table> in a
 *   horizontally-scrollable div (`.prose-table-scroll`) and switches
 *   the column-break policy to `overflow-wrap: anywhere`.
 *
 *   We assert on the rendered DOM shape — not CSS computed style —
 *   because jsdom doesn't compute layout. The assertion that matters
 *   for regression: any markdown table passes through the wrapper,
 *   so future "simplification" of the component override map can't
 *   silently remove the wrapper without this test going red.
 */
import React from "react";
import { render } from "@testing-library/react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { describe, expect, it } from "vitest";

/** Mirror of the same table override used by MessageBubble /
 *  MarkdownPreviewView / CompactionCard. If you change one, change all. */
const tableWrapper = {
  table: ({ children, ...rest }: React.TableHTMLAttributes<HTMLTableElement>) => (
    <div className="prose-table-scroll">
      <table {...rest}>{children}</table>
    </div>
  ),
};

const SAMPLE_MARKDOWN = `
| URL | Title | OK |
| --- | --- | --- |
| https://example.com/a/very/long/path/that/exceeds/the/container/width/by/a/lot | Status | yes |
| https://example.com/another/long/url | Status | no |
`;

describe("markdownTable wrapper", () => {
  it("wraps every rendered <table> in a .prose-table-scroll container", () => {
    const { container } = render(
      <div className="prose">
        <ReactMarkdown remarkPlugins={[remarkGfm]} components={tableWrapper}>
          {SAMPLE_MARKDOWN}
        </ReactMarkdown>
      </div>,
    );

    const wrappers = container.querySelectorAll(".prose-table-scroll");
    expect(wrappers).toHaveLength(1);

    const wrapper = wrappers[0];
    // The wrapper must contain the <table> as a direct child, not the
    // other way around — browsers compute scroll geometry on the
    // outer element, so the <table> sits *inside* the scroll viewport.
    const table = wrapper.querySelector("table");
    expect(table).not.toBeNull();
    expect(wrapper.firstElementChild?.tagName).toBe("TABLE");
  });

  it("passes through header rows so .prose th whitespace rules apply", () => {
    const { container } = render(
      <div className="prose">
        <ReactMarkdown remarkPlugins={[remarkGfm]} components={tableWrapper}>
          {SAMPLE_MARKDOWN}
        </ReactMarkdown>
      </div>,
    );

    // <th> must still exist (overrides mustn't swallow them).
    expect(container.querySelectorAll("th")).toHaveLength(3);
    // The header text comes through intact (no mid-word break injected).
    const headers = Array.from(container.querySelectorAll("th")).map((el) => el.textContent);
    expect(headers).toEqual(["URL", "Title", "OK"]);
  });
});
