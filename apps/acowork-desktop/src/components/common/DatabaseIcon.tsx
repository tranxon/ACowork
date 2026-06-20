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
 *   1. turn the top ellipse into a solid oval, erasing BOTH rims of the
 *      lid (front + back) and flattening the perspective;
 *   2. close the open middle arc into a thin lens shape, erasing the
 *      disk-separator line.
 *
 * We work around all three with an SVG `<mask>`:
 *
 *   - mask starts fully white (everything visible)
 *   - we paint a black stroke along the top BACK arc (upper half of the
 *     lid ellipse), carving a thin groove so the back rim stays visible;
 *   - same along the top FRONT arc (lower half of the lid ellipse), so
 *     the front rim — i.e. the line where the lid meets the body — also
 *     stays visible. Without this carve the lid would visually merge
 *     into the body and the cylinder would look like a flat blob.
 *   - same along the middle arc, restoring the disk separator.
 *
 * The mask is also extended slightly outside the 24×24 viewBox so it
 * covers the descender of the body's bottom curve and any stroke that
 * extends past the path itself (stroke is centered on the path, so the
 * outer 0.875px of the 1.75px stroke spills outside the geometry).
 *
 * The SVG carries `stroke="currentColor"` matching the outline version
 * so the filled state covers the same visual area — without it, the
 * fill is strictly inside the path and the icon visibly shrinks (by
 * strokeWidth/2 on each side) compared to the outline state. The same
 * stroke is also re-declared on the inner `<g>` to make sure mask-time
 * rasterisation actually includes the stroke region (some renderers
 * apply masks before inheriting parent stroke, which can clip the
 * outer stroke band).
 *
 * Net result: a solid cylinder with THREE visible "carved" lines (lid
 * back rim, lid front rim, middle disk), preserving the 3D feel that
 * the outline version has — and the same overall footprint.
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
        <mask id={maskId} maskUnits="userSpaceOnUse" x="-2" y="-2" width="28" height="28">
          {/* White = filled body shows through. Slightly oversized so the
              outer half of the body's stroke (which spills past the path)
              isn't accidentally clipped by the mask edge. */}
          <rect x="-2" y="-2" width="28" height="28" fill="white" />
          {/*
            Top lid BACK arc — carves a groove so the back rim stays visible.

            IMPORTANT: this arc is INSET from the original ellipse's top edge.
            The body's outer stroke spills 0.875px past the geometry (stroke
            is centered, width 1.75), so the visible top reaches roughly
            y ≈ 1.125. If we placed the carve groove right on the lid's back
            rim (y=2, the ellipse top), a 1.5px-wide black mask stroke would
            extend up to y ≈ 1.25, eating into the body's outer stroke band
            and making the icon look ~1px shorter than the outline version.

            Solution: shrink the carve arc so its highest point is around y=3,
            comfortably below the body's outer stroke band, and use a slightly
            narrower 1.5px mask stroke. The carve still reads as the lid's
            back rim because it sits inside the ellipse's filled area.
          */}
          <path
            d="M3.5 5 A 8.5 2 0 0 1 20.5 5"
            fill="none"
            stroke="black"
            strokeWidth="1.5"
            strokeLinecap="round"
          />
          {/* Top lid FRONT arc — carves a groove so the front rim
              (lid/body seam) stays visible. Placed exactly on the ellipse's
              bottom edge (y=8), well inside the icon, so no clipping risk. */}
          <path
            d="M3 5 A 9 3 0 0 0 21 5"
            fill="none"
            stroke="black"
            strokeWidth="1.5"
            strokeLinecap="round"
          />
          {/* Middle disk separator: carves a groove across the body. Sits
              well inside the silhouette, no clipping risk. */}
          <path
            d="M3 12A9 3 0 0 0 21 12"
            fill="none"
            stroke="black"
            strokeWidth="1.5"
            strokeLinecap="round"
          />
        </mask>
      </defs>
      <g
        mask={`url(#${maskId})`}
        fill="currentColor"
        stroke="currentColor"
        strokeWidth="1.75"
        strokeLinecap="round"
        strokeLinejoin="round"
      >
        <ellipse cx="12" cy="5" rx="9" ry="3" />
        <path d="M3 5V19A9 3 0 0 0 21 19V5" />
      </g>
    </svg>
  );
}