#!/usr/bin/env python3
"""Generate the VoiceKeyboard app icons (PNG set, .icns, .ico)."""
import math
import os
import struct
import subprocess
import zlib

SIZE = 1024
SS = 3  # supersampling factor


def rounded_rect(x, y, cx, cy, w, h, r):
    """Signed distance for a rounded rect centred on (cx, cy). Returns >0 inside."""
    dx = max(abs(x - cx) - (w / 2 - r), 0.0)
    dy = max(abs(y - cy) - (h / 2 - r), 0.0)
    return math.hypot(dx, dy) < r


def mic_shape(x, y):
    if rounded_rect(x, y, 512, 460, 340, 560, 170):
        return True
    if rounded_rect(x, y, 512, 150, 520, 300, 150):
        return True
    if rounded_rect(x, y, 512, 770, 140, 160, 70):
        return True
    if rounded_rect(x, y, 512, 880, 640, 120, 60):
        return True
    return False


def coverage(x, y):
    hits = 0
    total = SS * SS
    for i in range(SS):
        for j in range(SS):
            sx = x + (i + 0.5) / SS - 0.5
            sy = y + (j + 0.5) / SS - 0.5
            if mic_shape(sx, sy):
                hits += 1
    return hits / total


def render():
    pixels = bytearray()
    for y in range(SIZE):
        for x in range(SIZE):
            cov = coverage(x, y)
            if cov <= 0:
                pixels += b"\x00\x00\x00\x00"
            else:
                # soft grey with a blue accent gradient
                a = int(255 * cov)
                base = 88 + 60 * (y / SIZE)
                pixels += bytes((int(base * 0.9), int(base * 0.95), int(min(255, base + 110)), a))
    return bytes(pixels)


def chunk(tag, data):
    c = tag + data
    return struct.pack(">I", len(data)) + c + struct.pack(">I", zlib.crc32(c) & 0xFFFFFFFF)


def write_png(path, rgba, w, h):
    raw = b"".join(b"\x00" + rgba[y * w * 4 : (y + 1) * w * 4] for y in range(h))
    png = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )
    with open(path, "wb") as f:
        f.write(png)


def write_ico(path, rgba):
    s = 256
    rows = b"".join(b"\x00" + rgba[y * s * 4 : (y + 1) * s * 4] for y in range(s))
    png = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", s, s, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(rows, 9))
        + chunk(b"IEND", b"")
    )
    header = struct.pack("<HHH", 0, 1, 1)
    entry = struct.pack("<BBBBHHII", s % 256 or 0, s % 256 or 0, 0, 0, 1, 32, len(png), 22)
    with open(path, "wb") as f:
        f.write(header + entry + png)


if __name__ == "__main__":
    out = os.path.join(os.path.dirname(__file__), "..", "src-tauri", "icons")
    os.makedirs(out, exist_ok=True)
    rgba = render()

    master = os.path.join(out, "icon-1024.png")
    write_png(master, rgba, SIZE, SIZE)

    sizes = {
        "32x32.png": 32,
        "128x128.png": 128,
        "128x128@2x.png": 256,
        "icon.png": 512,
        "Square1024x1024Logo.png": 1024,
    }
    for name, size in sizes.items():
        write_png(os.path.join(out, name), rgba, size, size)

    write_ico(os.path.join(out, "icon.ico"), rgba)

    iconset = os.path.join(out, "icon.iconset")
    os.makedirs(iconset, exist_ok=True)
    for name, size in {
        "icon_16x16.png": 16,
        "icon_32x32@2x.png": 32,
        "icon_32x32.png": 32,
        "icon_64x64.png": 64,
        "icon_128x128.png": 128,
        "icon_128x128@2x.png": 256,
        "icon_256x256.png": 256,
        "icon_256x256@2x.png": 512,
        "icon_512x512.png": 512,
        "icon_512x512@2x.png": 1024,
    }.items():
        write_png(os.path.join(iconset, name), rgba, size, size)
    subprocess.run(["iconutil", "-c", "icns", iconset, "-o", os.path.join(out, "icon.icns")], check=True)

    for f in os.listdir(iconset):
        os.unlink(os.path.join(iconset, f))
    os.rmdir(iconset)
    print("icons generated in", out)
