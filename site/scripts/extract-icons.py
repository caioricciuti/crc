#!/usr/bin/env python3
"""Writes src/data/extension-icons.json: each icon an extension may name,
as an SVG path read from the glyph crc draws (the bundled Symbols Nerd
Font), so the site's extension cards show the same icons as the app.

Run from site/ when the icon list changes: python3 scripts/extract-icons.py
The list is EXTENSION_ICONS in ../src/project/icons.rs. Standard library.
"""

import json
import pathlib
import re
import struct

HERE = pathlib.Path(__file__).resolve().parent
REPO = HERE.parent.parent
FONT = REPO / "third_party/nerd-fonts-symbols/SymbolsNerdFont-Regular.ttf"
ICONS = re.findall(r'\("([a-z-]+)", \'\\u\{([0-9a-f]+)\}\'\)', (REPO / "src/project/icons.rs").read_text())

d = FONT.read_bytes()
tables = {}
for i in range(struct.unpack(">H", d[4:6])[0]):
    tag, _, off, length = struct.unpack(">4sIII", d[12 + 16 * i : 28 + 16 * i])
    tables[tag] = off

head = tables[b"head"]
units = struct.unpack(">H", d[head + 18 : head + 20])[0]
long_loca = struct.unpack(">h", d[head + 50 : head + 52])[0] == 1

cmap = tables[b"cmap"]
glyph_of = {}
for i in range(struct.unpack(">H", d[cmap + 2 : cmap + 4])[0]):
    _, _, off = struct.unpack(">HHI", d[cmap + 4 + 8 * i : cmap + 12 + 8 * i])
    o = cmap + off
    if struct.unpack(">H", d[o : o + 2])[0] == 12:
        for g in range(struct.unpack(">I", d[o + 12 : o + 16])[0]):
            s, e, start = struct.unpack(">III", d[o + 16 + 12 * g : o + 28 + 12 * g])
            for cp in range(s, e + 1):
                glyph_of.setdefault(cp, start + cp - s)


def loca(gid):
    base = tables[b"loca"]
    if long_loca:
        return struct.unpack(">I", d[base + 4 * gid : base + 4 * gid + 4])[0]
    return 2 * struct.unpack(">H", d[base + 2 * gid : base + 2 * gid + 2])[0]


def path(gid):
    start, end = loca(gid), loca(gid + 1)
    if start == end:
        return ""
    g = tables[b"glyf"] + start
    contours = struct.unpack(">h", d[g : g + 2])[0]
    if contours < 0:
        raise ValueError("composite glyph")
    ends = struct.unpack(f">{contours}H", d[g + 10 : g + 10 + 2 * contours])
    n = ends[-1] + 1
    p = g + 10 + 2 * contours
    p += 2 + struct.unpack(">H", d[p : p + 2])[0]
    flags = []
    while len(flags) < n:
        f = d[p]
        p += 1
        flags.append(f)
        if f & 8:
            flags.extend([f] * d[p])
            p += 1
    coords = []
    for short, same in ((2, 16), (4, 32)):
        value, out = 0, []
        for f in flags:
            if f & short:
                delta = d[p]
                p += 1
                value += delta if f & same else -delta
            elif not f & same:
                value += struct.unpack(">h", d[p : p + 2])[0]
                p += 2
            out.append(value)
        coords.append(out)
    xs, ys = coords
    pts = [(xs[i], units - ys[i], flags[i] & 1) for i in range(n)]
    out = []
    first = 0
    for last in ends:
        c = pts[first : last + 1]
        first = last + 1
        if not c:
            continue
        # Start on an on-curve point, inventing one between two off points.
        k = next((i for i, q in enumerate(c) if q[2]), None)
        if k is None:
            c = [((c[0][0] + c[1][0]) / 2, (c[0][1] + c[1][1]) / 2, 1)] + c
            k = 0
        c = c[k:] + c[:k]
        out.append(f"M{c[0][0]:g} {c[0][1]:g}")
        i = 1
        while i <= len(c):
            q = c[i % len(c)]
            if q[2]:
                out.append(f"L{q[0]:g} {q[1]:g}")
                i += 1
            else:
                nxt = c[(i + 1) % len(c)]
                end = nxt if nxt[2] else ((q[0] + nxt[0]) / 2, (q[1] + nxt[1]) / 2, 1)
                out.append(f"Q{q[0]:g} {q[1]:g} {end[0]:g} {end[1]:g}")
                i += 2 if nxt[2] else 1
        out.append("Z")
    return "".join(out)


def view_box(gid):
    """The glyph's own box, flipped like the path, padded to a square."""
    g = tables[b"glyf"] + loca(gid)
    x0, y0, x1, y1 = struct.unpack(">4h", d[g + 2 : g + 10])
    top, bottom = units - y1, units - y0
    w, h = x1 - x0, bottom - top
    side = max(w, h)
    return f"{x0 - (side - w) / 2:g} {top - (side - h) / 2:g} {side} {side}"


icons = {}
for name, cp in ICONS:
    gid = glyph_of[int(cp, 16)]
    icons[name] = {"viewBox": view_box(gid), "d": path(gid)}
out = {"icons": icons}
(HERE.parent / "src/data/extension-icons.json").write_text(json.dumps(out, indent=1) + "\n")
print(f"{len(icons)} icons, {units} units per em")
