#!/usr/bin/env python3
from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

USAGE = """usage: tools/dep_bans.py
  checks every crate's production dependency closure against the
  "MUST NOT depend on" column of docs/reference/workspace.md
  --self-test   run the hermetic fixture suite
"""
ROOT = Path(__file__).resolve().parent.parent

ASYNC = (
    "tokio",
    "tokio-util",
    "tokio-stream",
    "async-std",
    "smol",
    "mio",
    "polling",
)
OS = (
    "libc",
    "rustix",
    "nix",
    "windows",
    "windows-sys",
    "windows-targets",
    "core-foundation",
    "core-foundation-sys",
    "objc",
    "objc2",
)
RENDER = (
    "felis-shaping",
    "felis-render-core",
    "felis-render-wgpu",
    "swash",
    "zeno",
    "fontdb",
    "cosmic-text",
    "harfbuzz",
    "harfbuzz-sys",
    "harfbuzz_rs",
    "freetype",
    "freetype-rs",
    "freetype-sys",
    "ttf-parser",
    "ab_glyph",
    "ab_glyph_rasterizer",
    "wgpu",
    "wgpu-core",
    "wgpu-hal",
    "wgpu-types",
    "raw-window-handle",
    "winit",
    "bytemuck",
    "tiny-skia",
    "skia-safe",
    "lyon",
    "lyon_tessellation",
    "usvg",
    "resvg",
)
GPU_FREE = ("swash", "wgpu", "winit")

BANS: dict[str, tuple[str, ...]] = {
    "felis-protocol": ASYNC + OS + RENDER,
    "felis-vt": ASYNC + OS + ("felis-grid",),
    "felis-grid": ASYNC + RENDER,
    "felis-pty": ("felis-vt", "felis-grid", "felis-protocol", "felis-transport"),
    "felis-transport": ("felis-vt", "felis-grid", "felis-daemon"),
    "felis-daemon": RENDER,
    "felis-client-core": GPU_FREE,
    "felis-shaping": ("wgpu", "winit"),
    "felis-render-core": ("wgpu", "winit"),
    "felis-render-wgpu": ("winit",),
    "felis-cli": GPU_FREE,
}


def crate_names(tree: str) -> set[str]:
    lines = [line for line in tree.splitlines() if line and not line.startswith("[")]
    return {line.split()[0] for line in lines[1:]}


def violations(bans: tuple[str, ...], tree: str) -> list[str]:
    return sorted(crate_names(tree).intersection(bans))


def cargo_tree(crate: str) -> str:
    cargo = os.environ.get("CARGO", "cargo")
    result = subprocess.run(
        [
            cargo,
            "tree",
            "--package",
            crate,
            "--edges",
            "normal,build",
            "--target",
            "all",
            "--all-features",
            "--prefix",
            "none",
        ],
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        sys.exit(f"cargo tree -p {crate} failed:\n{result.stderr}")
    return result.stdout


def check() -> int:
    failed = False
    for crate, bans in BANS.items():
        found = violations(bans, cargo_tree(crate))
        if found:
            failed = True
            print(f"{crate} reaches banned crate(s): {', '.join(found)}")
            print(
                f"  inspect with: cargo tree -p {crate} -e normal,build --target all --all-features -i <crate>"
            )
    if failed:
        print('see the "MUST NOT depend on" column of docs/reference/workspace.md')
        return 1
    print(f"dep-bans: {len(BANS)} crates clean")
    return 0


FIXTURE = """\
felis-protocol v0.2.0 (/src/crates/felis-protocol)
bytes v1.12.1
serde v1.0.229
serde_derive v1.0.229 (proc-macro)
[build-dependencies]
prost-build v0.14.4
windows-sys v0.61.2
libc v0.2.190
libc v0.2.190 (*)
"""


def self_test() -> int:
    failures = []
    cases = (
        ("ban hits across normal and build sections", OS, ["libc", "windows-sys"]),
        ("crate root is not its own dependency", ("felis-protocol",), []),
        ("names match exactly, not by prefix", ("serde_der", "windows"), []),
        ("section headers are not crate names", ("[build-dependencies]",), []),
    )
    for name, bans, expected in cases:
        got = violations(bans, FIXTURE)
        if got != expected:
            failures.append(f"{name}: expected {expected}, got {got}")
    unknown = sorted(set(BANS) - {p.name for p in (ROOT / "crates").glob("felis-*")})
    if unknown:
        failures.append(f"BANS names crates absent from crates/: {unknown}")
    for failure in failures:
        print(f"FAIL {failure}")
    print("dep-bans self-test: " + ("FAILED" if failures else "ok"))
    return 1 if failures else 0


def main(argv: list[str]) -> int:
    if argv == ["--self-test"]:
        return self_test()
    if argv:
        sys.stderr.write(USAGE)
        return 2
    return check()


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
