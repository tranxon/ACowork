import { useId } from "react";

/**
 * Outline database/cylinder icon — stroke-only, used for the unselected state.
 *
 * SVG path source matches lucide-react's `Database` icon (ISC licensed) so the
 * outline silhouette is identical to other right-nav icons that use lucide.
 */
export function OutlineDatabaseIcon({ className }: { className?: string }) {
  return (
    <svg
      className={className}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.75"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      {/* Top lid */}
      <ellipse cx="12" cy="5" rx="9" ry="3" />
      {/* Cylinder body (sides + bottom curve) */}
      <path d="M3 5V19A9 3 0 0 0 21 19V5" />
      {/* Middle disk separator — the line that gives the cylinder its 3D look */}
      <path d="M3 12A9 3 0 0 0 21 12" />
    </svg>
  );
}

/**
 * Filled database/cylinder icon — used for the selected state.
 *
 * A naïve `fill="currentColor"` on the lucide paths would:
 *   1. turn the top ellipse into a solid oval, erasing the "back rim" of
 *      the lid and flattening the perspective;
 *   2. close the open middle arc into a thin lens shape, erasing the
 *      disk-separator line.
 *
 * We work around both with an SVG `<mask>`:
 *
 *   - mask starts fully white (everything visible)
 *   - we paint a black stroke along the top back arc, carving a thin
 *     groove through the fill so the parent's background shows through
 *     and the "lid back rim" stays visible;
 *   - same trick along the middle arc, restoring the disk separator.
 *
 * The SVG also carries a `stroke="currentColor"` matching the outline
 * version so the filled state covers the same visual area — without it,
 * the fill is strictly inside the path and the icon visibly shrinks
 * (by strokeWidth/2 on each side) compared to the outline state.
 *
 * Net result: a solid cylinder with two visible "carved" lines (lid
 * back rim and middle disk), preserving the 3D feel that the outline
 * version has — and the same overall footprint.
 *
 * `useId()` keeps the mask id unique when multiple instances are rendered.
 */
export function FilledDatabaseIcon({ className }: { className?: string }) {
  const maskId = useId();
  return (
    <svg
      className={className}
      viewBox="0 0 24 24"
      fill="currentColor"
      stroke="currentColor"
      strokeWidth="1.75"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      <defs>
        <mask id={maskId}>
          {/* White = filled body shows through */}
          <rect width="24" height="24" fill="white" />
          {/* Top "lid" back arc: carves a groove so the back rim stays visible */}
          <path
            d="M3 5 A 9 3 0 0 1 21 5"
            fill="none"
            stroke="black"
            strokeWidth="2"
            strokeLinecap="round"
          />
          {/* Middle disk separator: carves a groove across the body */}
          <path
            d="M3 12A9 3 0 0 0 21 12"
            fill="none"
            stroke="black"
            strokeWidth="2"
            strokeLinecap="round"
          />
        </mask>
      </defs>
      <g mask={`url(#${maskId})`}>
        <ellipse cx="12" cy="5" rx="9" ry="3" />
        <path d="M3 5V19A9 3 0 0 0 21 19V5" />
      </g>
    </svg>
  );
}