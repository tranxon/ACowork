/**
 * Self-check for `ChunkLoadBoundary` and the retry shape AppLayout uses.
 *
 * The editor panel goes through `React.lazy`, and a failed dynamic import
 * (dev server restarted, transient fetch error) rejects *during render*
 * instead of just suspending. Two properties matter here and both stay
 * invisible until a chunk really fails:
 *
 *  1. The rejection must be caught by the scoped boundary. The next boundary
 *     up is the app-level one in App.tsx, whose fallback covers the whole
 *     window — a leaf panel must not be able to blank the app.
 *  2. `React.lazy` caches its rejected promise, so re-rendering the same lazy
 *     component never recovers and never re-fetches. Only building a new lazy
 *     component — what the Retry button does — can load successfully.
 */
import { lazy, Suspense, useMemo, useState } from "react";
import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { ChunkLoadBoundary } from "./ErrorBoundary";

const Editor = () => <div>editor ready</div>;
type Load = () => Promise<{ default: typeof Editor }>;

/** Mirrors AppLayout: lazy component rebuilt per attempt, boundary keyed by attempt. */
function Harness({ load }: { load: Load }) {
  const [attempt, setAttempt] = useState(0);
  const Lazy = useMemo(() => lazy(load), [attempt]);
  return (
    <ChunkLoadBoundary
      key={attempt}
      fallback={<button onClick={() => setAttempt((n) => n + 1)}>retry</button>}
    >
      <Suspense fallback={<div>loading</div>}>
        <Lazy />
      </Suspense>
    </ChunkLoadBoundary>
  );
}

/** Same lazy object re-rendered forever — what a plain "reset the boundary" retry would do. */
function SameLazyHarness({ load }: { load: Load }) {
  const Lazy = useMemo(() => lazy(load), []);
  const [, bump] = useState(0);
  return (
    <>
      <button onClick={() => bump((n) => n + 1)}>rerender</button>
      <ChunkLoadBoundary fallback={<div>panel failed</div>}>
        <Suspense fallback={<div>loading</div>}>
          <Lazy />
        </Suspense>
      </ChunkLoadBoundary>
    </>
  );
}

describe("ChunkLoadBoundary", () => {
  it("catches a rejected lazy import instead of letting it reach the app", async () => {
    const load = vi.fn(() =>
      Promise.reject(new Error("Failed to fetch dynamically imported module")),
    );

    render(<Harness load={load} />);

    expect(await screen.findByRole("button", { name: "retry" })).toBeTruthy();
    expect(screen.queryByText("editor ready")).toBeNull();
    expect(load).toHaveBeenCalledTimes(1);
  });

  it("recovers when retry builds a new lazy component", async () => {
    let calls = 0;
    const load = vi.fn<Load>(() => {
      calls += 1;
      return calls === 1
        ? Promise.reject(new Error("Failed to fetch dynamically imported module"))
        : Promise.resolve({ default: Editor });
    });

    render(<Harness load={load} />);
    fireEvent.click(await screen.findByRole("button", { name: "retry" }));

    expect(await screen.findByText("editor ready")).toBeTruthy();
    expect(load).toHaveBeenCalledTimes(2);
  });

  it("does not recover by re-rendering the same lazy component", async () => {
    const load = vi.fn(() =>
      Promise.reject(new Error("Failed to fetch dynamically imported module")),
    );

    render(<SameLazyHarness load={load} />);
    expect(await screen.findByText("panel failed")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "rerender" }));

    // Still failed AND still only one import attempt: the rejection is cached
    // by React, not re-fetched.
    expect(screen.getByText("panel failed")).toBeTruthy();
    expect(load).toHaveBeenCalledTimes(1);
  });
});
