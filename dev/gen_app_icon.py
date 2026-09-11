#!/usr/bin/env python3
"""Generate the ACowork desktop application icon set from brand-mark.svg."""

import math
import sys
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

WORKSPACE = Path(__file__).resolve().parent.parent
ICON_DIR = WORKSPACE / "apps" / "acowork-desktop" / "src-tauri" / "icons"

GRAD_TOP = (0xF7, 0xC9, 0x48)
GRAD_BOTTOM = (0xF0, 0xA9, 0x2E)
LETTER_FILL = (0xFF, 0xFF, 0xFF, 0xFF)

MASTER_SIZE = 1024
CORNER_RADIUS_RATIO = 192 / 864
INSET = 80


def rounded_rect_mask(size, radius):
    mask = Image.new("L", (size, size), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        (0, 0, size - 1, size - 1), radius=radius, fill=255
    )
    return mask


def vertical_gradient(size, top, bottom):
    grad = Image.new("RGB", (size, size), top)
    px = grad.load()
    for y in range(size):
        t = y / (size - 1)
        row = tuple(int(top[c] * (1 - t) + bottom[c] * t) for c in range(3))
        for x in range(size):
            px[x, y] = row
    return grad


def draw_letter_a(size, draw):
    """Draw the letter 'A' centered on the yellow chip.

    Faithful to `assets/brand-mark.svg`: the SVG uses `font-size="76"`
    on an 84×84 chip → em-size is 76/84 ≈ 90.5 % of the chip side.
    Cap-height of Arial Bold (≈ 0.715 × em) then occupies ~65 % of
    the chip, matching the SVG's visual weight. We pass the same
    em-size ratio and rely on PIL's `anchor="mm"` to center the glyph
    by its own font metrics — no manual bbox juggling needed.
    """
    chip_side = size - 2 * INSET
    target_em = int(chip_side * 76 / 84)  # ≈ 782 on a 864 chip
    cx = size // 2
    cy = size // 2

    font_candidates = [
        ("arialbd.ttf", "Arial Bold (Windows)"),
        ("Arial Bold.ttf", "Arial Bold"),
        ("/Library/Fonts/Arial Bold.ttf", "macOS Arial Bold"),
        ("/System/Library/Fonts/Supplemental/Arial Bold.ttf", "macOS Arial Bold"),
    ]
    font = None
    for path, _ in font_candidates:
        try:
            font = ImageFont.truetype(path, size=target_em)
            break
        except OSError:
            continue
    if font is None:
        # Last-resort fallback so the script never crashes on a
        # font-less CI box — the icon still gets a chip + default
        # glyph.  Re-running with a font installed is recommended.
        font = ImageFont.load_default()

    draw.text((cx, cy), "A", font=font, fill=LETTER_FILL, anchor="mm")


def make_master():
    size = MASTER_SIZE
    grad = vertical_gradient(size, GRAD_TOP, GRAD_BOTTOM)
    radius = int((size - 2 * INSET) * CORNER_RADIUS_RATIO)
    chip = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    chip.paste(grad, (0, 0))
    chip.putalpha(rounded_rect_mask(size, radius))

    draw = ImageDraw.Draw(chip)
    draw_letter_a(size, draw)
    return chip


def write_png(img, name):
    out = ICON_DIR / name
    img.save(out, format="PNG", optimize=True)
    return out


def write_ico(master):
    sizes = [(16, 16), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)]
    frames = [master.resize(s, Image.Resampling.LANCZOS) for s in sizes]
    out = ICON_DIR / "icon.ico"
    frames[0].save(out, format="ICO", sizes=sizes, append_images=frames[1:])
    return out


def write_icns(master):
    sizes = [(16, 16), (32, 32), (64, 64), (128, 128), (256, 256), (512, 512), (1024, 1024)]
    frames = [master.resize(s, Image.Resampling.LANCZOS) for s in sizes]
    out = ICON_DIR / "icon.icns"
    frames[0].save(out, format="ICNS", append_images=frames[1:])
    return out


SQUARE_SIZES = {
    "StoreLogo.png": 50,
    "Square30x30Logo.png": 30,
    "Square44x44Logo.png": 44,
    "Square71x71Logo.png": 71,
    "Square89x89Logo.png": 89,
    "Square107x107Logo.png": 107,
    "Square142x142Logo.png": 142,
    "Square150x150Logo.png": 150,
    "Square284x284Logo.png": 284,
    "Square310x310Logo.png": 310,
}


def main():
    ICON_DIR.mkdir(parents=True, exist_ok=True)
    print(f"Output dir: {ICON_DIR}")

    master = make_master()
    write_png(master, "icon.png")

    write_png(master.resize((32, 32), Image.Resampling.LANCZOS), "32x32.png")
    write_png(master.resize((128, 128), Image.Resampling.LANCZOS), "128x128.png")
    write_png(master.resize((256, 256), Image.Resampling.LANCZOS), "128x128@2x.png")

    for name, px in SQUARE_SIZES.items():
        write_png(master.resize((px, px), Image.Resampling.LANCZOS), name)

    write_ico(master)
    write_icns(master)

    print("OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())