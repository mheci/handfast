#!/usr/bin/env python3
"""Render the Handfast mark to PNG, ICO and raw RGBA from one geometry.

Run from the repository root: python scripts/branding.py
"""

import math
import struct
import zlib
from pathlib import Path

OUT = Path("assets/branding")
VIEW = 512.0
CX = CY = 256.0
R_OUT = 116.0
R_IN = 66.0
RING_DX = 170.0
ANGLE = math.radians(-18.0)

TEAL = (18, 140, 126)
COPPER = (232, 131, 58)
CLEAR = (0, 0, 0)

SIZES = (16, 24, 32, 48, 64, 128, 256, 512, 1024)
ICO_SIZES = (16, 24, 32, 48, 64, 128, 256)


def ring_centers():
    half = RING_DX / 2.0
    return (CX - half, CY), (CX + half, CY)


def sample(x, y):
    dx = x - CX
    dy = y - CY
    cos_a = math.cos(-ANGLE)
    sin_a = math.sin(-ANGLE)
    ux = dx * cos_a - dy * sin_a + CX
    uy = dx * sin_a + dy * cos_a + CY
    a, b = ring_centers()
    in_a = R_IN <= math.hypot(ux - a[0], uy - a[1]) <= R_OUT
    in_b = R_IN <= math.hypot(ux - b[0], uy - b[1]) <= R_OUT
    if in_a:
        return TEAL
    if in_b:
        return COPPER
    return CLEAR


def render(size, ss=4):
    pixels = bytearray(size * size * 4)
    scale = VIEW / size
    samples = ss * ss
    for py in range(size):
        row = py * size * 4
        for px in range(size):
            r = g = b = a = 0
            for sy in range(ss):
                y = (py + (sy + 0.5) / ss) * scale
                for sx in range(ss):
                    x = (px + (sx + 0.5) / ss) * scale
                    sr, sg, sb = sample(x, y)
                    if sr or sg or sb:
                        r += sr
                        g += sg
                        b += sb
                        a += 255
            i = row + px * 4
            if a:
                pixels[i] = r * 255 // a
                pixels[i + 1] = g * 255 // a
                pixels[i + 2] = b * 255 // a
                pixels[i + 3] = a // samples
    return bytes(pixels)


def png(size, rgba):
    raw = bytearray()
    stride = size * 4
    for y in range(size):
        raw.append(0)
        raw += rgba[y * stride : (y + 1) * stride]

    def chunk(tag, data):
        body = tag + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    header = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(bytes(raw), 9))
        + chunk(b"IEND", b"")
    )


def dib(size, rgba):
    header = struct.pack("<IiiHHIIiiII", 40, size, size * 2, 1, 32, 0, 0, 0, 0, 0, 0)
    xor = bytearray()
    for y in range(size - 1, -1, -1):
        row = y * size * 4
        for x in range(size):
            i = row + x * 4
            xor += bytes((rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]))
    mask_stride = ((size + 31) // 32) * 4
    return header + bytes(xor) + bytes(mask_stride * size)


def ico(images):
    entries = b""
    payload = b""
    offset = 6 + 16 * len(images)
    for size, data in images:
        entries += struct.pack("<BBBBHHII", size % 256, size % 256, 0, 0, 1, 32, len(data), offset)
        payload += data
        offset += len(data)
    return struct.pack("<HHH", 0, 1, len(images)) + entries + payload


def rings(stroke):
    a, b = ring_centers()
    mid = (R_OUT + R_IN) / 2.0
    return a, b, mid, stroke


def svg():
    a, b, mid, stroke = rings(R_OUT - R_IN)
    return f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 512" role="img" aria-labelledby="title">
  <title id="title">Handfast</title>
  <g transform="rotate(-18 256 256)" fill="none" stroke-width="{stroke:g}">
    <circle cx="{b[0]:g}" cy="{b[1]:g}" r="{mid:g}" stroke="#e8833a"/>
    <circle cx="{a[0]:g}" cy="{a[1]:g}" r="{mid:g}" stroke="#128c7e"/>
  </g>
</svg>
"""


def svg_mono():
    a, b, mid, stroke = rings(R_OUT - R_IN)
    return f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 512" role="img" aria-labelledby="title">
  <title id="title">Handfast</title>
  <g transform="rotate(-18 256 256)" fill="none" stroke="currentColor" stroke-width="{stroke:g}">
    <circle cx="{a[0]:g}" cy="{a[1]:g}" r="{mid:g}"/>
    <circle cx="{b[0]:g}" cy="{b[1]:g}" r="{mid:g}"/>
  </g>
</svg>
"""


def wordmark():
    a, b, mid, stroke = rings(R_OUT - R_IN)
    return f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 720 160" role="img" aria-labelledby="title">
  <title id="title">Handfast</title>
  <g transform="translate(8 8) scale(0.28)" fill="none" stroke-width="{stroke:g}">
    <g transform="rotate(-18 256 256)">
      <circle cx="{b[0]:g}" cy="{b[1]:g}" r="{mid:g}" stroke="#e8833a"/>
      <circle cx="{a[0]:g}" cy="{a[1]:g}" r="{mid:g}" stroke="#128c7e"/>
    </g>
  </g>
  <text x="176" y="107" font-family="Inter, 'Segoe UI', system-ui, sans-serif" font-size="76" font-weight="650" letter-spacing="-2" fill="#0b1220">Hand<tspan fill="#128c7e">fast</tspan></text>
</svg>
"""


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    rendered = {}
    for size in SIZES:
        rgba = render(size)
        rendered[size] = rgba
        (OUT / f"handfast-{size}.png").write_bytes(png(size, rgba))
        print(f"handfast-{size}.png")
    (OUT / "handfast.ico").write_bytes(ico([(s, dib(s, rendered[s])) for s in ICO_SIZES]))
    print("handfast.ico")
    (OUT / "handfast-128.rgba").write_bytes(rendered[128])
    print("handfast-128.rgba")
    (OUT / "handfast-mark.svg").write_text(svg(), encoding="utf-8")
    (OUT / "handfast-mark-mono.svg").write_text(svg_mono(), encoding="utf-8")
    (OUT / "handfast-wordmark.svg").write_text(wordmark(), encoding="utf-8")
    print("handfast-mark.svg, handfast-mark-mono.svg, handfast-wordmark.svg")


if __name__ == "__main__":
    main()
