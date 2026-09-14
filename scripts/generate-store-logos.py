#!/usr/bin/env python3
"""Generate the Windows Store logos that `tauri icon` does not produce.

These are the Square*NxN*Logo.png + StoreLogo.png files referenced by
Tauri's bundle config for Microsoft Store targets.
"""
import importlib.util
from pathlib import Path
from PIL import Image

# Make the icon generator importable (filename has hyphens, so plain import fails).
_spec = importlib.util.spec_from_file_location(
    "_make_icon",
    Path(__file__).resolve().parent / "generate-app-icon.py",
)
assert _spec and _spec.loader, "failed to locate generate-app-icon.py"
_module = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_module)
make_icon = _module.make_icon

ICONS_DIR = (
    Path(__file__).resolve().parent.parent
    / "apps"
    / "acowork-desktop"
    / "src-tauri"
    / "icons"
)

# filename -> pixel size (Microsoft Store logo requirements)
STORE_LOGOS = {
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


def main() -> None:
    # Render once at 1024 (highest-res target) then downscale. Downscale from
    # a large source gives better antialiasing than rendering at every size.
    master = make_icon(1024)
    for name, size in STORE_LOGOS.items():
        out = ICONS_DIR / name
        img = master.resize((size, size), Image.LANCZOS)
        img.save(out, format="PNG", optimize=True)
        print(f"wrote {out.name} ({size}x{size}, {out.stat().st_size} bytes)")


if __name__ == "__main__":
    main()