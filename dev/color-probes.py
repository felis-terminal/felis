"""Builds the color-glyph probe fonts the felis-shaping tests read.

Usage: color-probes.py OUT_DIR

Each face holds only what a test needs from it; the glyph comments name it.
PNG images are written here, so the build needs nothing beyond fontTools.
"""

import struct
import sys
import zlib
from pathlib import Path

from fontTools.colorLib.builder import buildCOLR, buildCPAL
from fontTools.fontBuilder import FontBuilder
from fontTools.pens.ttGlyphPen import TTGlyphPen
from fontTools.ttLib import newTable
from fontTools.ttLib.tables.DefaultTable import DefaultTable
from fontTools.ttLib.tables import otTables as ot
from fontTools.ttLib.tables.sbixGlyph import Glyph as SbixGlyph
from fontTools.ttLib.tables.sbixStrike import Strike as SbixStrike
from fontTools.ttLib.tables.ttProgram import Program

UPEM = 1000


def rect(x0, y0, x1, y1):
    pen = TTGlyphPen(None)
    pen.moveTo((x0, y0))
    pen.lineTo((x0, y1))
    pen.lineTo((x1, y1))
    pen.lineTo((x1, y0))
    pen.closePath()
    return pen.glyph()


def sliver():
    """An inkless triangle of collinear points by (1000, 1000)."""
    pen = TTGlyphPen(None)
    pen.moveTo((990, 990))
    pen.lineTo((995, 995))
    pen.lineTo((1000, 1000))
    pen.closePath()
    return pen.glyph()


def empty():
    return TTGlyphPen(None).glyph()


def encode_png(width, height, color_type, bit_depth, rows, chunks=()):
    """A PNG of `rows`, each a row's packed samples before filtering, with
    `chunks` as (kind, data) between the header and the image data."""

    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))

    ihdr = struct.pack(">IIBBBBB", width, height, bit_depth, color_type, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + b"".join(chunk(kind, data) for kind, data in chunks)
        + chunk(b"IDAT", zlib.compress(b"".join(b"\0" + row for row in rows)))
        + chunk(b"IEND", b"")
    )


def png(width, height, pixel, ihdr=None):
    """An RGBA PNG whose pixel at (x, y) is pixel(x, y); `ihdr` overrides
    the size the header claims and drops the image data."""
    if ihdr:
        return encode_png(*ihdr, 6, 8, [])[:33]
    rows = [b"".join(bytes(pixel(x, y)) for x in range(width)) for y in range(height)]
    return encode_png(width, height, 6, 8, rows)


def png_formats(size):
    """One PNG per color type and sample depth the format allows beyond
    8-bit RGBA, keyed by the glyph that shows it."""
    alpha = [[disc(size)(x, y)[3] for x in range(size)] for y in range(size)]
    index = [[(x // 5 + y // 5) % 4 for x in range(size)] for y in range(size)]
    palette = bytes([230, 40, 30, 20, 120, 220, 250, 210, 40, 30, 30, 30])

    def rows(sample):
        return [b"".join(bytes(sample(x, y)) for x in range(size)) for y in range(size)]

    def two_bit(x, y):
        return (x + y) % 4

    packed = [
        bytes(
            sum(two_bit(x + i, y) << (6 - 2 * i) for i in range(4) if x + i < size)
            for x in range(0, size, 4)
        )
        for y in range(size)
    ]
    return {
        # F: 8-bit RGB.  G: 8-bit gray.  H: 8-bit gray with alpha.
        "F": encode_png(size, size, 2, 8, rows(lambda x, y: (x * 14, y * 14, 128))),
        "G": encode_png(size, size, 0, 8, rows(lambda x, y: ((x + y) * 7,))),
        "H": encode_png(size, size, 4, 8, rows(lambda x, y: (x * 14, alpha[y][x]))),
        # I: a palette without tRNS.  J: the palette with tRNS alphas.
        "I": encode_png(
            size, size, 3, 8, rows(lambda x, y: (index[y][x],)), [(b"PLTE", palette)]
        ),
        "J": encode_png(
            size,
            size,
            3,
            8,
            rows(lambda x, y: (index[y][x],)),
            [(b"PLTE", palette), (b"tRNS", bytes([255, 128, 0, 64]))],
        ),
        # K: 2-bit gray.  L: 16-bit RGBA.
        "K": encode_png(size, size, 0, 2, packed),
        "L": encode_png(
            size,
            size,
            6,
            16,
            rows(
                lambda x, y: struct.pack(
                    ">HHHH", x * 3600, y * 3600, 0x8080 + x, alpha[y][x] * 257
                )
            ),
        ),
    }


def disc(size):
    """A red disc with a soft translucent edge on a transparent field."""

    def pixel(x, y):
        c = (size - 1) / 2
        d = ((x - c) ** 2 + (y - c) ** 2) ** 0.5
        alpha = max(0, min(255, round((size / 2 - d) * 128)))
        return (230, 40, 30, alpha)

    return pixel


def base(names, family, cmap):
    fb = FontBuilder(UPEM, isTTF=True)
    fb.setupGlyphOrder(names)
    fb.setupCharacterMap(cmap)
    fb.setupHorizontalMetrics({name: (1000, 0) for name in names})
    fb.setupHorizontalHeader(ascent=800, descent=-200)
    fb.setupNameTable({"familyName": family, "styleName": "Regular"})
    fb.setupOS2(
        sTypoAscender=800, sTypoDescender=-200, usWinAscent=800, usWinDescent=200
    )
    fb.setupPost()
    return fb


def record(cls, **fields):
    value = cls()
    for key, field in fields.items():
        setattr(value, key, field)
    return value


def color_probe(out):
    # A: version 0 layers and a version 1 paint.  B: version 1 only.
    # C: two layers 20 em apart.  D: layers that paint no ink.
    # E: two small layers at opposite corners of an em square.
    # F: E's first layer, and a layer inkless at the opposite corner.
    # G: A's version 0 layers alone.
    layer_names = ["lay1", "lay2", "far1", "far2", "blank", "corner1", "corner2"]
    names = [".notdef", "A", "B", "C", "D", "E", "F", "G", *layer_names, "sliver"]
    cmap = {ord(name): name for name in "ABCDEFG"}
    fb = base(names, "Felis Color Probe", cmap)
    glyphs = {name: rect(50, 0, 550, 700) for name in names}
    glyphs.update(
        lay1=rect(50, 0, 550, 400),
        lay2=rect(150, 300, 450, 700),
        far1=rect(0, 0, 500, 500),
        far2=rect(20000, 0, 20500, 500),
        blank=empty(),
        corner1=rect(0, 0, 10, 10),
        corner2=rect(990, 990, 1000, 1000),
        sliver=sliver(),
    )
    fb.setupGlyf(glyphs)
    # TrueType draws an outline shifted so its xMin sits at the lsb.
    glyf = fb.font["glyf"]
    fb.setupHorizontalMetrics(
        {name: (600, getattr(glyf[name], "xMin", 0)) for name in names}
    )
    fb.font["CPAL"] = buildCPAL([[(1, 0, 0, 1), (0, 0, 1, 0.5), (0, 1, 0, 1)]])
    paint = {
        "Format": ot.PaintFormat.PaintGlyph,
        "Glyph": "lay1",
        "Paint": {"Format": ot.PaintFormat.PaintSolid, "PaletteIndex": 2, "Alpha": 1.0},
    }
    colr = buildCOLR({"A": paint, "B": paint}, version=1)
    layers = {
        "A": [("lay1", 0), ("lay2", 1)],
        "C": [("far1", 0), ("far2", 2)],
        "D": [("blank", 0)],
        "E": [("corner1", 0), ("corner2", 2)],
        "F": [("corner1", 0), ("sliver", 2)],
        "G": [("lay1", 0), ("lay2", 1)],
    }
    table = colr.table
    table.BaseGlyphRecordArray = record(ot.BaseGlyphRecordArray, BaseGlyphRecord=[])
    table.LayerRecordArray = record(ot.LayerRecordArray, LayerRecord=[])
    for glyph, glyph_layers in layers.items():
        table.BaseGlyphRecordArray.BaseGlyphRecord.append(
            record(
                ot.BaseGlyphRecord,
                BaseGlyph=glyph,
                FirstLayerIndex=len(table.LayerRecordArray.LayerRecord),
                NumLayers=len(glyph_layers),
            )
        )
        table.LayerRecordArray.LayerRecord.extend(
            record(ot.LayerRecord, LayerGlyph=layer, PaletteIndex=index)
            for layer, index in glyph_layers
        )
    table.BaseGlyphRecordCount = len(layers)
    table.LayerRecordCount = len(table.LayerRecordArray.LayerRecord)
    fb.font["COLR"] = colr
    fb.save(out / "FelisColorProbe.ttf")
    del fb.font["CPAL"]
    fb.save(out / "FelisColorProbeNoCpal.ttf")


def small_metrics(width, height, bearing_x, bearing_y, advance):
    return struct.pack(">BBbbB", height, width, bearing_x, bearing_y, advance)


def index_subtable(image_format, start, end):
    """Format 1: one glyph's offset into CBDT and the next."""
    return struct.pack(">HHI", 1, image_format, start) + struct.pack(
        ">II", 0, end - start
    )


def bitmap_probe(out):
    # A 20 ppem strike of 32-bit color.  A: a translucent PNG.
    # B: premultiplied BGRA.  C: a PNG whose header claims 30 000 × 30 000
    # and holds no image.  F to L: PNGs of the other color types.
    # A 32 ppem strike of 1-bit masks.  D: a 255 × 255 solid mask, bit
    # aligned.  E: a 3 × 2 mask of rows 101 and 011, byte aligned.
    size = 18
    formats = png_formats(size)
    names = [".notdef", "A", "B", "C", *formats, "D", "E"]
    cmap = {ord(name): name for name in names[1:]}
    fb = base(names, "Felis Bitmap Probe", cmap)
    fb.setupGlyf(
        {name: rect(0, 0, 500, 500) if name == "C" else empty() for name in names}
    )
    translucent = png(size, size, disc(size))
    huge = png(1, 1, None, ihdr=(30000, 30000))
    solid = 255
    strikes = [
        (
            20,
            32,
            [
                (
                    17,
                    small_metrics(size, size, 1, 16, 20)
                    + struct.pack(">I", len(translucent))
                    + translucent,
                ),
                (
                    1,
                    small_metrics(2, 2, 3, 12, 20)
                    + bytes([0x40, 0x20, 0x10, 0x80, 0, 0, 0, 0]) * 2,
                ),
                (
                    17,
                    small_metrics(size, size, 1, 16, 20)
                    + struct.pack(">I", len(huge))
                    + huge,
                ),
                *(
                    (
                        17,
                        small_metrics(size, size, 1, 16, 20)
                        + struct.pack(">I", len(image))
                        + image,
                    )
                    for image in formats.values()
                ),
            ],
        ),
        (
            32,
            1,
            [
                (
                    2,
                    small_metrics(solid, solid, 0, 30, 32)
                    + b"\xff" * ((solid * solid + 7) // 8),
                ),
                (1, small_metrics(3, 2, 1, 2, 32) + bytes([0b10100000, 0b01100000])),
            ],
        ),
    ]
    cbdt = struct.pack(">HH", 3, 0)
    header_len, size_len = 8, 48
    list_offset = header_len + size_len * len(strikes)
    sizes, lists = b"", b""
    first_glyph = 1
    for ppem, bit_depth, glyphs in strikes:
        records, bodies = b"", b""
        body_offset = 8 * len(glyphs)
        for i, (image_format, data) in enumerate(glyphs):
            glyph = first_glyph + i
            records += struct.pack(">HHI", glyph, glyph, body_offset + len(bodies))
            bodies += index_subtable(image_format, len(cbdt), len(cbdt) + len(data))
            cbdt += data
        index_list = records + bodies
        line = struct.pack(
            ">bbBbbbbbbbbb", ppem * 4 // 5, -ppem // 5, ppem, 1, 0, 0, 0, 0, 0, 0, 0, 0
        )
        last_glyph = first_glyph + len(glyphs) - 1
        sizes += (
            struct.pack(">III", list_offset + len(lists), len(index_list), len(glyphs))
            + struct.pack(">I", 0)
            + line
            + line
            + struct.pack(">HHBBBb", first_glyph, last_glyph, ppem, ppem, bit_depth, 1)
        )
        lists += index_list
        first_glyph = last_glyph + 1
    cblc = struct.pack(">HHI", 3, 0, len(strikes)) + sizes + lists
    for tag, data in (("CBDT", cbdt), ("CBLC", cblc)):
        table = DefaultTable(tag)
        table.data = data
        fb.font[tag] = table
    fb.save(out / "FelisBitmapProbe.ttf")


def sbix_probe(out):
    # The family Apple Color Emoji names, so the placement it gets is pinned.
    # A: a PNG at origin offset 0 in strikes of 20 and 40 ppem.  B: a dupe
    # of A.  C: a PNG with a nonzero origin offset.
    names = [".notdef", "A", "B", "C"]
    fb = base(names, "Apple Color Emoji", {0x41: "A", 0x42: "B", 0x43: "C"})
    fb.setupGlyf({name: empty() for name in names})
    sbix = newTable("sbix")
    sbix.version = 1
    sbix.flags = 1
    sbix.strikes = {}
    for ppem in (20, 40):
        strike = SbixStrike(ppem=ppem, resolution=72)
        size = ppem - 2
        strike.glyphs["A"] = SbixGlyph(
            glyphName="A", graphicType="png ", imageData=png(size, size, disc(size))
        )
        strike.glyphs["B"] = SbixGlyph(
            glyphName="B", graphicType="dupe", referenceGlyphName="A"
        )
        strike.glyphs["C"] = SbixGlyph(
            glyphName="C",
            graphicType="png ",
            imageData=png(size, size, disc(size)),
            originOffsetX=2,
            originOffsetY=-3,
        )
        sbix.strikes[ppem] = strike
    fb.font["sbix"] = sbix
    fb.save(out / "FelisSbixProbe.ttf")


def program(bytecode):
    instructions = Program()
    instructions.fromBytecode(bytecode)
    return instructions


def hinted_probe(out):
    # A: one version 0 layer, B, whose glyph program lifts its top edge
    # 3 px above where its outline puts it.  B is mapped as well, so its
    # outline draws on its own.
    names = [".notdef", "A", "B"]
    fb = base(names, "Felis Hinted Probe", {0x41: "A", 0x42: "B"})
    lifted = rect(100, 0, 500, 500)
    # On the y axis, points 1 and 2 (the top corners) are touched in
    # place, which backward-compatible hinting needs before it shifts a
    # point, then shifted by 3 px (192 in 26.6).
    lifted.program = program(
        bytes(
            [0x00, 0xB1, 1, 2, 0x2E, 0x2E, 0xB1, 1, 2, 0xB0, 2, 0x17, 0xB0, 192, 0x38]
        )
    )
    fb.setupGlyf({".notdef": empty(), "A": empty(), "B": lifted})
    fb.setupHorizontalMetrics({".notdef": (600, 0), "A": (600, 0), "B": (600, 100)})
    for tag in ("fpgm", "prep"):
        table = newTable(tag)
        table.program = program(b"")
        fb.font[tag] = table
    maxp = fb.font["maxp"]
    maxp.maxStackElements = 16
    maxp.maxSizeOfInstructions = 16
    maxp.maxZones = 1
    fb.font["CPAL"] = buildCPAL([[(1, 0, 0, 1)]])
    colr = buildCOLR({"A": [("B", 0)]})
    fb.font["COLR"] = colr
    fb.save(out / "FelisHintedProbe.ttf")


def main():
    out = Path(sys.argv[1])
    out.mkdir(parents=True, exist_ok=True)
    color_probe(out)
    bitmap_probe(out)
    sbix_probe(out)
    hinted_probe(out)


if __name__ == "__main__":
    main()
