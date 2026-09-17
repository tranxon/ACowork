/**
 * markdownLinkResolver — chat-surface link-handling regression tests.
 *
 * Why these exist:
 *   A chat message carrying `[x](D:/repo/a.md)` used to render as
 *   `<a href="">`: react-markdown's stock `defaultUrlTransform`
 *   classifies `D:` as an unknown protocol and blanks the href, and an
 *   empty-href click lets the webview default-navigate — a full reload
 *   that on WebView2 could crash the renderer.
 *
 * Contracts locked in:
 *   1. `markdownUrlTransform` keeps local absolute paths (`D:/…`, `D:\…`,
 *      `file:///…`) alive while every other input keeps the stock
 *      sanitisation — the `javascript:` XSS guard included.
 *   2. `parseChatLinkHref` normalises the clickable shapes (URL-ified
 *      `/D:/…`, `#L42` / `#L42-L67` line fragments) the resolver accepts.
 *   3. A `D:/…` link survives the *real* ReactMarkdown render pipeline
 *      with a non-empty href — the exact regression that made clicking
 *      a chat link reload the app.
 */
import React from "react";
import { render } from "@testing-library/react";
import ReactMarkdown, { defaultUrlTransform } from "react-markdown";
import { describe, expect, it } from "vitest";
import {
  handleChatMarkdownLinkClick,
  markdownUrlTransform,
  parseChatLinkHref,
} from "./markdownLinkResolver";

describe("markdownUrlTransform", () => {
  it("keeps Windows drive paths intact (the empty-href regression)", () => {
    expect(markdownUrlTransform("D:/projects/repo/docs/a.md")).toBe("D:/projects/repo/docs/a.md");
    expect(markdownUrlTransform("D:\\projects\\repo\\docs\\a.md")).toBe("D:\\projects\\repo\\docs\\a.md");
    expect(markdownUrlTransform("d:/lower-case.md")).toBe("d:/lower-case.md");
  });

  it("converts file:// URLs to plain local paths", () => {
    expect(markdownUrlTransform("file:///D:/projects/a.md")).toBe("D:/projects/a.md");
    expect(markdownUrlTransform("file:///home/u/a.md")).toBe("/home/u/a.md");
    expect(markdownUrlTransform("file:///home/u/a%20b/c.md")).toBe("/home/u/a b/c.md");
  });

  it("falls back to stock sanitisation for file:// UNC URLs", () => {
    // `file://server/share` carries a host the workspace resolver cannot
    // map — the stock transform blanks it and the render layers degrade
    // the link to plain text.
    expect(markdownUrlTransform("file://server/share/a.md")).toBe("");
  });

  it("passes allow-listed schemes and relative paths through unchanged", () => {
    expect(markdownUrlTransform("https://example.com/x")).toBe("https://example.com/x");
    expect(markdownUrlTransform("mailto:a@b.c")).toBe("mailto:a@b.c");
    expect(markdownUrlTransform("docs/relative/a.md")).toBe("docs/relative/a.md");
    expect(markdownUrlTransform("#anchor")).toBe("#anchor");
  });

  it("keeps the stock XSS posture (unknown protocols blanked)", () => {
    expect(markdownUrlTransform("javascript:alert(1)")).toBe("");
    expect(markdownUrlTransform("data:text/html,plain")).toBe(
      defaultUrlTransform("data:text/html,plain"),
    );
  });
});

describe("parseChatLinkHref", () => {
  it("keeps plain drive paths and relative paths", () => {
    expect(parseChatLinkHref("D:/repo/a.md")).toEqual({ path: "D:/repo/a.md", line: undefined });
    expect(parseChatLinkHref("docs/a.md")).toEqual({ path: "docs/a.md", line: undefined });
  });

  it("strips the leading slash from URL-ified drive paths", () => {
    // `/D:/…` — the shape left over from editors that slash-prefix drive
    // paths; the workspace prefix matcher cannot absorb the slash form.
    expect(parseChatLinkHref("/D:/repo/a.md")).toEqual({ path: "D:/repo/a.md", line: undefined });
  });

  it("extracts #L42 / #L42-L67 line fragments", () => {
    expect(parseChatLinkHref("D:/repo/a.md#L42")).toEqual({ path: "D:/repo/a.md", line: 42 });
    expect(parseChatLinkHref("src/foo.ts#L42-L67")).toEqual({ path: "src/foo.ts", line: 42 });
  });

  it("ignores non-line fragments", () => {
    expect(parseChatLinkHref("docs/a.md#section")).toEqual({ path: "docs/a.md", line: undefined });
    expect(parseChatLinkHref("docs/a.md#")).toEqual({ path: "docs/a.md", line: undefined });
  });
});

describe("handleChatMarkdownLinkClick", () => {
  it("silently ignores empty hrefs and in-page anchors", () => {
    // Must not throw and must not touch any store — these two shapes are
    // exactly what a broken render would hand over.
    expect(() => handleChatMarkdownLinkClick("")).not.toThrow();
    expect(() => handleChatMarkdownLinkClick("#section")).not.toThrow();
  });
});

describe("D:/ link through the real ReactMarkdown pipeline", () => {
  it("renders a non-empty href for a drive-path link", () => {
    const { container } = render(
      <ReactMarkdown urlTransform={markdownUrlTransform}>{"[a](D:/repo/docs/a.md)"}</ReactMarkdown>,
    );
    const anchor = container.querySelector("a");
    expect(anchor).not.toBeNull();
    // Regression: the stock transform produced `href=""` here.
    expect(anchor?.getAttribute("href")).toBe("D:/repo/docs/a.md");
  });
});
