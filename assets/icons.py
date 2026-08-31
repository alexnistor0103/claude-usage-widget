"""Regenerates apps/overlay/src-tauri/icons/ from the mark in logo.svg.

Run with Pillow installed: python assets/icons.py. The SVG is the readable
source; this draws the same geometry, because the bundlers want raster.

The mark is what the widget shows: three usage bars filling toward the same
limit, each one a fill inside a full-width track. Drawn at 8x and downsampled,
so the capsule ends stay clean at 16 px, where this still has to read.
"""

import io
import os
import struct
import sys

from PIL import Image, ImageDraw, ImageFilter

OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "apps", "overlay", "src-tauri", "icons")
SS = 8

# Warm terminal ink. The longest bar runs orange because it is the one closest
# to its cap; the shorter two stay amber.
BG_TOP, BG_BOT = (32, 26, 22), (15, 12, 10)
GLOW = (245, 158, 11)
TRACK = (56, 46, 40)
AMBER = ((252, 211, 77), (245, 158, 11))
ORANGE = ((251, 146, 60), (234, 88, 12))
BARS = ((0.88, ORANGE), (0.58, AMBER), (0.31, AMBER))


def _lgrad(size, a, b):
    """Left-to-right gradient; the bars are horizontal, so no rotation needed."""
    w, h = size
    strip = Image.new("RGB", (w, 1))
    px = strip.load()
    for x in range(w):
        t = x / max(w - 1, 1)
        px[x, 0] = tuple(round(p + (q - p) * t) for p, q in zip(a, b))
    return strip.resize((w, h), Image.BILINEAR)


def _tile(n, corner_frac):
    img = Image.new("RGBA", (n, n))
    strip = Image.new("RGB", (1, n))
    px = strip.load()
    for y in range(n):
        f = y / max(n - 1, 1)
        px[0, y] = tuple(round(a + (b - a) * f) for a, b in zip(BG_TOP, BG_BOT))
    img.paste(strip.resize((n, n), Image.BILINEAR).convert("RGBA"), (0, 0))

    # A soft lift behind the bars, so the tile is not a flat rectangle.
    r = int(n * 0.46)
    glow = Image.new("L", (n, n), 0)
    ImageDraw.Draw(glow).ellipse([n // 2 - r, n // 2 - r, n // 2 + r, n // 2 + r], fill=52)
    img.paste(Image.new("RGB", (n, n), GLOW), (0, 0), glow.filter(ImageFilter.GaussianBlur(r * 0.5)))

    mask = Image.new("L", (n, n), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        (0, 0, n - 1, n - 1), radius=int(n * corner_frac), fill=255
    )
    out = Image.new("RGBA", (n, n), (0, 0, 0, 0))
    out.paste(img, (0, 0), mask)
    ImageDraw.Draw(out).rounded_rectangle(
        (0, 0, n - 1, n - 1), radius=int(n * corner_frac),
        outline=(255, 255, 255, 24), width=max(2, n // 260),
    )
    return out


def _rows(t):
    """Bar geometry as (x0, y0, x1, y1) tracks, centred in a tile of side t."""
    m = t * 0.170
    h = t * 0.134
    gap = t * 0.104
    y = (t - (len(BARS) * h + (len(BARS) - 1) * gap)) / 2
    for _ in BARS:
        yield m, y, t - m, y + h
        y += h + gap


def mark(side, inset_frac=0.0, corner_frac=0.185):
    """The tile. `inset_frac` leaves the transparent margin macOS expects."""
    n = side * SS
    pad = round(n * inset_frac)
    t = n - 2 * pad
    img = _tile(t, corner_frac)
    d = ImageDraw.Draw(img)

    for (x0, y0, x1, y1), (frac, cols) in zip(_rows(t), BARS):
        h = y1 - y0
        d.rounded_rectangle((x0, y0, x1, y1), radius=h / 2, fill=TRACK + (255,))
        mask = Image.new("L", (t, t), 0)
        ImageDraw.Draw(mask).rounded_rectangle(
            (x0, y0, x0 + (x1 - x0) * frac, y1), radius=h / 2, fill=255
        )
        img.paste(_lgrad((t, t), *cols), (0, 0), mask)

    out = Image.new("RGBA", (n, n), (0, 0, 0, 0))
    out.paste(img, (pad, pad), img)
    return out.resize((side, side), Image.LANCZOS)


def glyph(side):
    """Tray mark: no tile, black on alpha, which is what a macOS template is."""
    n = side * SS
    img = Image.new("RGBA", (n, n), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    # Tighter margins than the tile: the menu bar gives it less room to start.
    t = n
    m = t * 0.085
    h = t * 0.170
    gap = t * 0.125
    y = (t - (3 * h + 2 * gap)) / 2
    for frac, _ in BARS:
        d.rounded_rectangle((m, y, t - m, y + h), radius=h / 2, fill=(0, 0, 0, 70))
        d.rounded_rectangle((m, y, m + (t - 2 * m) * frac, y + h), radius=h / 2, fill=(0, 0, 0, 255))
        y += h + gap
    return img.resize((side, side), Image.LANCZOS)


def write_icns(path, master):
    """icns is a header plus OSType blocks; the modern types take a raw PNG."""
    types = [
        (b"ic07", 128), (b"ic08", 256), (b"ic09", 512), (b"ic10", 1024),
        (b"ic11", 32), (b"ic12", 64), (b"ic13", 256), (b"ic14", 512),
    ]
    blocks = b""
    for ostype, size in types:
        buf = io.BytesIO()
        master.resize((size, size), Image.LANCZOS).save(buf, format="PNG")
        data = buf.getvalue()
        blocks += ostype + struct.pack(">I", len(data) + 8) + data
    with open(path, "wb") as f:
        f.write(b"icns" + struct.pack(">I", len(blocks) + 8) + blocks)


def verify_icns(path):
    with open(path, "rb") as f:
        blob = f.read()
    assert blob[:4] == b"icns", "bad magic"
    total = struct.unpack(">I", blob[4:8])[0]
    assert total == len(blob), f"header says {total}, file is {len(blob)}"
    off, seen = 8, []
    while off < len(blob):
        ostype = blob[off:off + 4]
        length = struct.unpack(">I", blob[off + 4:off + 8])[0]
        assert length >= 8 and off + length <= len(blob), f"bad block at {off}"
        assert blob[off + 8:off + 12] == b"\x89PNG", f"{ostype!r} is not a PNG"
        seen.append(ostype.decode())
        off += length
    return seen


def main():
    os.makedirs(OUT, exist_ok=True)
    p = lambda *a: os.path.join(OUT, *a)

    # Full bleed for Windows; the squircle inset for macOS, whose icon grid
    # expects the art to sit inside a transparent margin.
    win = mark(1024, inset_frac=0.0, corner_frac=0.185)
    mac = mark(1024, inset_frac=0.098, corner_frac=0.225)

    win.save(p("icon.png"))
    for size in (32, 128, 256, 512):
        win.resize((size, size), Image.LANCZOS).save(p(f"{size}x{size}.png"))
    win.resize((256, 256), Image.LANCZOS).save(p("128x128@2x.png"))

    for name, size in (
        ("Square30x30Logo", 30), ("Square44x44Logo", 44), ("Square71x71Logo", 71),
        ("Square89x89Logo", 89), ("Square107x107Logo", 107), ("Square142x142Logo", 142),
        ("Square150x150Logo", 150), ("Square284x284Logo", 284), ("Square310x310Logo", 310),
        ("StoreLogo", 50),
    ):
        win.resize((size, size), Image.LANCZOS).save(p(f"{name}.png"))

    win.save(p("icon.ico"), sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])
    write_icns(p("icon.icns"), mac)

    glyph(22).save(p("tray-mac.png"))
    glyph(44).save(p("tray-mac@2x.png"))
    glyph(32).save(p("tray.png"))

    print("icns blocks:", " ".join(verify_icns(p("icon.icns"))))
    print("ico sizes:", sorted(Image.open(p("icon.ico")).ico.sizes()))
    print("files:", len(os.listdir(OUT)))


if __name__ == "__main__":
    sys.exit(main())
