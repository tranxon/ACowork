/**
 * PreviewPane.test.ts — regression checks for `isTextMime`.
 *
 * Background: `isTextMime` drives the text-vs-binary branch in
 * `FilePreview`. The runtime's `mime_type_for` table (see
 * core/acowork-runtime/src/usecases/workspace_query_impl.rs) is the
 * source of truth — every text-classified mime it can return must be
 * recognised here, otherwise the preview falls through to the
 * "binary file (mime, size) — snippet only" placeholder for an
 * actually-textual file. This test pins the table to catch any drift
 * in either direction:
 *
 *   - false positive (text file marked binary) → confusing UX
 *     ("TSX is not a binary file, why does preview say binary?").
 *   - false negative (binary file marked text)  → `<pre>` rendered
 *     with base64 bytes (ripgrep would never match a binary, but
 *     defence-in-depth).
 *
 * Run: `npx vitest run src/components/search/PreviewPane.test.ts`
 */

import { describe, expect, it } from "vitest";
import { isTextMime } from "./PreviewPane";

describe("isTextMime", () => {
    // Source-code / config mime types the runtime sends via
    // `mime_type_for`. All must be treated as text — every entry here
    // corresponds to a documented line in workspace_query_impl.rs.
    const TEXT_MIMES = [
        // text/* family — most source files (.rs, .py, .go, .java, .kt,
        // .swift, .c, .cpp, plain text, markdown, html, css, …)
        "text/plain",
        "text/markdown",
        "text/html",
        "text/css",
        "text/x-rust",
        "text/x-python",
        "text/x-go",
        "text/x-c",
        "text/x-cpp",

        // application/* source / config — without `application/typescript`
        // here, .ts / .tsx previews fall through to the binary branch
        // (the original bug this regression test pins down).
        "application/json",
        "application/javascript",
        "application/typescript",
        "application/xml",
        "application/yaml",
        "application/x-yaml", // legacy alias
        "application/toml",
        "application/x-sh",
        "application/x-shellscript", // legacy alias
        "application/x-powershell",

        // SVG is XML markup; runtime returns it as raw text in `content`.
        "image/svg+xml",

        // Runtime's "unknown extension" fallback. If ripgrep matched it,
        // the content is text — render it.
        "application/octet-stream",

        // Missing mime — default to text rather than guess binary.
        undefined as unknown as string,
        "",
    ];

    it.each(TEXT_MIMES)("treats %s as text", (mime) => {
        expect(isTextMime(mime)).toBe(true);
    });

    const BINARY_MIMES = [
        // Anything image/* other than the SVG exception above.
        "image/png",
        "image/jpeg",
        "image/gif",
        "image/webp",
        "image/bmp",

        // PDF — text but binary-encoded; dump bytes to <pre> and the
        // user gets garbage.
        "application/pdf",

        // Archives / compiled artefacts the runtime would never
        // return from `mime_type_for` today, but a future runtime-side
        // change could. Stay conservative.
        "application/zip",
        "application/wasm",
        "font/woff2",
        "video/mp4",
        "audio/mpeg",
    ];

    it.each(BINARY_MIMES)("treats %s as binary", (mime) => {
        expect(isTextMime(mime)).toBe(false);
    });
});