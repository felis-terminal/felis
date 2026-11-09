"""Compares felis's color glyphs with cairo's, both written as
cairo_reference.c describes, over black and over white.

Usage: compare.py FELIS_DIR CAIRO_DIR LABEL

Per glyph, every channel of every pixel either image inks is compared
after compositing each image over black and over white. The mean and the
99th percentile take every such channel; the maximum is the edge
allowance, which antialiasing differences between the two rasterizers
spend, and one pixel per glyph may pass it: where the edge of a
two-point conical gradient's cone grazes a pixel center, whether that
pixel paints is decided by the last bits of precision, which pixman's
16.16 coordinates and felis's f32 decide differently. The interior bound
holds where cairo's 3x3 neighborhood is flat, away from any edge. Exits 1 when any glyph is outside a bound, or felis
drew no color glyph where cairo drew one.

A pixel the glyph's clip box (`<glyph>.clip` beside felis's image)
covers in part is skipped: cairo paints each layer and composite as a
group through the clip, so it takes such a pixel's coverage once per
nesting level, even where the box cuts no ink.
"""

import sys
from pathlib import Path

MEAN, P99, MAX, INTERIOR = 2.0, 16, 64, 2


def load(path):
    data = path.read_bytes()
    header, _, pixels = data.partition(b"\n")
    width, height, left, top = map(int, header.split())
    assert len(pixels) == width * height * 4, path
    return width, height, left, top, pixels


def frame(image, x0, y0, width, height):
    """The image's straight RGBA in a frame whose top left pixel is (x0, y0)
    in pixels, y up; transparent outside the image."""
    w, h, left, top, pixels = image
    out = [(0, 0, 0, 0)] * (width * height)
    for row in range(h):
        y = y0 - (top - row)
        for col in range(w):
            x = left + col - x0
            i = (row * w + col) * 4
            out[y * width + x] = tuple(pixels[i : i + 4])
    return out


def over(pixel, background):
    a = pixel[3] / 255
    return [round(c * a + background * (1 - a)) for c in pixel[:3]]


def load_clip(path):
    return tuple(map(float, path.read_text().split())) if path.exists() else None


def partial(clip, x, y):
    """Whether the clip box covers the pixel whose bottom left corner is
    (x, y) in part."""
    if clip is None:
        return False
    x0, y0, x1, y1 = clip
    inside = x0 < x + 1 and x1 > x and y0 < y + 1 and y1 > y
    cut = any(x < v < x + 1 for v in (x0, x1)) or any(y < v < y + 1 for v in (y0, y1))
    return inside and cut


def compare(felis, cairo, clip):
    boxes = [img for img in (felis, cairo) if img[0] and img[1]]
    if not boxes:
        return None
    x0 = min(img[2] for img in boxes)
    x1 = max(img[2] + img[0] for img in boxes)
    y0 = max(img[3] for img in boxes)
    y1 = min(img[3] - img[1] for img in boxes)
    width, height = x1 - x0, y0 - y1
    f, c = frame(felis, x0, y0, width, height), frame(cairo, x0, y0, width, height)
    diffs, pixels, interior = [], [], 0
    for i in range(width * height):
        if f[i][3] == 0 and c[i][3] == 0:
            continue
        x, y = i % width, i // width
        if partial(clip, x0 + x, y0 - y - 1):
            continue
        flat = 0 < x < width - 1 and 0 < y < height - 1
        if flat:
            around = {
                tuple(
                    v for bg in (0, 255) for v in over(c[(y + dy) * width + x + dx], bg)
                )
                for dy in (-1, 0, 1)
                for dx in (-1, 0, 1)
            }
            flat = len(around) == 1
        worst = 0
        for bg in (0, 255):
            delta = [abs(p - q) for p, q in zip(over(f[i], bg), over(c[i], bg))]
            diffs.extend(delta)
            worst = max(worst, *delta)
            if flat:
                interior = max(interior, *delta)
        pixels.append(worst)
    if not diffs:
        return None
    diffs.sort()
    pixels.sort()
    return (
        sum(diffs) / len(diffs),
        diffs[min(len(diffs) - 1, int(len(diffs) * 0.99))],
        pixels[-2] if len(pixels) > 1 else pixels[-1],
        interior,
    )


def main():
    felis_dir, cairo_dir, label = Path(sys.argv[1]), Path(sys.argv[2]), sys.argv[3]
    failed = 0
    worst = [0.0, 0, 0, 0]
    for path in sorted(cairo_dir.glob("*.img"), key=lambda p: int(p.stem)):
        if not (felis_dir / path.name).exists():
            failed += 1
            print(f"{label} glyph {path.stem}: felis drew no color glyph FAIL")
            continue
        clip = load_clip(felis_dir / f"{path.stem}.clip")
        stats = compare(load(felis_dir / path.name), load(path), clip)
        if stats is None:
            continue
        worst = [max(w, s) for w, s in zip(worst, stats)]
        mean, p99, top, interior = stats
        bad = mean > MEAN or p99 > P99 or top > MAX or interior > INTERIOR
        failed += bad
        print(
            f"{label} glyph {path.stem}: mean {mean:.3f} p99 {p99} max {top} interior {interior}"
            + (" FAIL" if bad else "")
        )
    print(
        f"{label} worst: mean {worst[0]:.3f} p99 {worst[1]} max {worst[2]} interior {worst[3]}"
    )
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
