#!/usr/bin/env python3
"""The files the `cat` suite floods a terminal with, built from a seed.

ghostty's published comparison is `time cat` over a 150 MB file, once
ASCII and once mixed-language Unicode; the third kind here adds the CSI
traffic a TUI actually emits. What matters is that every terminal in the
field — and every run on every machine — receives the *same bytes*, so
the payloads are generated from a fixed seed rather than downloaded or
captured, and cached under `target/bench-payloads/`.

Two properties the recipes are built for:

- **The payload never depends on the grid.** vtebench sizes its work
  from the tty, which is why an unequal grid there means unequal
  payloads; here the bytes are the same everywhere and the grid changes
  rendering load only. That is also what lets the same file be reused
  between runs.
- **No block repeats forever.** The file is `BLOCKS` distinct blocks
  cycled to length, not one block repeated: a single repeated block is
  a friendlier branch-prediction target than any real output, and the
  cost of generating a few more is paid once.

Build one by hand:

    python3 tools/bench/payloads.py ascii --megabytes 8 --dir /tmp/p
"""

from __future__ import annotations

import argparse
import hashlib
import random
import sys
from pathlib import Path

# Any fixed number would do; it is recorded in the results so a payload
# can be rebuilt byte-for-byte from a report that used it.
SEED = 20260821
BLOCK_BYTES = 1 << 20
BLOCKS = 16

ASCII_ALPHABET = (
    bytes(b for b in range(0x20, 0x7F)) + b"\t"
)  # the tab a text file carries

# Mixed scripts, so the payload exercises multi-byte decoding, wide
# cells, combining marks and emoji rather than only Latin-1 in UTF-8.
UNICODE_WORDS = [
    "the quick brown fox",
    "日本語のテキストが混ざる",
    "中文字符宽度为两格",
    "한국어도 함께 들어간다",
    "Ελληνικά γράμματα",
    "Кириллица тоже здесь",
    "מילים בעברית",
    "combining áèîõü",
    "emoji 😀🔥🚀 and flags 🇯🇵🇩🇪",
    "symbols → ⇒ ∞ ± × ÷ ¶ †",
]

# One entry per thing a TUI does between characters. Widths and counts
# are small on purpose: the point is the parser's per-sequence cost, not
# a scroll storm.
CSI_CHUNKS = [
    "\x1b[m",
    "\x1b[1;31m",
    "\x1b[38;5;214m",
    "\x1b[38:2:125:136:147m",
    "\x1b[4:3;58;5;44m",
    "\x1b[7m",
    "\x1b[H",
    "\x1b[12;40H",
    "\x1b[2K",
    "\x1b[3A\x1b[5C",
    "\x1b[?25l",
    "\x1b[?25h",
]


def ascii_block(rng: random.Random) -> bytes:
    """Printable ASCII in lines of varied length."""
    table = bytes(ASCII_ALPHABET[b % len(ASCII_ALPHABET)] for b in range(256))
    data = bytearray(rng.randbytes(BLOCK_BYTES).translate(table))
    pos = 0
    while pos < len(data):
        pos += rng.randint(20, 100)
        if pos < len(data):
            data[pos] = 0x0A
    return bytes(data)


def unicode_block(rng: random.Random) -> bytes:
    """Lines assembled from the mixed-script word list."""
    out: list[str] = []
    size = 0
    while size < BLOCK_BYTES:
        line = " ".join(rng.choices(UNICODE_WORDS, k=rng.randint(2, 6))) + "\n"
        out.append(line)
        size += len(line.encode())
    return "".join(out).encode()


def csi_block(rng: random.Random) -> bytes:
    """Short text runs separated by the escape sequences a TUI emits."""
    out: list[str] = []
    size = 0
    while size < BLOCK_BYTES:
        piece = rng.choice(CSI_CHUNKS)
        if rng.random() < 0.35:
            piece += "".join(
                chr(rng.randint(0x21, 0x7E)) for _ in range(rng.randint(1, 40))
            )
        out.append(piece)
        size += len(piece)
    out.append("\x1b[m\n")
    return "".join(out).encode()


BUILDERS = {"ascii": ascii_block, "unicode": unicode_block, "csi": csi_block}
KINDS = tuple(BUILDERS)


def build(kind: str, megabytes: int, directory: Path) -> Path:
    """The payload file, generated on first use and cached afterwards.

    Reused across runs: the bytes are a function of (kind, size, SEED)
    alone, so a cached file from last week is the same file this run
    would write. The digest sidecar is what a report leans on to say so.
    """
    directory.mkdir(parents=True, exist_ok=True)
    want = megabytes << 20
    path = directory / f"{kind}-{megabytes}mb.bin"
    if path.exists() and path.stat().st_size == want:
        return path
    rng = random.Random(f"{SEED}:{kind}")
    blocks = [BUILDERS[kind](rng) for _ in range(BLOCKS)]
    digest = hashlib.sha256()
    written = 0
    with path.open("wb") as sink:
        i = 0
        while written < want:
            block = blocks[i % BLOCKS][: want - written]
            sink.write(block)
            digest.update(block)
            written += len(block)
            i += 1
    path.with_suffix(".sha256").write_text(digest.hexdigest() + "\n")
    return path


def digest(path: Path) -> str | None:
    sidecar = path.with_suffix(".sha256")
    return sidecar.read_text().strip() if sidecar.exists() else None


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("kind", choices=KINDS)
    parser.add_argument("--megabytes", type=int, default=150)
    parser.add_argument("--dir", type=Path, default=Path("target/bench-payloads"))
    args = parser.parse_args(argv)
    path = build(args.kind, args.megabytes, args.dir)
    print(f"{path} ({path.stat().st_size} bytes, sha256 {digest(path)})")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
