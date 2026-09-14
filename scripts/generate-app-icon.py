#!/usr/bin/env python3
"""Generate the source app-icon.png from the yellow chip in brand-mark.svg.

Design (in chip's 84x84 coord space, from brand-mark.svg):
  - Yellow rounded square (rx=20) with linear gradient #F7C948 -> #F0A92E
    along the top-left to bottom-right diagonal.
  - Black "A" path M32 68 L56 20 L80 68, stroke #282828, stroke-width 10,
    round line caps + joins.
  - Black circle cx=56 cy=56 r=8.5 (the crossbar stand-in).
  - A tiny "bite" hole at cx=62 cy=56 r=3 that reveals the yellow gradient
    underneath (subtle Co wordmark echo).
  - No drop shadow (OS renders its own for app icons).
"""
import math
from pathlib import Path
from PIL import Image, ImageDraw
import numpy as np

CHIP_TOP = (0xF7, 0xC9, 0x48)
CHIP_BOT = (0xF0, 0xA9, 0x2E)
STROKE = (0x28, 0x28, 0x28)

# brand-mark.svg places the chip at (14, 2) in the SVG's 396x96 viewBox.
# All A / circle / bite coordinates below are SVG-absolute; we subtract
# this origin to convert into the chip's local 0..84 coordinate space.
CHIP_ORIGIN = (14, 2)
CHIP_SIDE = 84  # chip is 84x84 in the SVG
CHIP_RX = 20  # rounded corner radius (in chip-local units)
A_PTS = [(32, 68), (56, 20), (80, 68)]  # M32 68 L56 20 L80 68, SVG-absolute
A_W = 10  # stroke-width (in chip-local units)
CIRCLE = (56, 56, 8.5)  # cx, cy, r, SVG-absolute
BITE = (62, 56, 3)  # cx, cy, r, SVG-absolute


def gradient_size(size: int) -> Image.Image:
    """Diagonal linear gradient (top-left -> bottom-right), full alpha."""
    ys = np.arange(size, dtype=np.float64).reshape(-1, 1)
    xs = np.arange(size, dtype=np.float64).reshape(1, -1)
    t = (xs + ys) / (2 * (size - 1)) if size > 1 else np.zeros((1, 1))
    t = np.clip(t, 0.0, 1.0)
    top = np.array(CHIP_TOP, dtype=np.float64)
    bot = np.array(CHIP_BOT, dtype=np.float64)
    rgb = ((1 - t)[..., None] * top + t[..., None] * bot).astype(np.uint8)
    alpha = np.full((size, size, 1), 255, dtype=np.uint8)
    arr = np.concatenate([rgb, alpha], axis=-1)
    return Image.fromarray(arr, "RGBA")


def apply_rounded_mask(img: Image.Image, radius: int) -> Image.Image:
    """Multiply alpha channel by a rounded-rect mask of given corner radius."""
    size = img.size[0]
    mask = Image.new("L", (size, size), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        (0, 0, size - 1, size - 1), radius=radius, fill=255
    )
    img.putalpha(mask)
    return img


def draw_a(img: Image.Image, s: float) -> None:
    """Draw the A polyline with stroke width, round caps + joins."""
    pts = [to_local(p, s) for p in A_PTS]
    sw = A_W * s
    draw = ImageDraw.Draw(img)
    # width must be int for Pillow's line(), but for big sizes we want sub-pixel accuracy;
    # round to nearest int >= 1 (any pixel error is hidden by the round caps below).
    sw_int = max(1, int(round(sw)))
    draw.line(pts, fill=STROKE, width=sw_int, joint="curve")
    # Round caps + round join: paint a filled circle of radius sw/2 at every vertex.
    r = sw / 2
    for x, y in pts:
        draw.ellipse((x - r, y - r, x + r, y + r), fill=STROKE)


def draw_circle(img: Image.Image, s: float) -> None:
    """Draw the central black circle (A's crossbar stand-in)."""
    cx_svg, cy_svg, rr = CIRCLE
    cx, cy = to_local((cx_svg, cy_svg), s)
    rr = rr * s
    draw = ImageDraw.Draw(img)
    draw.ellipse((cx - rr, cy - rr, cx + rr, cy + rr), fill=STROKE)


def draw_bite(img: Image.Image, s: float) -> None:
    """Carve a small bite hole that reveals the yellow gradient underneath."""
    bx_svg, by_svg, br = BITE
    bx, by = to_local((bx_svg, by_svg), s)
    br = br * s
    # Sample the gradient at the bite center.
    size = img.size[0]
    t = (bx + by) / (2 * (size - 1)) if size > 1 else 0.0
    t = max(0.0, min(1.0, t))
    color = tuple(
        int(round((1 - t) * CHIP_TOP[i] + t * CHIP_BOT[i])) for i in range(3)
    )
    draw = ImageDraw.Draw(img)
    draw.ellipse((bx - br, by - br, bx + br, by + br), fill=color)


def to_local(svg_pt: tuple[float, float], s: float) -> tuple[float, float]:
    """Convert an SVG-absolute point to chip-local pixel coords."""
    ox, oy = CHIP_ORIGIN
    return ((svg_pt[0] - ox) * s, (svg_pt[1] - oy) * s)


def make_icon(size: int) -> Image.Image:
    s = size / CHIP_SIDE
    img = gradient_size(size)
    img = apply_rounded_mask(img, radius=int(round(CHIP_RX * s)))
    draw_a(img, s)
    draw_circle(img, s)
    draw_bite(img, s)
    return img


if __name__ == "__main__":
    out = Path(__file__).resolve().parent / "app-icon.png"
    img = make_icon(1024)
    img.save(out, format="PNG", optimize=True)
    print(f"wrote {out} ({out.stat().st_size} bytes)")