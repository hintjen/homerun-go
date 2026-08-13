#!/usr/bin/env python3
"""Regenerate the Android launcher and notification icons from the brand master.

    python3 scripts/generate-icons.py [path/to/master.png]

The master is `brand/app-icon.png`: a square flat #5677DA tile with the white
Homerun mark centred on it, with the padding the mark is meant to have. It is
vendored rather than pulled from the desktop repo because the two icons have
drifted apart once already — `homerun-ui/assets/icon.png` is the same mark on a
rounded tile with much less air around it.

The padding in the master is load-bearing: everything below sizes the mark by
the share of the tile it occupies, so re-cropping the master is how you change
how big the icon looks.

Needs Pillow (`python3 -m pip install Pillow`), which is why this is not part of
any build: icons change roughly never, and the generated PNGs are committed.

Two things come out of it, and they are different assets with different rules:

  mipmap-*/ic_launcher_foreground.png   the adaptive icon's foreground layer.
      108dp canvas, of which the system shows the middle 72dp under a mask it
      chooses, and only the middle 66dp is guaranteed to survive it. The mark
      is scaled to hold the same share of the *visible* area that it holds in
      the master tile, so the icon looks like the master rather than like a
      different crop of it. The adaptive background is a colour, declared in
      mipmap-anydpi-v26/ic_launcher.xml — the tile is not redrawn here.

  drawable-*/ic_notification.png        the status-bar icon.
      Drawn from its alpha channel alone and tinted flat, so it carries no
      colour and no background. 24dp with no safe zone, and the mark nearly
      fills it because it is drawn at status-bar size.

Both are alpha-only white. The mark is recovered from the master rather than
traced: every pixel of the tile sits on the line between the tile colour and
white, so the red channel (86 -> 255 for #5677DA) gives the mark's coverage
directly, antialiasing and the blue dots punched through the crossbars
included. Blue would work in principle (218 -> 255) and in practice quantises
to mush.

The tile colour is measured, not assumed — it has to keep agreeing with
`@color/launch_background`, which is what the adaptive icon uses as its
background layer, and the script says so if it stops.
"""

import os
import sys
from collections import Counter

try:
    from PIL import Image
except ImportError:  # pragma: no cover - a developer-tool failure, not a build one
    sys.exit("This needs Pillow:  python3 -m pip install Pillow")

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
RES = os.path.join(REPO, "android", "app", "src", "main", "res")
DEFAULT_MASTER = os.path.join(REPO, "brand", "app-icon.png")

# What res/values/colors.xml declares. The adaptive icon's background is that
# resource, not a copy of the master's pixels, so the two have to match.
LAUNCH_BACKGROUND = (0x56, 0x77, 0xDA)

# Every density Android asks for. The largest is the only one most phones will
# ever load, but a missing bucket makes the platform upscale a smaller one.
DENSITIES = [("mdpi", 1), ("hdpi", 1.5), ("xhdpi", 2), ("xxhdpi", 3), ("xxxhdpi", 4)]


def tile_colour(src):
    """The flat colour the mark sits on: the most common opaque pixel."""
    px = src.load()
    w, h = src.size
    seen = Counter()
    for y in range(0, h, 7):
        for x in range(0, w, 7):
            r, g, b, a = px[x, y]
            if a == 255:
                seen[(r, g, b)] += 1
    return seen.most_common(1)[0][0]


def lift_mark(master):
    """The white mark alone, cropped to its bounds, alpha = coverage."""
    src = Image.open(master).convert("RGBA")
    w, h = src.size
    px = src.load()

    tile = tile_colour(src)
    if tile != LAUNCH_BACKGROUND:
        sys.exit(
            "The master's tile is #%02X%02X%02X but @color/launch_background is "
            "#%02X%02X%02X.\nThe adaptive icon's background layer is that colour "
            "resource, so a mismatch shows as a seam\naround the mark on every "
            "launcher. Update colors.xml (and the splash, which shares it)." %
            (tile + LAUNCH_BACKGROUND)
        )
    base = float(tile[0])

    mark = Image.new("RGBA", (w, h), (255, 255, 255, 0))
    mp = mark.load()
    for y in range(h):
        for x in range(w):
            r, _, _, a = px[x, y]
            if a == 0:  # outside the tile, if the master has rounded corners
                continue
            cov = (r - base) / (255.0 - base)
            cov = 0.0 if cov < 0 else 1.0 if cov > 1 else cov
            mp[x, y] = (255, 255, 255, int(round(cov * a)))

    bounds = mark.getchannel("A").point(lambda v: 255 if v > 127 else 0).getbbox()
    return mark.crop(bounds), (bounds[3] - bounds[1]) / float(h)


def emit(mark, directory, name, canvas, height_fraction):
    """One square canvas with the mark centred at a fraction of its height."""
    mw, mh = mark.size
    out = Image.new("RGBA", (canvas, canvas), (255, 255, 255, 0))
    th = max(1, int(round(canvas * height_fraction)))
    tw = max(1, int(round(mw * th / float(mh))))
    out.alpha_composite(
        mark.resize((tw, th), Image.LANCZOS), ((canvas - tw) // 2, (canvas - th) // 2)
    )
    path = os.path.join(RES, directory, name)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    out.save(path)
    print("  " + os.path.relpath(path, REPO))


def main():
    master = sys.argv[1] if len(sys.argv) > 1 else DEFAULT_MASTER
    if not os.path.exists(master):
        sys.exit("No brand master at %s" % master)

    mark, share_of_tile = lift_mark(master)
    print("mark %dx%d, %.1f%% of the master tile" % (mark.size + (share_of_tile * 100,)))

    # Same share of the visible 72dp as it has of the master tile, expressed
    # against the 108dp canvas — the master's padding carried over rather than
    # re-invented. Well inside the 66dp safe circle at any sane master.
    foreground = share_of_tile * (72.0 / 108.0)
    print("launcher foreground (%.1f%% of the 108dp canvas):" % (foreground * 100))
    for density, scale in DENSITIES:
        emit(mark, "mipmap-" + density, "ic_launcher_foreground.png",
             int(108 * scale), foreground)

    print("notification icon:")
    for density, scale in DENSITIES:
        emit(mark, "drawable-" + density, "ic_notification.png",
             int(24 * scale), 20.0 / 24.0)


if __name__ == "__main__":
    main()
