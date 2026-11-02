#!/usr/bin/env python3
"""Generates the plugin's PNG icons (stdlib only): `python3 scripts/gen-icons.py`.

Original artwork: a 2x2 grid of keys (one lit amber, like a switched-on light) for the plugin and
the button action, and a dial for the dial action. Action list icons are white on transparent,
as Stream Deck expects; key images are on the dark tile colour of the server's default buttons.
"""
import math
import os
import struct
import zlib

OUT = os.path.join(os.path.dirname(__file__), "..", "ca.bcaldwell.sdc.sdPlugin", "imgs")
SS = 4  # supersampling per axis
AMBER = (0xF5, 0xB4, 0x00)
TILE = (0x1C, 0x1C, 0x1E)
WHITE = (255, 255, 255)


def rrect(x, y, x0, y0, x1, y1, r):
    cx = min(max(x, x0 + r), x1 - r)
    cy = min(max(y, y0 + r), y1 - r)
    return x0 <= x <= x1 and y0 <= y <= y1 and (x - cx) ** 2 + (y - cy) ** 2 <= r * r


def ring(x, y, cx, cy, r0, r1):
    return r0 * r0 <= (x - cx) ** 2 + (y - cy) ** 2 <= r1 * r1


def segment(x, y, ax, ay, bx, by, w):
    dx, dy = bx - ax, by - ay
    t = max(0, min(1, ((x - ax) * dx + (y - ay) * dy) / (dx * dx + dy * dy)))
    return (x - ax - t * dx) ** 2 + (y - ay - t * dy) ** 2 <= (w / 2) ** 2


def grid_glyph(lit, fg):
    # unit coordinates 0..1
    def f(x, y):
        for i, (x0, y0) in enumerate([(0.12, 0.12), (0.54, 0.12), (0.12, 0.54), (0.54, 0.54)]):
            if rrect(x, y, x0, y0, x0 + 0.34, y0 + 0.34, 0.07):
                return lit if i == 3 else fg
        return None

    return f


def dial_glyph(lit, fg):
    def f(x, y):
        if segment(x, y, 0.5, 0.5, 0.5 + 0.22 * math.cos(-0.9), 0.5 + 0.22 * math.sin(-0.9), 0.1):
            return lit
        if ring(x, y, 0.5, 0.5, 0.34, 0.44):
            return fg
        return None

    return f


def render(size, glyph, bg, pad=0.0, radius=0.0):
    rows = []
    n = SS * SS
    for py in range(size):
        row = bytearray()
        for px in range(size):
            acc = [0.0, 0.0, 0.0, 0.0]
            for sy in range(SS):
                for sx in range(SS):
                    x = (px + (sx + 0.5) / SS) / size
                    y = (py + (sy + 0.5) / SS) / size
                    c = None
                    if bg is not None and (radius == 0 or rrect(x, y, 0, 0, 1, 1, radius)):
                        c = bg
                    if pad < x < 1 - pad and pad < y < 1 - pad:
                        g = glyph((x - pad) / (1 - 2 * pad), (y - pad) / (1 - 2 * pad))
                        if g is not None:
                            c = g
                    if c is not None:
                        acc[0] += c[0]
                        acc[1] += c[1]
                        acc[2] += c[2]
                        acc[3] += 255
            a = acc[3] / n
            if a == 0:
                row += bytes(4)
            else:
                k = 255 / acc[3]
                row += bytes([round(acc[0] * k), round(acc[1] * k), round(acc[2] * k), round(a)])
        rows.append(bytes(row))
    return rows


def write_png(path, rows):
    h, w = len(rows), len(rows[0]) // 4

    def chunk(t, d):
        c = struct.pack(">I", len(d)) + t + d
        return c + struct.pack(">I", zlib.crc32(t + d) & 0xFFFFFFFF)

    raw = b"".join(b"\x00" + r for r in rows)
    data = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
    data += chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b"")
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as f:
        f.write(data)


def both(name, size, glyph, bg=None, pad=0.0, radius=0.0):
    for suffix, s in (("", size), ("@2x", size * 2)):
        write_png(os.path.join(OUT, name + suffix + ".png"), render(s, glyph, bg, pad, radius))


# plugin: marketplace 288, category icon 28 (white, transparent)
both("plugin/marketplace", 288, grid_glyph(AMBER, WHITE), TILE, pad=0.12, radius=0.2)
both("plugin/category-icon", 28, grid_glyph(WHITE, WHITE))
# actions: list icon 20 (white), key image 72 (dark tile)
both("actions/button/icon", 20, grid_glyph(WHITE, WHITE))
both("actions/button/key", 72, grid_glyph(AMBER, WHITE), TILE, pad=0.14)
both("actions/dial/icon", 20, dial_glyph(WHITE, WHITE))
both("actions/dial/encoder", 20, dial_glyph(WHITE, WHITE))
both("actions/dial/key", 72, dial_glyph(AMBER, WHITE), TILE, pad=0.14)
