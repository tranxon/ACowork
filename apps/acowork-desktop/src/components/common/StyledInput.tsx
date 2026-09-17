import React from "react";

/**
 * Unified input field component with consistent focus style:
 * thin accent-colored border on focus (no ring / outline).
 */
interface StyledInputProps extends React.InputHTMLAttributes<HTMLInputElement> {
  /** Use monospace font (for API keys, code, etc.) */
  fontMono?: boolean;
}

export const StyledInput = React.forwardRef<HTMLInputElement, StyledInputProps>(
  ({ fontMono, className = "", ...props }, ref) => {
    return (
      <input
        ref={ref}
        className={`w-full rounded-md border border-zinc-200 px-3 py-[var(--ui-input-py)] text-xs outline-none transition-colors focus:border-[var(--color-accent)] dark:border-zinc-700 dark:bg-zinc-900  ${
          fontMono ? "font-mono" : ""
        } ${className}`}
        {...props}
      />
    );
  },
);
StyledInput.displayName = "StyledInput";

/**
 * Unified textarea with consistent focus style.
 */
interface StyledTextareaProps
  extends React.TextareaHTMLAttributes<HTMLTextAreaElement> {
  /** Use monospace font */
  fontMono?: boolean;
}

export const StyledTextarea = React.forwardRef<
  HTMLTextAreaElement,
  StyledTextareaProps
>(({ fontMono, className = "", ...props }, ref) => {
  return (
    <textarea
      ref={ref}
      className={`w-full resize-y rounded-md border border-zinc-200 px-3 py-[var(--ui-input-py)] text-xs outline-none transition-colors focus:border-[var(--color-accent)] dark:border-zinc-700 dark:bg-zinc-900  ${
        fontMono ? "font-mono" : ""
      } ${className}`}
      {...props}
    />
  );
});
StyledTextarea.displayName = "StyledTextarea";
