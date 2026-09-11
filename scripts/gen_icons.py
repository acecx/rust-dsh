#!/usr/bin/env python3
"""Generate DSH Client icons with the Python standard library only.

Outputs:
  assets/tray-template.rgba  44x44 raw RGBA (macOS menu-bar template)
  assets/icon.iconset/*.png  app icon set
  assets/icon.icns           app icon (via iconutil)
"""
import os
import struct
import subprocess
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
ASSETS = os.path.join(HERE, "..", "assets")

# --------------------------------------------------------------------------
# Geometry (normalized to a 1024 grid)
# --------------------------------------------------------------------------
R = 224  # corner radius of the app icon rounded square
# Chevron ">": A -> B -> C
CHEV_A = (212, 317)
CHEV_B = (512, 499)
CHEV_C = (212, 681)
STROKE = 84  # glyph stroke width (at 1024)
# Underscore "_": rounded bar
BAR = (592, 607, 812, 707)  # x0, y0, x1, y1
BAR_R = 30

TRAY_GRID = 1024  # reuse the same normalized geometry for the tray glyph


def png_chunk(tag, data):
    return (
        struct.pack(">I", len(data))
        + tag
        + data
        + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
    )


def write_png(path, size, pixels):
    w = h = size
    raw = b"".join(
        b"\x00" + b"".join(struct.pack("4B", *px) for px in row) for row in pixels
    )
    data = (
        b"\x89PNG\r\n\x1a\n"
        + png_chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
        + png_chunk(b"IDAT", zlib.compress(raw, 9))
        + png_chunk(b"IEND", b"")
    )
    with open(path, "wb") as f:
        f.write(data)


def seg_dist(px, py, ax, ay, bx, by):
    vx, vy = bx - ax, by - ay
    wx, wy = px - ax, py - ay
    t = max(0.0, min(1.0, (wx * vx + wy * vy) / (vx * vx + vy * vy)))
    dx, dy = px - (ax + t * vx), py - (ay + t * vy)
    return (dx * dx + dy * dy) ** 0.5


def rounded_rect_inside(x, y, half, r):
    dx = max(abs(x) - (half - r), 0.0)
    dy = max(abs(y) - (half - r), 0.0)
    return dx * dx + dy * dy <= r * r


def glyph_alpha(x, y, s):
    """Coverage of the '>_' glyph at normalized coords scaled by s."""
    hw = STROKE / 2 * s
    d = min(
        seg_dist(x, y, CHEV_A[0] * s, CHEV_A[1] * s, CHEV_B[0] * s, CHEV_B[1] * s),
        seg_dist(x, y, CHEV_C[0] * s, CHEV_C[1] * s, CHEV_B[0] * s, CHEV_B[1] * s),
    )
    alpha = max(0.0, min(1.0, hw - d + 0.5))  # chevron coverage
    # underscore bar (rounded rect)
    bx0, by0, bx1, by1 = BAR[0] * s, BAR[1] * s, BAR[2] * s, BAR[3] * s
    bhw, bhh = (bx1 - bx0) / 2, (by1 - by0) / 2
    bcx, bcy = (bx0 + bx1) / 2, (by0 + by1) / 2
    if rounded_rect_inside(x - bcx, y - bcy, min(bhw, bhh), BAR_R * s):
        alpha = 1.0
    else:
        # soft edge of the bar
        dx = max(abs(x - bcx) - (bhw - BAR_R * s), 0.0)
        dy = max(abs(y - bcy) - (bhh - BAR_R * s), 0.0)
        dist = (dx * dx + dy * dy) ** 0.5
        alpha = max(alpha, max(0.0, min(1.0, (BAR_R * s) - dist + 0.5)))
    return alpha


def lerp(a, b, t):
    return a + (b - a) * t


def render(size, background=True, glyph=(255, 255, 255), margin=0):
    """Render the icon at `size`x`size`. Returns rows of (r,g,b,a).

    `margin` (in 1024 units) insets the rounded square — used for the
    menu-bar template so the glyph does not touch the icon edges.
    """
    s = size / 1024.0
    half = (512 - margin) * s
    radius = R * s
    # gradient endpoints
    top = (93, 107, 247)    # #5D6BF7
    bottom = (36, 56, 168)  # #2438A8
    rows = []
    for py in range(size):
        row = []
        for px in range(size):
            # 4x4 supersampled coverage
            cov = 0
            gc = 0.0
            for sy in (0.25, 0.75):
                for sx in (0.25, 0.75):
                    x = px + sx - half
                    y = py + sy - half
                    if rounded_rect_inside(x, y, half, radius):
                        cov += 1
                    if background:
                        gc += glyph_alpha(px + sx, py + sy, s)
            cov /= 4
            gc /= 4
            if not background:
                # template icon: pure alpha shape
                row.append((0, 0, 0, round(cov * 255)))
                continue
            t = (py + 0.5) / size  # vertical gradient
            r = lerp(top[0], bottom[0], t)
            g = lerp(top[1], bottom[1], t)
            b = lerp(top[2], bottom[2], t)
            r = round(lerp(r, glyph[0], gc))
            g = round(lerp(g, glyph[1], gc))
            b = round(lerp(b, glyph[2], gc))
            row.append((r, g, b, round(cov * 255)))
        rows.append(row)
    return rows


def main():
    os.makedirs(ASSETS, exist_ok=True)

    # Tray template: 44x44 raw RGBA, shape only, with an inset margin.
    px = render(44, background=False, margin=100)
    with open(os.path.join(ASSETS, "tray-template.rgba"), "wb") as f:
        for row in px:
            for (r, g, b, a) in row:
                f.write(struct.pack("4B", r, g, b, a))
    # Preview PNG of the same shape (not used at runtime, handy for review).
    write_png(os.path.join(ASSETS, "tray-template.png"), 44, px)
    print("tray-template.rgba written")

    # App icon set.
    iconset = os.path.join(ASSETS, "icon.iconset")
    os.makedirs(iconset, exist_ok=True)
    targets = {
        "icon_16x16.png": 16,
        "icon_16x16@2x.png": 32,
        "icon_32x32.png": 32,
        "icon_32x32@2x.png": 64,
        "icon_128x128.png": 128,
        "icon_128x128@2x.png": 256,
        "icon_256x256.png": 256,
        "icon_256x256@2x.png": 512,
        "icon_512x512.png": 512,
        "icon_512x512@2x.png": 1024,
    }
    for name, size in targets.items():
        write_png(os.path.join(iconset, name), size, render(size))
    # iconutil wants sips-normalized PNGs; normalize in place.
    for name in targets:
        path = os.path.join(iconset, name)
        tmp = path + ".norm.png"
        subprocess.run(
            ["sips", "-s", "format", "png", path, "--out", tmp],
            check=True,
            capture_output=True,
        )
        os.replace(tmp, path)
    print("iconset written")

    # iconutil fails on some non-boot volumes (/Volumes/...); stage the
    # iconset under a temp dir, then copy the finished .icns back.
    import shutil
    import tempfile

    staging = os.path.join(tempfile.mkdtemp(prefix="dsh-icons-"), "icon.iconset")
    shutil.copytree(iconset, staging)
    icns_tmp = os.path.join(os.path.dirname(staging), "icon.icns")
    subprocess.run(
        ["iconutil", "-c", "icns", staging, "-o", icns_tmp],
        check=True,
    )
    shutil.copyfile(icns_tmp, os.path.join(ASSETS, "icon.icns"))
    shutil.rmtree(os.path.dirname(staging))
    print("icon.icns written")


if __name__ == "__main__":
    main()
