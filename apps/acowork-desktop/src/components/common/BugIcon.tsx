import { useId } from "react";

/**
 * Outline bug icon — stroke-only, used for the unselected state.
 *
 * Path data matches lucide-react's `Bug` icon (ISC licensed):
 * a rounded body, two antennae, a top-of-head arc, six legs, and a
 * vertical spine line down the centre of the body.
 */
export function OutlineBugIcon({ className }: { className?: string }) {
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
      {/* Antennae */}
      <path d="m8 2 1.88 1.88" />
      <path d="M14.12 3.88 16 2" />
      {/* Top-of-head arc */}
      <path d="M9 7.13v-1a3.003 3.003 0 1 1 6 0v1" />
      {/* Body */}
      <path d="M12 20c-3.3 0-6-2.7-6-6v-3a4 4 0 0 1 4-4h4a4 4 0 0 1 4 4v3c0 3.3-2.7 6-6 6" />
      {/* Spine */}
      <path d="M12 20v-9" />
      {/* Legs (3 left, 3 right) */}
      <path d="M6.53 9C4.6 8.8 3 7.1 3 5" />
      <path d="M6 13H2" />
      <path d="M3 21c0-2.1 1.7-3.9 3.8-4" />
      <path d="M20.97 5c0 2.1-1.6 3.8-3.5 4" />
      <path d="M22 13h-4" />
      <path d="M17.2 17c2.1.1 3.8 1.9 3.8 4" />
    </svg>
  );
}

/**
 * Filled bug icon — used for the selected state.
 *
 * Naïvely setting `fill="currentColor"` on lucide's path list would:
 *   1. fill the body shape solid (good — that's the silhouette we want);
 *   2. drown the SPINE line (`M12 20v-9`) inside the same fill, erasing
 *      the central detail that gives the bug its segmented look.
 *
 * The other parts (antennae, head arc, legs) are line/curve paths
 * outside the body's filled region, so they remain visible as strokes.
 *
 * We restore the spine via an SVG `<mask>`:
 *
 *   - mask starts fully white (everything visible);
 *   - paint a black stroke along the spine, carving a thin groove
 *     through the body fill so the spine reads as a negative
 *     (white-on-coloured) line — same trick as FilledDatabaseIcon's
 *     middle disk separator and FilledGaugeIcon's needle.
 *
 * The spine endpoints (12, 11) and (12, 20) sit safely inside the
 * body, far from any outer stroke band, so we can use a slightly
 * thicker 1.75px mask stroke for better visibility at small sizes
 * without risking outer-edge clipping.
 *
 * The SVG also carries `stroke="currentColor" strokeWidth="1.75"`
 * matching the outline version so the filled state covers the same
 * visual area — without it, the fill is strictly inside the path
 * and the icon visibly shrinks. The same stroke is re-declared on
 * the inner `<g mask>` so mask-time rasterisation actually includes
 * the stroke region.
 *
 * Spine path is intentionally NOT rendered inside the masked group —
 * the mask carves it as negative space; rendering it on top would
 * paint currentColor over the carved white groove and undo the effect.
 *
 * `useId()` keeps the mask id unique when multiple instances render.
 */
export function FilledBugIcon({ className }: { className?: string }) {
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
        <mask
          id={maskId}
          maskUnits="userSpaceOnUse"
          x="-2"
          y="-2"
          width="28"
          height="28"
        >
          {/* White = filled body shows through. Slightly oversized so the
              outer half of the body's stroke (which spills past the path)
              isn't accidentally clipped by the mask edge. */}
          <rect x="-2" y="-2" width="28" height="28" fill="white" />
          {/* Spine carved as a white groove. Endpoints are well inside
              the body, no inset needed beyond the natural path coords. */}
          <path
            d="M 12 11 L 12 20"
            fill="none"
            stroke="black"
            strokeWidth="1.75"
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
        {/* Antennae (line strokes — no fill area) */}
        <path d="m8 2 1.88 1.88" />
        <path d="M14.12 3.88 16 2" />
        {/* Top-of-head arc */}
        <path d="M9 7.13v-1a3.003 3.003 0 1 1 6 0v1" />
        {/* Body — filled */}
        <path d="M12 20c-3.3 0-6-2.7-6-6v-3a4 4 0 0 1 4-4h4a4 4 0 0 1 4 4v3c0 3.3-2.7 6-6 6" />
        {/* Legs (line strokes — no fill area) */}
        <path d="M6.53 9C4.6 8.8 3 7.1 3 5" />
        <path d="M6 13H2" />
        <path d="M3 21c0-2.1 1.7-3.9 3.8-4" />
        <path d="M20.97 5c0 2.1-1.6 3.8-3.5 4" />
        <path d="M22 13h-4" />
        <path d="M17.2 17c2.1.1 3.8 1.9 3.8 4" />
        {/*
          Spine path intentionally omitted — the mask carves it as
          negative space. Adding a stroked line here would paint
          currentColor over the carved groove and undo the effect.
        */}
      </g>
    </svg>
  );
}
