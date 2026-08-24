import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

const host = process.env.TAURI_DEV_HOST;

export default defineConfig(async () => ({
  plugins: [react(), tailwindcss()],
  clearScreen: false,
  worker: {
    format: "es",
  },
  optimizeDeps: {
    // ──────────────────────────────────────────────────────────────────
    // Why these explicit includes?
    //
    // `@codingame/monaco-vscode-api` dynamically imports the
    // `*-service-override` packages at runtime, chosen by
    // `viewsConfig.$type`.  Vite's dependency scanner cannot see those
    // dynamic imports, so it discovers them lazily during the first
    // browser request — and assigns a fresh content-hash.  On the next
    // dev-server restart the hash rotates again, and any in-browser
    // module reference still pointing at the old hash produces
    //
    //   TypeError: Failed to fetch dynamically imported module:
    //     /node_modules/.vite/deps/monaco-vscode-editor-service-override-XXXX.js
    //
    // Listing them here makes Vite pre-bundle them at startup, with
    // hashes that only rotate when the underlying source actually
    // changes (not on every restart).  This stabilises the URLs that
    // `monaco-vscode-api` emits at runtime.
    // ──────────────────────────────────────────────────────────────────
    include: [
      "monaco-editor",
      "monaco-languageclient",
      "@codingame/monaco-vscode-api",
      "@codingame/monaco-vscode-editor-api",
      "@codingame/monaco-vscode-configuration-service-override",
      "@codingame/monaco-vscode-editor-service-override",
      "@codingame/monaco-vscode-extensions-service-override",
      "@codingame/monaco-vscode-languages-service-override",
      "@codingame/monaco-vscode-log-service-override",
      "@codingame/monaco-vscode-model-service-override",
      "@codingame/monaco-vscode-monarch-service-override",
      "@codingame/monaco-vscode-textmate-service-override",
      "@codingame/monaco-vscode-theme-defaults-default-extension",
      "@codingame/monaco-vscode-theme-service-override",
      "@codingame/monaco-vscode-views-service-override",
      "@codingame/monaco-vscode-workbench-service-override",
    ],
  },
  server: {
    port: 5173,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 5174,
        }
      : undefined,
    watch: {
      ignored: ["**/src-tauri/**"],
    },
  },
}));
