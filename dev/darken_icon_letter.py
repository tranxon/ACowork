"""
Replace the white "A" letter in the ACowork app icon set with the dark variant (#282828).

Why on the PNG instead of re-rendering from the SVG:
  No SVG rasterizer is installed in this environment (no cairo / sharp / resvg).
  The existing icons were flattened onto the yellow chip (alpha is 0 or 255 only),
  so we identify the glyph by luma and re-derive each channel as
      new = bg + (fg_new - bg) * alpha
  where alpha is the per-pixel white contribution recovered from luma.
  This keeps the anti-aliased fringe looking correct on every output size.

ponytail: the bg luma anchor (202) is a single hard-coded constant, fine because
the yellow chip is a fixed gradient. If the brand mark ever changes the chip colour,
this number has to move with it.
"""

from __future__ import annotations

import sys
from pathlib import Path

import numpy as np
from PIL import Image

ICON_DIR = Path(__file__).resolve().parent.parent / "apps" / "acowork-desktop" / "src-tauri" / "icons"

# Background yellow gradient extremes (from brand-mark.svg):
#   stop 0: #F7C948 = (247, 201,  72)  -> luma 199.9
#   stop 1: #F0A92E = (240, 169,  46)  -> luma 180.7
# Anchor for "fully background" = the brighter end of the gradient; pixels above this
# luma must contain some white contribution. 202 is a safe headroom over the gradient.
BG_LUMA = 202.0
FG_LUMA = 255.0
FG_OLD = 255      # white "A" (255,255,255)
FG_NEW = 0x28     # dark "A" (#282828 = 40,40,40)
ALPHA_MIN = 0.15  # skip pixels where the white contribution is negligible — keeps the
                  # yellow chip untouched on the bright end of its gradient.


def darken_glyph(rgba: np.ndarray) -> np.ndarray:
    r = rgba[..., 0].astype(np.float32)
    g = rgba[..., 1].astype(np.float32)
    b = rgba[..., 2].astype(np.float32)
    a = rgba[..., 3]

    luma = 0.299 * r + 0.587 * g + 0.114 * b
    alpha = np.clip((luma - BG_LUMA) / (FG_LUMA - BG_LUMA), 0.0, 1.0)
    mask = alpha > ALPHA_MIN

    delta = (FG_OLD - FG_NEW) * alpha  # how much each channel drops

    out = rgba.copy()
    out[..., 0] = np.where(mask, np.clip(r - delta, 0, 255), r).astype(np.uint8)
    out[..., 1] = np.where(mask, np.clip(g - delta, 0, 255), g).astype(np.uint8)
    out[..., 2] = np.where(mask, np.clip(b - delta, 0, 255), b).astype(np.uint8)
    out[..., 3] = a.astype(np.uint8) if a.dtype != np.uint8 else a
    return out


def process_png(path: Path) -> None:
    im = Image.open(path).convert("RGBA")
    arr = np.array(im)
    new_arr = darken_glyph(arr)
    im_out = Image.fromarray(new_arr, mode="RGBA")
    im_out.save(path, format="PNG")


def process_ico(path: Path) -> None:
    im = Image.open(path)
    frames = []
    sizes = []
    try:
        for i in range(getattr(im, "n_frames", 1)):
            im.seek(i)
            sizes.append(im.size)
            arr = np.array(im.convert("RGBA"))
            frames.append(Image.fromarray(darken_glyph(arr), mode="RGBA"))
    except EOFError:
        pass
    if not frames:
        frames = [Image.fromarray(darken_glyph(np.array(im.convert("RGBA"))), mode="RGBA")]
        sizes = [im.size]
    # PIL writes one ICO frame per entry in `sizes`; we feed the rebuilt frames so each
    # one carries the darkened glyph. The largest size is the canonical one.
    base = frames[-1]
    base.save(
        path,
        format="ICO",
        sizes=[(s.width, s.height) for s in frames] if len(frames) > 1 else [(sizes[-1][0], sizes[-1][1])],
    )


def main() -> int:
    targets_png = sorted(ICON_DIR.glob("*.png"))
    targets_ico = sorted(ICON_DIR.glob("*.ico"))
    if not targets_png:
        print(f"no PNGs found under {ICON_DIR}", file=sys.stderr)
        return 1
    for p in targets_png:
        process_png(p)
        print(f"  ok  {p.name}")
    for p in targets_ico:
        process_ico(p)
        print(f"  ok  {p.name}  (multi-frame)")
    icns = sorted(ICON_DIR.glob("*.icns"))
    if icns:
        print(f"  skip {icns[0].name}  (PIL has no icns encoder; rerun `npm run tauri icon` if you need it refreshed)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
