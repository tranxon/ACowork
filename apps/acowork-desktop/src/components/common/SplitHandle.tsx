/**
 * SplitHandle — vertical drag handle separating a fixed-width left column
 * from a fluid right area (Projects list / Docs tree, …).
 *
 * The markup (4px hover highlight, invisible until hover/drag) intentionally
 * mirrors the chat AgentList↔ChatPanel resize handle in AppLayout so every
 * split view in the app shares one visual grammar.
 */

interface SplitHandleProps {
  onMouseDown: (e: React.MouseEvent<HTMLElement>) => void;
  ariaLabel: string;
}

export function SplitHandle({ onMouseDown, ariaLabel }: SplitHandleProps) {
  return (
    <div
      role="separator"
      aria-orientation="vertical"
      aria-label={ariaLabel}
      onMouseDown={onMouseDown}
      className="group relative w-1 shrink-0 cursor-col-resize select-none"
    >
      <div className="absolute inset-y-0 left-0 w-1 rounded-full transition-colors group-hover:bg-[var(--color-accent)]/30 group-active:bg-[var(--color-accent)]/60" />
    </div>
  );
}
