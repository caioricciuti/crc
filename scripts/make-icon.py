#!/usr/bin/env python3
"""Generates the app icon as a .icns, with no image-library dependency.

PNG is straightforward to emit by hand: a fixed signature, an IHDR, one
zlib-compressed IDAT of filtered scanlines, and an IEND. zlib is in the
standard library, so this needs nothing installed.

The mark is the editor's own palette: the background colour from the default
theme, rounded, with the caret bar in the cursor blue.
"""

import struct
import subprocess
import sys
import zlib
from pathlib import Path

# Straight from render/layout.rs Theme::default(): the graphite background,
# the mint accent for the caret, and the keyword and function hues for the
# two bars, so the mark is a line of code in the editor's own colours.
BG = (25, 28, 29)
CARET = (166, 221, 194)
ROW_COLORS = [(199, 146, 234), (130, 170, 255)]


def png(width: int, height: int, pixels: bytes) -> bytes:
    """Encodes RGBA8 pixel data as a PNG."""

    def chunk(tag: bytes, data: bytes) -> bytes:
        body = tag + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))

    # Each scanline is prefixed with filter type 0 (None).
    raw = b"".join(
        b"\x00" + pixels[y * width * 4 : (y + 1) * width * 4] for y in range(height)
    )
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def render(size: int) -> bytes:
    """Draws the icon at `size` square, with 4x supersampled edges."""
    px = bytearray(size * size * 4)

    # macOS icons sit in a rounded square inset from the canvas.
    margin = size * 0.10
    radius = size * 0.22
    left, top = margin, margin
    right, bottom = size - margin, size - margin

    # Caret bar, proportioned like the one the editor actually draws.
    bar_w = max(1.0, size * 0.055)
    bar_x = size * 0.30
    bar_top = size * 0.30
    bar_bottom = size * 0.70

    # Two text bars beside the caret, suggesting a line of code.
    rows = [
        (size * 0.40, size * 0.415, size * 0.70, 0.95),
        (size * 0.52, size * 0.415, size * 0.62, 0.95),
    ]

    def rounded_alpha(x: float, y: float) -> float:
        """Coverage of the rounded square at a point."""
        cx = min(max(x, left + radius), right - radius)
        cy = min(max(y, top + radius), bottom - radius)
        dx, dy = x - cx, y - cy
        if dx == 0 and dy == 0:
            return 1.0 if (left <= x <= right and top <= y <= bottom) else 0.0
        dist = (dx * dx + dy * dy) ** 0.5
        return 1.0 if dist <= radius else 0.0

    samples = [(0.25, 0.25), (0.75, 0.25), (0.25, 0.75), (0.75, 0.75)]

    for y in range(size):
        for x in range(size):
            bg_cov = 0.0
            caret_cov = 0.0
            text_cov = [0.0, 0.0]

            for sx, sy in samples:
                fx, fy = x + sx, y + sy
                a = rounded_alpha(fx, fy)
                if a == 0.0:
                    continue
                bg_cov += a
                if bar_x <= fx <= bar_x + bar_w and bar_top <= fy <= bar_bottom:
                    caret_cov += 1.0
                for i, (ry, rx0, rx1, _) in enumerate(rows):
                    rh = max(1.0, size * 0.045)
                    if rx0 <= fx <= rx1 and ry <= fy <= ry + rh:
                        text_cov[i] += 1.0

            n = len(samples)
            bg_a = bg_cov / n
            if bg_a == 0.0:
                continue

            r, g, b = BG
            for i, (_, _, _, strength) in enumerate(rows):
                c = text_cov[i] / n
                if c > 0:
                    tr, tg, tb = ROW_COLORS[i]
                    r = r * (1 - c * strength) + tr * c * strength
                    g = g * (1 - c * strength) + tg * c * strength
                    b = b * (1 - c * strength) + tb * c * strength
            c = caret_cov / n
            if c > 0:
                r = r * (1 - c) + CARET[0] * c
                g = g * (1 - c) + CARET[1] * c
                b = b * (1 - c) + CARET[2] * c

            o = (y * size + x) * 4
            px[o] = int(r)
            px[o + 1] = int(g)
            px[o + 2] = int(b)
            px[o + 3] = int(bg_a * 255)

    return png(size, size, bytes(px))


def main() -> int:
    out = Path(sys.argv[1] if len(sys.argv) > 1 else "crc.icns")
    iconset = out.with_suffix(".iconset")
    iconset.mkdir(parents=True, exist_ok=True)

    # The exact names iconutil expects.
    for size, names in {
        16: ["icon_16x16.png"],
        32: ["icon_16x16@2x.png", "icon_32x32.png"],
        64: ["icon_32x32@2x.png"],
        128: ["icon_128x128.png"],
        256: ["icon_128x128@2x.png", "icon_256x256.png"],
        512: ["icon_256x256@2x.png", "icon_512x512.png"],
        1024: ["icon_512x512@2x.png"],
    }.items():
        data = render(size)
        for name in names:
            (iconset / name).write_bytes(data)

    subprocess.run(
        ["iconutil", "-c", "icns", str(iconset), "-o", str(out)],
        check=True,
    )
    subprocess.run(["rm", "-rf", str(iconset)], check=True)
    print(f"wrote {out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
