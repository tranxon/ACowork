/**
 * Self-check for `lib/monacoBootstrap`.
 *
 * Two properties that are easy to break and stay invisible until an editor
 * fails to open on a user machine:
 *
 *  1. `window.MonacoEnvironment` must be AUGMENTED, never replaced. `lspUtils`
 *     and monaco-languageclient's `useWorkerFactory` keep their own state on
 *     that same global, and our write lands asynchronously (after a dynamic
 *     import) — a wholesale replacement can wipe state written by a racing
 *     LSP start.
 *  2. A failed import must not be cached as a rejection. The failure is
 *     transient (vite dep-hash rotation, see vite.config.ts), and caching it
 *     turns "restart the dev server" into "restart the app".
 *
 * Run: npx vitest run src/lib/monacoBootstrap.test.ts
 */
import { describe, it, expect, vi, beforeEach } from "vitest";

const h = vi.hoisted(() => ({ fail: true }));

vi.mock("monaco-editor", () => {
  if (h.fail) throw new Error("Failed to fetch dynamically imported module");
  return { editor: {}, languages: {}, Uri: {} };
});
vi.mock("@monaco-editor/react", () => ({ loader: { config: vi.fn() } }));

/** Import a fresh copy so the module-level `initPromise` starts as null. */
async function freshBootstrap() {
  vi.resetModules();
  return import("./monacoBootstrap");
}

type Env = Record<string, unknown>;

describe("initMonaco", () => {
  beforeEach(() => {
    h.fail = true;
    delete (window as unknown as { MonacoEnvironment?: unknown }).MonacoEnvironment;
  });

  it("starts a new attempt after a failed import instead of re-throwing the cached rejection", async () => {
    const { initMonaco } = await freshBootstrap();

    const failed = initMonaco();
    await expect(failed).rejects.toThrow();

    // The property under test is that the rejected promise was dropped from
    // the cache, so the caller gets a fresh attempt. Whether that attempt can
    // succeed depends on the runner (Node caches a failed module evaluation,
    // browsers retry a failed fetch) — the `rejects` below is just the
    // precondition that we did get a failed promise back, not a retry test.
    const retry = initMonaco();
    expect(retry).not.toBe(failed);
    await expect(retry).rejects.toThrow();
  });

  it("coalesces concurrent callers onto one import", async () => {
    h.fail = false;
    const { initMonaco } = await freshBootstrap();

    const a = initMonaco();
    const b = initMonaco();
    expect(b).toBe(a);
    await expect(a).resolves.toBeUndefined();
  });

  it("augments an existing MonacoEnvironment instead of replacing it", async () => {
    h.fail = false;
    const { initMonaco } = await freshBootstrap();

    const preexisting: Env = {
      vscodeApiInitialising: true,
      viewServiceType: "EditorService",
    };
    (window as unknown as { MonacoEnvironment: Env }).MonacoEnvironment = preexisting;

    await initMonaco();

    const env = (window as unknown as { MonacoEnvironment: Env }).MonacoEnvironment;
    expect(env).toBe(preexisting); // same object, not a fresh one
    expect(env.vscodeApiInitialising).toBe(true); // LSP state survived
    expect(env.viewServiceType).toBe("EditorService");
    expect(typeof env.getWorker).toBe("function"); // monaco wiring installed
  });
});
