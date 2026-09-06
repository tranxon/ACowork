/**
 * useDragResize — horizontal splitter drag state for resizable left-column
 * panels (Projects list, Docs tree, …).
 *
 * Mirrors the pointer-driven resize logic that AppLayout inlines for the
 * chat AgentList / RightPanel / FileEditorPanel splitters (startX +
 * startWidth refs, document-level mousemove/mouseup, `userSelect: none`
 * while dragging, clamp to [min,max], persist final width on mouseup).
 * Extracting it here lets each new split view share one implementation
 * instead of a fourth inline copy.
 *
 * The hook only manages the numeric width + the pointer lifecycle; the
 * caller renders the actual draggable handle (see SplitHandle) and applies
 * the returned `width` to the left column.
 */

import { useCallback, useEffect, useRef, useState } from "react";

export interface UseDragResizeOptions {
  /** localStorage key for persisting the width across sessions. */
  storageKey: string;
  /** Width used when nothing is stored yet. */
  defaultWidth: number;
  minWidth: number;
  maxWidth: number;
}

export function useDragResize({
  storageKey,
  defaultWidth,
  minWidth,
  maxWidth,
}: UseDragResizeOptions): {
  width: number;
  onHandleMouseDown: (e: React.MouseEvent<HTMLElement>) => void;
} {
  const [width, setWidth] = useState<number>(() => {
    const stored = window.localStorage.getItem(storageKey);
    if (stored === null) return defaultWidth;
    const parsed = Number.parseInt(stored, 10);
    if (!Number.isFinite(parsed)) return defaultWidth;
    return Math.min(Math.max(parsed, minWidth), maxWidth);
  });

  // Refs so the document-level move/up handlers never go stale.
  const widthRef = useRef(width);
  widthRef.current = width;
  const draggingRef = useRef(false);
  const startXRef = useRef(0);
  const startWidthRef = useRef(defaultWidth);
  const minRef = useRef(minWidth);
  const maxRef = useRef(maxWidth);

  const handleMouseMove = useCallback((e: MouseEvent) => {
    if (!draggingRef.current) return;
    e.preventDefault();
    const raw = startWidthRef.current + (e.clientX - startXRef.current);
    const clamped = Math.min(Math.max(raw, minRef.current), maxRef.current);
    widthRef.current = clamped;
    setWidth(clamped);
  }, []);

  const handleMouseUp = useCallback(() => {
    if (!draggingRef.current) return;
    draggingRef.current = false;
    document.body.style.userSelect = "";
    document.removeEventListener("mousemove", handleMouseMove);
    document.removeEventListener("mouseup", handleMouseUp);
    window.localStorage.setItem(storageKey, String(widthRef.current));
  }, [handleMouseMove, storageKey]);

  const onHandleMouseDown = useCallback(
    (e: React.MouseEvent<HTMLElement>) => {
      e.preventDefault();
      draggingRef.current = true;
      startXRef.current = e.clientX;
      startWidthRef.current = widthRef.current;
      document.body.style.userSelect = "none";
      document.addEventListener("mousemove", handleMouseMove);
      document.addEventListener("mouseup", handleMouseUp);
    },
    [handleMouseMove, handleMouseUp],
  );

  // Cleanup if the view unmounts mid-drag (avoids leaking listeners).
  useEffect(
    () => () => {
      document.removeEventListener("mousemove", handleMouseMove);
      document.removeEventListener("mouseup", handleMouseUp);
    },
    [handleMouseMove, handleMouseUp],
  );

  return { width, onHandleMouseDown };
}
