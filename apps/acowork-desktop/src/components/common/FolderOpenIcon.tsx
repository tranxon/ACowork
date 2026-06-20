import { useId } from "react";

/**
 * Outline open-folder icon — stroke-only, used for the unselected state.
 *
 * SVG path source matches lucide-react's `FolderOpen` icon (ISC licensed)
 * so the silhouette is identical to the WorkspaceSelector button in the
 * chat input toolbar (which renders the same lucide icon directly).
 */
export function OutlineFolderOpenIcon({ className }: { className?: string }) {
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
      <path d="m6 14 1.5-2.9A2 2 0 0 1 9.24 10H20a2 2 0 0 1 1.94 2.5l-1.54 6a2 2 0 0 1-1.95 1.5H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h3.9a2 2 0 0 1 1.69.9l.81 1.2a2 2 0 0 0 1.67.9H18a2 2 0 0 1 2 2v2" />
    </svg>
  );
}

/**
 * Filled open-folder icon — used for the selected state.
 *
 * The lucide `FolderOpen` path is a single self-intersecting outline that
 * traces both the back lid (trapezoidal tab + top plate) and the front
 * lid (slanted scoop) in one stroke. A naïve `fill="currentColor"` fills
 * the entire enclosed region as one solid blob, completely erasing the
 * "open" visual cue (you can no longer see the front lid sitting on top
 * of the back lid).
 *
 * We restore the open-lid look using an SVG `<mask>`:
 *
 *   - mask starts fully white (everything visible);
 *   - we paint a black stroke along the FRONT-LID TOP edge — i.e. the
 *     line from the top of the left scoop slant to the top-right corner
 *     — carving a thin groove so the seam between the two lids stays
 *     visible;
 *   - we paint another black stroke along the FRONT-LID LEFT slant for
 *     the same reason: it's the most recognisable "open scoop" line in
 *     the outline version.
 *
 * Both carve segments are INSET from the geometry endpoints by ~0.5px
 * so their round caps don't extend into the icon's outer stroke band
 * (a stroke-width 1.75 fill spills 0.875px past the path; placing the
 * carve right at the corner would eat that band and make the filled
 * version look ~1px smaller than the outline version).
 *
 * The SVG also carries `stroke="currentColor" strokeWidth="1.75"`
 * matching the outline version so the filled state covers the same
 * visual area — without it, the fill is strictly inside the path and
 * the icon visibly shrinks (by strokeWidth/2 on each side).
 *
 * The same stroke is re-declared on the inner `<g mask>` to make sure
 * mask-time rasterisation actually includes the stroke region (some
 * renderers apply masks before inheriting parent stroke, which can
 * clip the outer band).
 *
 * `useId()` keeps the mask id unique when multiple instances render.
 */
export function FilledFolderOpenIcon({ className }: { className?: string }) {
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
          {/*
            Front-lid TOP edge — the horizontal seam between the back lid
            and the front lid, the single most important line for the
            "open folder" read. Inset 0.5px on each side so the round cap
            stays clear of the right-corner arc and the left scoop join.
          */}
          <path
            d="M 9.7 10 L 19.5 10"
            fill="none"
            stroke="black"
            strokeWidth="1.5"
            strokeLinecap="round"
          />
          {/*
            Front-lid LEFT slant — the diagonal "scoop" edge from the
            bottom-left of the front lid up to where it meets the back
            lid. Endpoints inset slightly from (6,14) and (9.24,10) so
            the carve doesn't bleed into the body's outer stroke band.
          */}
          <path
            d="M 6.3 13.6 L 9.0 10.4"
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
        <path d="m6 14 1.5-2.9A2 2 0 0 1 9.24 10H20a2 2 0 0 1 1.94 2.5l-1.54 6a2 2 0 0 1-1.95 1.5H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h3.9a2 2 0 0 1 1.69.9l.81 1.2a2 2 0 0 0 1.67.9H18a2 2 0 0 1 2 2v2" />
      </g>
    </svg>
  );
}
