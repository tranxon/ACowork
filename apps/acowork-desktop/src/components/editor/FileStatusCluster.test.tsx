/**
 * ADR-078 frontend tests — FileStatusCluster read-only branches.
 *
 * The cluster is a pure renderer driven by structural props. For
 * virtual git tabs (`kind: "diff" | "log"`) it must NOT show the edit
 * affordances (language name, LSP status, cursor position, selection
 * count) — instead it falls into the read-only branch that shows the
 * file's language/mime type and no cursor. This mirrors the "URL
 * preview" branch (also read-only) but without the host line.
 */

import { describe, it, expect, vi } from "vitest";
import { render, screen } from "@testing-library/react";

// ── Mocks ────────────────────────────────────────────────────────────────

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({
    t: (key: string, params?: Record<string, unknown>) => {
      if (key === "fileStatus.cursorPosition") {
        return `Ln ${params?.line}, Col ${params?.column}`;
      }
      if (key === "fileStatus.selectionCount") {
        return `(${params?.count} selected)`;
      }
      const table: Record<string, string> = {
        "fileStatus.languageFallback": "plaintext",
        "fileStatus.previewKind.url": "URL",
      };
      return table[key] ?? key;
    },
  }),
}));

vi.mock("./LspIndicator", () => ({
  LspIndicator: () => <span data-testid="lsp-indicator" />,
}));

// ── SUT ──────────────────────────────────────────────────────────────────

import { FileStatusCluster, type FileStatusClusterActiveFile } from "./FileStatusCluster";

const baseFile: FileStatusClusterActiveFile = {
  fileName: "a.ts",
  relPath: "src/a.ts",
  language: "typescript",
  mode: "edit",
  kind: "file",
};

const props = {
  cursor: { line: 3, column: 7 },
  selectedCount: 0,
  lspEnabled: true,
  lspLanguage: "typescript",
  lspStatus: { state: "ready" } as never,
  lspStatusMessage: "ready",
};

describe("FileStatusCluster", () => {
  it("renders nothing for a null or loading active file", () => {
    const { container, rerender } = render(
      <FileStatusCluster activeFile={null} {...props} />,
    );
    expect(container.firstChild).toBeNull();
    rerender(
      <FileStatusCluster activeFile={{ ...baseFile, loading: true }} {...props} />,
    );
    expect(container.firstChild).toBeNull();
  });

  it("renders edit branch (language + LSP + cursor) for a real file", () => {
    render(<FileStatusCluster activeFile={baseFile} {...props} />);
    expect(screen.getByText("typescript")).toBeTruthy();
    expect(screen.getByTestId("lsp-indicator")).toBeTruthy();
    expect(screen.getByText("Ln 3, Col 7")).toBeTruthy();
  });

  it("renders the read-only branch for a diff virtual tab — no LSP, no cursor", () => {
    render(
      <FileStatusCluster
        activeFile={{ ...baseFile, kind: "diff" }}
        {...props}
      />,
    );
    // Language label still shown (from the Monaco language), but no
    // LSP indicator and no cursor position.
    expect(screen.getByText("typescript")).toBeTruthy();
    expect(screen.queryByTestId("lsp-indicator")).toBeNull();
    expect(screen.queryByText(/Ln \d+, Col \d+/)).toBeNull();
  });

  it("renders the read-only branch for a log virtual tab", () => {
    render(
      <FileStatusCluster
        activeFile={{ ...baseFile, kind: "log", language: "plaintext" }}
        {...props}
      />,
    );
    expect(screen.getByText("plaintext")).toBeTruthy();
    expect(screen.queryByTestId("lsp-indicator")).toBeNull();
    expect(screen.queryByText(/Ln \d+, Col \d+/)).toBeNull();
  });

  it("renders the URL branch with host and no cursor", () => {
    render(
      <FileStatusCluster
        activeFile={{
          ...baseFile,
          kind: "url",
          url: "https://example.com/page",
          relPath: "https://example.com/page",
        }}
        {...props}
      />,
    );
    expect(screen.getByText("URL")).toBeTruthy();
    expect(screen.getByText("example.com")).toBeTruthy();
    expect(screen.queryByTestId("lsp-indicator")).toBeNull();
  });
});
