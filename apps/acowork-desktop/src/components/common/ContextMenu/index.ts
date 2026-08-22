// src/components/common/ContextMenu/index.ts
//
// Single entry point for the unified context-menu subsystem. Every call
// site imports from this barrel — never from the individual files — so
// the public surface stays small and renames don't cascade.

export { ContextMenu } from "./ContextMenu";
export type { ContextMenuProps } from "./ContextMenu";
export { useContextMenu } from "./useContextMenu";
export type { UseContextMenuResult } from "./useContextMenu";
export type {
  ContextMenuItem,
  ContextMenuClickContext,
  ContextMenuItemVariant,
} from "./types";