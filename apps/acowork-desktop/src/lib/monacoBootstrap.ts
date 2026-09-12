// Monaco bootstrap — deferred so the ~170 kB monaco-editor bundle no
// longer blocks first paint (it used to be a top-level import in
// main.tsx, which made React wait for the whole module graph before
// mounting). `initMonaco()` kicks the import off in the background;
// FileEditorPanel awaits it before rendering <Editor>.
import { loader } from "@monaco-editor/react";

let initPromise: Promise<void> | null = null;

export function initMonaco(): Promise<void> {
  if (!initPromise) {
    initPromise = import("monaco-editor")
      .then((monaco) => {
        // Tell @monaco-editor/react to use the locally-installed
        // monaco-editor instead of loading scripts from CDN (which may
        // fail in Tauri's WebView).
        loader.config({ monaco });

        // Vite-compatible worker resolution: each language label maps to
        // a monaco-editor worker entry that Vite bundles separately.
        (window as any).MonacoEnvironment = {
          getWorker(_workerId: string, label: string) {
            switch (label) {
              case "json":
                return new Worker(
                  new URL("monaco-editor/esm/vs/language/json/json.worker.js", import.meta.url),
                  { type: "module" },
                );
              case "css":
              case "scss":
              case "less":
                return new Worker(
                  new URL("monaco-editor/esm/vs/language/css/css.worker.js", import.meta.url),
                  { type: "module" },
                );
              case "html":
              case "handlebars":
              case "razor":
                return new Worker(
                  new URL("monaco-editor/esm/vs/language/html/html.worker.js", import.meta.url),
                  { type: "module" },
                );
              case "typescript":
              case "javascript":
                return new Worker(
                  new URL("monaco-editor/esm/vs/language/typescript/ts.worker.js", import.meta.url),
                  { type: "module" },
                );
              default:
                return new Worker(
                  new URL("monaco-editor/esm/vs/editor/editor.worker.js", import.meta.url),
                  { type: "module" },
                );
            }
          },
        };
      });
  }
  return initPromise;
}
