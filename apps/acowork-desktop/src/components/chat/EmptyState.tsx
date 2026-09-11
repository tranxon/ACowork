import brandMarkGray from "../../../../../assets/brand-mark-gray.svg";

/**
 * EmptyState — placeholder shown inside the chat surface when the current
 * session has zero messages. Kept visually quiet so it never competes with
 * actual conversation content once the user starts typing.
 *
 * Layout: brand mark on top, small uppercase tagline below.
 *
 * Visual hierarchy (intentional, both themes):
 *   - Logo      : 20% opacity, ~44px tall, brand identity (passive)
 *   - Tagline   : 40% opacity, 10px, bold + wide tracking, functional hint
 * The tagline is *more* opaque than the logo so it stays legible — a
 * completely uniform 20% would render bold uppercase glyphs as an
 * unreadable gray smudge. Logo still leads the eye; tagline earns its
 * legibility budget.
 *
 * Uses the grayscale brand mark (brand-mark-gray.svg), which is itself
 * theme-aware via an inline <style> + prefers-color-scheme query, so this
 * component needs no React-side theme handling. The tagline relies on
 * `text-zinc-500/40` (Tailwind opacity modifier) so it inherits the
 * surrounding surface tone in both light and dark modes.
 */
export function EmptyState(): React.ReactElement {
  return (
    <div
      className="absolute inset-0 flex flex-col items-center justify-center gap-3"
      // Pointer events stay on (default); the empty surface must still
      // receive clicks for things like "scroll to bottom" if added later.
      aria-label="ACowork"
    >
      <img
        src={brandMarkGray}
        alt=""
        width={180}
        height={44}
        // Don't announce the decorative mark to screen readers — the
        // surrounding chat surface already exposes the app context.
        aria-hidden="true"
        draggable={false}
        // maxWidth guards against future viewBox tweaks blowing up the
        // placeholder if the asset is ever swapped.
        style={{ maxWidth: "60%", height: "auto", opacity: 0.2 }}
      />
      <p className="text-[10px] font-bold uppercase tracking-[0.25em] text-zinc-500/40 dark:text-zinc-400/40">
        Talk with your AI colleague
      </p>
    </div>
  );
}
