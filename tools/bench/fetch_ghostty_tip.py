#!/usr/bin/env python3
"""Fetch ghostty's `tip` pre-release and print the `--tool` line for it.

`tip` is where ghostty's unreleased performance work lands, so putting it
in the field beside the pinned release answers "how much did the next
version gain?" in one chart, under one condition.

The two platforms need different things, which is the whole reason this
exists as a script rather than a line in the docs:

- **Linux** — the bench shell already pins a tip through dev/flake.lock,
  so this is only for measuring a newer one: upstream's flake builds it
  and serves `ghostty.cachix.org`, so nothing is downloaded by hand.
- **macOS** — there is no Nix path. Upstream's flake excludes darwin (the
  build wants Swift 6 / xcodebuild) and nixpkgs' `ghostty-bin` tracks
  tagged releases, so the only source is the universal zip attached to
  the `tip` release, which is rebuilt daily.

`tip` is not reproducible by construction: today's artifact replaces
yesterday's at the same URL. So this records what does identify a
build — the digest of the file, GitHub's `Last-Modified`, and the fetch
time — into `tip.json` beside it. ghostty's own banner happens to carry
a commit (`Ghostty 1.3.2-main-+42a161aad`), but `crossterm.py` digests
any `--tool` binary because a hand-rolled build need not say anything
useful about itself.

That daily churn is also why this is a separate, explicit step rather
than something a run does for itself: a benchmark that silently
re-downloaded a moving target would make two results incomparable
without saying so.

    just bench-vs-fetch-tip
    just bench-vs --tool ghostty-tip=<the path it prints>
"""

from __future__ import annotations

import argparse
import hashlib
import json
import platform
import shutil
import subprocess
import sys
import time
import urllib.error
import urllib.request
import zipfile
from pathlib import Path

RELEASE = "https://github.com/ghostty-org/ghostty/releases/download/tip"
MACOS_ASSET = "ghostty-macos-universal.zip"
FLAKE = "github:ghostty-org/ghostty#ghostty"


def fetch_linux(refresh: bool) -> tuple[Path, dict]:
    """Build from upstream's flake; the cachix substituter does the work."""
    argv = ["nix", "build", FLAKE, "--no-link", "--print-out-paths"]
    # Without --refresh, nix serves the flake from its tarball cache for
    # up to an hour, which for a daily nightly means silently building
    # yesterday's tip and labeling it today's.
    if refresh:
        argv.append("--refresh")
    print(f"building {FLAKE} (upstream serves ghostty.cachix.org)...", flush=True)
    out = subprocess.run(argv, capture_output=True, text=True)
    if out.returncode != 0:
        raise SystemExit(out.stderr.strip() or "nix build failed")
    store = Path(out.stdout.strip().splitlines()[-1])
    # A store path names its own contents, so the path IS the provenance.
    return store / "bin/ghostty", {"source": FLAKE, "store_path": str(store)}


def download(url: str, into: Path) -> dict:
    """Stream an asset to disk, hashing it on the way past."""
    digest = hashlib.sha256()
    tmp = into.with_suffix(into.suffix + ".part")
    try:
        with urllib.request.urlopen(url) as response:
            total = int(response.headers.get("content-length") or 0)
            last_modified = response.headers.get("last-modified")
            done = 0
            with tmp.open("wb") as sink:
                while chunk := response.read(1 << 20):
                    sink.write(chunk)
                    digest.update(chunk)
                    done += len(chunk)
                    if total:
                        print(
                            f"\r  {done / 1024**2:5.1f} / {total / 1024**2:.1f} MB",
                            end="",
                            flush=True,
                        )
            print()
    except urllib.error.HTTPError as err:
        # A half-written file left behind would be unpacked on the next
        # run as though it were whole.
        tmp.unlink(missing_ok=True)
        raise SystemExit(f"{url}: HTTP {err.code} {err.reason}") from err
    except OSError as err:
        tmp.unlink(missing_ok=True)
        raise SystemExit(f"{url}: {err}") from err
    tmp.replace(into)
    return {
        "source": url,
        "sha256": digest.hexdigest(),
        "bytes": into.stat().st_size,
        "last_modified": last_modified,
    }


def fetch_macos(dest: Path, force: bool) -> tuple[Path, dict]:
    binary = dest / "Ghostty.app/Contents/MacOS/ghostty"
    manifest_path = dest / "tip.json"
    if binary.exists() and not force:
        print(f"already unpacked: {binary}\n(pass --force to fetch today's build)")
        manifest = {}
        if manifest_path.exists():
            manifest = json.loads(manifest_path.read_text())
        return binary, manifest

    dest.mkdir(parents=True, exist_ok=True)
    archive = dest / MACOS_ASSET
    url = f"{RELEASE}/{MACOS_ASSET}"
    print(f"downloading {url}", flush=True)
    manifest = download(url, archive)

    if (app := dest / "Ghostty.app").exists():
        shutil.rmtree(app)
    with zipfile.ZipFile(archive) as zf:
        zf.extractall(dest)
    archive.unlink()
    if not binary.exists():
        raise SystemExit(f"the archive did not contain {binary.relative_to(dest)}")
    # ZipFile drops the executable bit, and a bundle whose binary is not
    # executable fails with a launch error that names nothing useful.
    for path in binary.parent.iterdir():
        path.chmod(0o755)

    # Gatekeeper's App Translocation is the trap here: a quarantined
    # bundle launched from an arbitrary directory is copied to a random
    # read-only mount and re-executed there, so the process the suite
    # started detaches and never exits — that leg then burns its whole
    # timeout and reports nothing. Seen exactly once in this repo, as a
    # translocated Ghostty.app outliving its launcher.
    subprocess.run(
        ["xattr", "-dr", "com.apple.quarantine", str(app)], capture_output=True
    )
    return binary, manifest


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--dest",
        type=Path,
        default=Path("target/bench-tools/ghostty-tip"),
        help="where the macOS bundle is unpacked (default: %(default)s)",
    )
    parser.add_argument(
        "--force",
        action="store_true",
        help="fetch again even if a build is already unpacked",
    )
    parser.add_argument(
        "--no-refresh",
        action="store_true",
        help="Linux: accept nix's cached flake instead of re-resolving it",
    )
    args = parser.parse_args(argv)

    dest = args.dest.resolve()
    if platform.system() == "Darwin":
        binary, manifest = fetch_macos(dest, args.force)
    else:
        binary, manifest = fetch_linux(not args.no_refresh)
    if not binary.exists():
        raise SystemExit(f"expected a binary at {binary}, found nothing")

    banner = subprocess.run(
        [str(binary), "+version"], capture_output=True, text=True
    ).stdout
    manifest |= {
        "binary": str(binary),
        "version": banner.splitlines()[0].strip() if banner else None,
        "fetched": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
    }
    dest.mkdir(parents=True, exist_ok=True)
    (dest / "tip.json").write_text(json.dumps(manifest, indent=2) + "\n")

    print()
    for key in ("version", "last_modified", "sha256", "store_path"):
        if value := manifest.get(key):
            print(f"  {key:14} {value}")
    print(f"\n  just bench-vs --tool ghostty-tip={binary}\n")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
