// src/components/common/ContextMenu/ContextMenu.tsx
//
// The single render target for every context menu in the app.
//
// Why this is a separate component (not just a JSX fragment in each site):
//
//   - Renders through `createPortal` to `document.body`. This is the only
//     way `position: fixed` reliably resolves against the **viewport** and
//     not the nearest `transform: translateY(...)` ancestor (a known issue
//     inside `VirtualMessageList` and the workspace file tree).
//
//   - Owns the close-on-action contract: after every item's `onClick`
//     returns, the menu is closed by calling `onClose()`. Callers never
//     have to wire that up themselves.
//
//   - Centralises the CSS class names. The five legacy implementations
//     each wrote `<button className="context-menu-item">` by hand — easy
//     to typo, easy to drift. Now there is one place that hard-codes the
//     class names against the rules in `styles/globals.css`.
//
//   - Disables the right-click default on the menu container itself. If
//     the user right-clicks the menu (a corner-case but observed when
//     selecting text inside), the browser default context menu would
//     otherwise pop on top of ours.
//
// The component is generic over the payload type so the TypeScript
// signature stays tight at every call site. Generic type-parameter
// forwarding through `forwardRef` is awkward, so this is a plain function
// component — the parent places the `ref` from `useContextMenu()` on the
// `menuProps.ref` slot.

import { createPortal } from "react-dom";
import type { CSSProperties, MouseEvent as ReactMouseEvent, RefObject } from "react";
import type {
  ContextMenuItem,
  ContextMenuItemVariant,
} from "./types";

export interface ContextMenuProps<TPayload> {
  /** True between `openAt` and `close`. */
  isOpen: boolean;
  /** The `{ ref, style }` pair produced by `useContextMenu().menuProps`. */
  menuProps: {
    ref: RefObject<HTMLDivElement | null>;
    style: CSSProperties & {
      left: number | undefined;
      top: number | undefined;
      visibility: "hidden" | "visible";
    };
  };
  /**
   * Declarative menu items. Build these with `useMemo` at the call site
   * so React.memo-wrapped parents (MessageBubble) don't see prop churn.
   */
  items: ContextMenuItem<TPayload>[];
  /** Payload captured at right-click time — propagated to onClick handlers. */
  payload: TPayload | undefined;
  /** Snapshot of the document selection at right-click time. */
  selectionAtOpen: string;
  /** Called when the menu should close (outside click, Escape, after-action). */
  onClose: () => void;
  /** When true, applies the narrower `context-menu--compact` width class. */
  compact?: boolean;
}

/**
 * Map from the item's `variant` to the CSS modifier class declared in
 * `styles/globals.css`. Centralised so a typo is caught at the JSX site.
 */
function variantClass(variant: ContextMenuItemVariant | undefined): string | undefined {
  switch (variant) {
    case "danger":
      return "context-menu-item--danger";
    case "warning":
      return "context-menu-item--warning";
    case "default":
    case undefined:
      return undefined;
  }
}

export function ContextMenu<TPayload>({
  isOpen,
  menuProps,
  items,
  payload,
  selectionAtOpen,
  onClose,
  compact,
}: ContextMenuProps<TPayload>) {
  // SSR / very-early-mount guard — portalTarget must exist.
  if (!isOpen || typeof document === "undefined") return null;

  // Wrap every item's onClick so the menu closes after the action runs.
  // Disabled items are short-circuited — same contract as the legacy
  // `invokeAndClose` helper that used to live inside MessageBubble.
  const closeAfter = (fn: (ctx: {
    event: ReactMouseEvent<HTMLButtonElement>;
    payload: TPayload | undefined;
    selectionAtOpen: string;
  }) => void | Promise<void>) => {
    return (event: ReactMouseEvent<HTMLButtonElement>) => {
      void Promise.resolve(fn({ event, payload, selectionAtOpen })).finally(() => {
        onClose();
      });
    };
  };

  return createPortal(
    <div
      ref={menuProps.ref}
      className={`context-menu${compact ? " context-menu--compact" : ""}`}
      style={menuProps.style}
      // Defensive: if the user right-clicks the menu itself (rare — they
      // would normally have to drag-select over it), keep the browser
      // context menu from popping on top of ours.
      onContextMenu={(e) => e.preventDefault()}
    >
      {items.map((item) => {
        const vClass = variantClass(item.variant);
        const className = `context-menu-item${vClass ? ` ${vClass}` : ""}`;
        return (
          // Fragment-with-key, NOT a wrapper div, so the rendered DOM is
          // exactly the same as before: optional `<div class="context-menu-divider">`
          // followed by a single `<button>`. Keeps the existing CSS
          // selector `.context-menu > .context-menu-item` intact.
          <ContextMenuRow key={item.key} item={item} className={className} onClick={closeAfter(item.onClick)} />
        );
      })}
    </div>,
    document.body,
  );
}

interface ContextMenuRowProps<TPayload> {
  item: ContextMenuItem<TPayload>;
  className: string;
  onClick: (event: ReactMouseEvent<HTMLButtonElement>) => void;
}

/**
 * One row of the menu. Split out so we can keep the divider-before logic
 * close to the row render and stay type-safe on `disabled` (the legacy
 * inline `<button>` copy-pasted the same conditional everywhere).
 */
function ContextMenuRow<TPayload>({
  item,
  className,
  onClick,
}: ContextMenuRowProps<TPayload>) {
  return (
    <>
      {item.dividerBefore && <div className="context-menu-divider" />}
      <button
        type="button"
        className={className}
        onClick={onClick}
        disabled={item.disabled}
        aria-disabled={item.disabled ? "true" : undefined}
        title={item.title}
      >
        {item.icon !== undefined && (
          <span className="context-menu-item__icon">{item.icon}</span>
        )}
        <span>{item.label}</span>
      </button>
    </>
  );
}