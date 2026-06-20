import { useId } from "react";

/**
 * Outline gauge icon — stroke-only, used for the unselected state.
 *
 * SVG path source matches lucide-react's `Gauge` icon (ISC licensed):
 * a top-half arc (the dial) and a short diagonal line (the needle).
 */
export function OutlineGaugeIcon({ className }: { className?: string }) {
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
      <path d="m12 14 4-4" />
      <path d="M3.34 19a10 10 0 1 1 17.32 0" />
    </svg>
  );
}

/**
 * Filled gauge icon — used for the selected state.
 *
 * Naïvely fill-painting the lucide paths would:
 *   1. close the open top-half arc into a SOLID HALF-DISC (good — that
 *      gives us the body to fill);
 *   2. drown the needle line inside the same fill colour, erasing the
 *      one detail that makes it read as a "gauge".
 *
 * Restore the needle visibility with an SVG `<mask>`:
 *
 *   - mask starts fully white (everything visible);
 *   - we paint a black stroke along the needle, carving a thin groove
 *     through the half-disc fill so the needle reads as a negative
 *     (white-on-coloured) line — same trick as FilledDatabaseIcon's
 *     middle disk separator.
 *
 * The needle's outer endpoint (16, 10) sits very close to the dial
 * arc's outer edge (the arc surface passes through ≈ (16, 10.15)). To
 * avoid the carve groove eating into the body's outer stroke band
 * (which spills 0.875px past the geometry for a 1.75 stroke), we
 * inset the needle's outer endpoint to (15.5, 10.5) — about 0.7
 * units inside the arc — and use a 1.5px mask stroke. The groove now
 * stays comfortably inside the filled half-disc.
 *
 * The SVG also carries `stroke="currentColor" strokeWidth="1.75"`
 * matching the outline version so the filled state covers the same
 * visual area — without it, the fill is strictly inside the path
 * and the icon visibly shrinks. The same stroke is re-declared on
 * the inner `<g mask>` so mask-time rasterisation actually includes
 * the stroke region.
 *
 * `useId()` keeps the mask id unique when multiple instances render.
 */
export function FilledGaugeIcon({ className }: { className?: string }) {
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
          {/* Needle — carved as a white groove. Outer endpoint inset
              from (16, 10) to (15.5, 10.5) so the round cap stays
              clear of the dial arc's outer stroke band. */}
          <path
            d="M 12 14 L 15.5 10.5"
            fill="none"
            stroke="black"
            strokeWidth="1.5"
            strokeLinecap="round"
          />
          {/*
            Optional: a small "pivot dot" carved at the needle base so
            the gauge reads more clearly at small sizes. A 0.9-radius
            black disc subtracts a tiny white circle from the fill,
            mimicking the central pin of a real gauge. Centred at the
            needle origin (12, 14).
          */}
          <circle cx="12" cy="14" r="0.9" fill="black" />
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
        {/*
          Dial — the top-half arc. Filled (with implicit close from the
          right endpoint back to the left, i.e. a horizontal baseline
          along y=19) it becomes a solid half-disc, which is the body
          we want to fill.
        */}
        <path d="M3.34 19a10 10 0 1 1 17.32 0" />
        {/*
          Needle path is intentionally NOT rendered here as a stroked
          line — the mask carves the needle as negative space instead.
          Re-stroking it on top would paint currentColor over the
          carved white groove and undo the effect.
        */}
      </g>
    </svg>
  );
}
