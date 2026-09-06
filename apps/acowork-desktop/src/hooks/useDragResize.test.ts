/**
 * useDragResize test — verifies the drag-to-resize contract that the
 * ProjectsView / DocsView splitters rely on:
 *
 *   - Width initializes to the stored localStorage value (clamped), else
 *     to the default.
 *   - Dragging the handle right/left changes width by the pointer delta.
 *   - Width is clamped to [min, max] while dragging.
 *   - Releasing the pointer persists the final width.
 *
 * Drag is driven with real document-level mousemove/mouseup events, the
 * same events the hook listens to in production.
 */

import { describe, it, expect, beforeEach, afterEach } from "vitest";
import { renderHook, act } from "@testing-library/react";
import { useDragResize } from "./useDragResize";

const STORAGE_KEY = "test-drag-width";

function setup(overrides: Partial<{ defaultWidth: number; minWidth: number; maxWidth: number }> = {}) {
  return renderHook(() =>
    useDragResize({
      storageKey: STORAGE_KEY,
      defaultWidth: overrides.defaultWidth ?? 240,
      minWidth: overrides.minWidth ?? 100,
      maxWidth: overrides.maxWidth ?? 400,
    }),
  );
}

function mouseDown(handler: (e: React.MouseEvent<HTMLElement>) => void, clientX: number) {
  act(() => {
    handler({ clientX, preventDefault: () => {} } as unknown as React.MouseEvent<HTMLElement>);
  });
}

function mouseMove(clientX: number) {
  act(() => {
    document.dispatchEvent(new MouseEvent("mousemove", { clientX }));
  });
}

function mouseUp() {
  act(() => {
    document.dispatchEvent(new MouseEvent("mouseup"));
  });
}

beforeEach(() => {
  window.localStorage.clear();
});

afterEach(() => {
  document.body.style.userSelect = "";
});

describe("useDragResize", () => {
  it("uses the default width when nothing is stored", () => {
    const { result } = setup();
    expect(result.current.width).toBe(240);
  });

  it("restores the stored width, clamped into [min, max]", () => {
    window.localStorage.setItem(STORAGE_KEY, "999");
    const { result } = setup();
    expect(result.current.width).toBe(400);

    window.localStorage.setItem(STORAGE_KEY, "5");
    const { result: tooSmall } = setup();
    expect(tooSmall.current.width).toBe(100);
  });

  it("grows/shrinks width by the pointer delta while dragging", () => {
    const { result } = setup();
    mouseDown(result.current.onHandleMouseDown, 100);
    mouseMove(160);
    expect(result.current.width).toBe(300);
    mouseMove(40);
    expect(result.current.width).toBe(180);
    mouseUp();
  });

  it("clamps the width while dragging", () => {
    const { result } = setup();
    mouseDown(result.current.onHandleMouseDown, 100);
    mouseMove(1000);
    expect(result.current.width).toBe(400);
    mouseMove(-1000);
    expect(result.current.width).toBe(100);
    mouseUp();
  });

  it("persists the final width on mouseup", () => {
    const { result } = setup();
    mouseDown(result.current.onHandleMouseDown, 100);
    mouseMove(200);
    mouseUp();
    expect(window.localStorage.getItem(STORAGE_KEY)).toBe("340");
  });
});
