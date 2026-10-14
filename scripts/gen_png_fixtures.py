#!/usr/bin/env python3
"""PNG fixtures and expected outputs for the firmware's decoder (src/png.rs).

    gen_png_fixtures.py <out_dir> [captured.png ...]

Writes into out_dir:
- synthetic PNGs: greyscale at depths 1/2/4/8, every row filter, IDAT
  split across many chunks, an ancillary chunk, odd widths (partial
  bytes), plus one full-panel 1280x720 image; and malformed or unsupported
  variants, each expected to fail with a specific error;
- for every captured.png given (real server output, e.g. Makespace's
  trouble-tickets.png): its expected decode, from an independent Python
  decoder;
- manifest.txt: `<png> <width> <height> ok:<expected.1bpp>:<expected.lv4>`
  or `<png> <width> <height> err:<PngError variant>`.

Expected outputs: `.1bpp` is row-major, MSB leftmost, 1 = black, where a
pixel is black iff its sample is below half the depth's range (the
two-tone path); `.lv4` is the panel-level buffer of the greyscale path:
each sample's nearest of 8 levels (0 = black ..= 7 = white, round half
up), 4bpp, even x in the high nibble. Synthetic expectations come straight
from the source samples. Every `.1bpp` expectation is cross-checked against
Pillow when it is installed; otherwise the script is stdlib only (zlib,
struct).
"""

import random
import struct
import sys
import zlib
from pathlib import Path


def chunk(kind: bytes, data: bytes) -> bytes:
    return (struct.pack(">I", len(data)) + kind + data
            + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF))


def pack_row(samples, depth):
    out, acc, nbits = bytearray(), 0, 0
    for s in samples:
        acc = (acc << depth) | s
        nbits += depth
        if nbits == 8:
            out.append(acc)
            acc, nbits = 0, 0
    if nbits:
        out.append(acc << (8 - nbits))
    return bytes(out)


def paeth(a, b, c):
    p = a + b - c
    pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
    return a if pa <= pb and pa <= pc else (b if pb <= pc else c)


def filter_row(ftype, cur, prev):
    out = bytearray()
    for i, x in enumerate(cur):
        a = cur[i - 1] if i else 0
        b = prev[i]
        c = prev[i - 1] if i else 0
        pred = [0, a, b, (a + b) // 2, paeth(a, b, c)][ftype]
        out.append((x - pred) & 0xFF)
    return bytes(out)


def encode(samples_rows, depth, *, filters, idat_split=None, colour=0,
           interlace=0, extra_before_idat=b""):
    h, w = len(samples_rows), len(samples_rows[0])
    raw, prev = bytearray(), bytes((w * depth + 7) // 8)
    for y, row in enumerate(samples_rows):
        cur = pack_row(row, depth)
        f = filters[y % len(filters)]
        raw.append(f)
        raw += filter_row(f, cur, prev)
        prev = cur
    z = zlib.compress(bytes(raw), 9)
    parts = [z] if not idat_split else [z[i:i + idat_split] for i in range(0, len(z), idat_split)]
    ihdr = struct.pack(">IIBBBBB", w, h, depth, colour, 0, 0, interlace)
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + extra_before_idat
            + b"".join(chunk(b"IDAT", p) for p in parts) + chunk(b"IEND", b""))


def expected_1bpp(samples_rows, depth):
    half = 1 << (depth - 1)
    out = bytearray()
    for row in samples_rows:
        out += pack_row([1 if s < half else 0 for s in row], 1)
    return bytes(out)


def reference_decode(png: bytes):
    """Independent greyscale PNG decode → (w, h, depth, samples_rows)."""
    assert png[:8] == b"\x89PNG\r\n\x1a\n"
    pos, idat, ihdr = 8, b"", None
    while pos < len(png):
        n, = struct.unpack(">I", png[pos:pos + 4])
        kind, data = png[pos + 4:pos + 8], png[pos + 8:pos + 8 + n]
        pos += 12 + n
        if kind == b"IHDR":
            ihdr = struct.unpack(">IIBBBBB", data)
        elif kind == b"IDAT":
            idat += data
        elif kind == b"IEND":
            break
    w, h, depth, colour, _, _, interlace = ihdr
    assert colour == 0 and interlace == 0
    stride = (w * depth + 7) // 8
    raw, rows, prev = zlib.decompress(idat), [], bytes(stride)
    for y in range(h):
        f, cur = raw[y * (stride + 1)], bytearray(raw[y * (stride + 1) + 1:(y + 1) * (stride + 1)])
        for i in range(stride):
            a = cur[i - 1] if i else 0
            b = prev[i]
            c = prev[i - 1] if i else 0
            cur[i] = (cur[i] + [0, a, b, (a + b) // 2, paeth(a, b, c)][f]) & 0xFF
        prev = bytes(cur)
        mask = (1 << depth) - 1
        rows.append([(cur[(x * depth) // 8] >> (8 - depth - (x * depth) % 8)) & mask for x in range(w)])
    return w, h, depth, rows


def expected_levels(samples_rows, depth):
    top = (1 << depth) - 1
    out = bytearray()
    for row in samples_rows:
        lv = [(s * 14 + top) // (2 * top) for s in row] + [0]
        out += bytes((lv[x] << 4) | (lv[x + 1] if x + 1 < len(row) else 0)
                     for x in range(0, len(row), 2))
    return bytes(out)


def pillow_check(path, expected, w, h):
    try:
        from PIL import Image
    except ImportError:
        return
    img = Image.open(path).convert("L")
    assert img.size == (w, h), (path, img.size)
    px = img.load()
    got = expected_1bpp([[0 if px[x, y] < 128 else 1 for x in range(w)] for y in range(h)], 1)
    assert got == expected, f"Pillow disagrees with expected output for {path}"


def main():
    out = Path(sys.argv[1])
    out.mkdir(parents=True, exist_ok=True)
    rng = random.Random(20261009)
    manifest = []

    def emit_ok(name, png, rows, depth):
        w, h = len(rows[0]), len(rows)
        (out / f"{name}.png").write_bytes(png)
        exp = expected_1bpp(rows, depth)
        (out / f"{name}.1bpp").write_bytes(exp)
        (out / f"{name}.lv4").write_bytes(expected_levels(rows, depth))
        pillow_check(out / f"{name}.png", exp, w, h)
        manifest.append(f"{name}.png {w} {h} ok:{name}.1bpp:{name}.lv4")

    def emit_err(name, png, w, h, err):
        (out / f"{name}.png").write_bytes(png)
        manifest.append(f"{name}.png {w} {h} err:{err}")

    def image(w, h, depth):
        top = (1 << depth) - 1
        # Gradient + noise + solid runs: exercises every filter predictor.
        return [[(x * top // max(1, w - 1)) if y % 3 == 0 else
                 (rng.randrange(top + 1) if y % 3 == 1 else (top if (x // 5) % 2 else 0))
                 for x in range(w)] for y in range(h)]

    text = chunk(b"tEXt", b"Comment\x00fixture")
    for depth in (1, 2, 4, 8):
        rows = image(61, 37, depth)
        for f in range(5):
            emit_ok(f"syn_d{depth}_f{f}", encode(rows, depth, filters=[f]), rows, depth)
        emit_ok(f"syn_d{depth}_mixed_split",
                encode(rows, depth, filters=[0, 1, 2, 3, 4], idat_split=97,
                       extra_before_idat=text), rows, depth)
    panel = image(1280, 720, 2)
    emit_ok("syn_panel_1280x720_d2", encode(panel, 2, filters=[0, 1, 2, 3, 4], idat_split=8192),
            panel, 2)

    # Malformed and unsupported inputs: each must fail, and fail precisely.
    rows = image(61, 37, 2)
    good = encode(rows, 2, filters=[0, 4])
    bad_crc = bytearray(good)
    bad_crc[8 + 8 + 13] ^= 0xFF                      # IHDR CRC
    emit_err("bad_crc", bytes(bad_crc), 61, 37, "BadCrc")
    emit_err("not_png", b"GIF89a" + good[6:], 61, 37, "NotPng")
    emit_err("truncated", good[:-20], 61, 37, "Truncated")
    emit_err("no_iend", good[:-12], 61, 37, "Truncated")
    emit_err("size_mismatch", good, 64, 37, "SizeMismatch")
    emit_err("rgb", encode([[0] * 61] * 37, 8, filters=[0], colour=2), 61, 37, "Unsupported")
    emit_err("interlaced", encode(rows, 2, filters=[0], interlace=1), 61, 37, "Unsupported")
    emit_err("plte_chunk", encode(rows, 2, filters=[0], extra_before_idat=chunk(b"PLTE", b"\x00" * 3)),
             61, 37, "Unsupported")
    # A filter byte outside 0..=4, re-compressed so zlib stays valid.
    raw = bytearray(zlib.decompress(b"".join(
        good[p + 8:p + 8 + struct.unpack(">I", good[p:p + 4])[0]]
        for p in [good.index(b"IDAT") - 4])))
    raw[0] = 7
    emit_err("bad_filter", good[:good.index(b"IDAT") - 4] + chunk(b"IDAT", zlib.compress(bytes(raw)))
             + chunk(b"IEND", b""), 61, 37, "BadFilter")
    zbad = bytearray(zlib.compress(bytes(raw)))
    zbad[-1] ^= 0xFF                                 # Adler-32
    emit_err("bad_adler", good[:good.index(b"IDAT") - 4] + chunk(b"IDAT", bytes(zbad))
             + chunk(b"IEND", b""), 61, 37, "Inflate")

    # Real server captures: expected output from the reference decoder.
    for cap in map(Path, sys.argv[2:]):
        png = cap.read_bytes()
        w, h, depth, rows = reference_decode(png)
        name = f"cap_{cap.stem}"
        emit_ok(name, png, rows, depth)

    (out / "manifest.txt").write_text("\n".join(manifest) + "\n")
    print(f"{len(manifest)} fixtures -> {out}")


if __name__ == "__main__":
    main()
