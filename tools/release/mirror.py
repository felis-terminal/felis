#!/usr/bin/env python3
from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import time
import urllib.parse
from pathlib import Path

USAGE = """usage: tools/release/mirror.py <tag>
       tools/release/mirror.py --self-test

Waits for the Forgejo release of <tag> under MIRROR_SOURCE (a repository API
base) and republishes its notes and assets on GitHub. Reads MIRROR_SOURCE,
GITHUB_API_URL, GITHUB_REPOSITORY and GH_TOKEN, plus MIRROR_TIMEOUT and
MIRROR_INTERVAL in seconds.
"""


class MirrorError(Exception):
    pass


def note(message: str) -> None:
    print(f"release-mirror: {message}", flush=True)


def curl(*args: str, config: Path | None = None, data: bytes | None = None) -> bytes:
    command = ["curl", "-sS", "-f", *(["-K", str(config)] if config else []), *args]
    try:
        result = subprocess.run(
            command, input=data, stdout=subprocess.PIPE, check=False
        )
    except OSError as error:
        raise MirrorError(f"cannot run curl: {error}") from error
    if result.returncode != 0:
        raise MirrorError(f"curl {' '.join(args)} exited {result.returncode}")
    return result.stdout


def fetch_status(url: str, body: Path, config: Path | None = None) -> str:
    command = [
        "curl",
        "-sS",
        *(["-K", str(config)] if config else []),
        "-o",
        str(body),
        "-w",
        "%{http_code}",
        url,
    ]
    try:
        result = subprocess.run(command, stdout=subprocess.PIPE, check=False)
    except OSError as error:
        raise MirrorError(f"cannot run curl: {error}") from error
    if result.returncode != 0:
        raise MirrorError(f"curl {url} exited {result.returncode}")
    return result.stdout.decode()


def wait_for_source(
    source: str, tag: str, scratch: Path, timeout: float, interval: float
) -> dict:
    url = f"{source}/releases/tags/{urllib.parse.quote(tag, safe='')}"
    body = scratch / "source.json"
    deadline = time.monotonic() + timeout
    while True:
        code = fetch_status(url, body)
        if code == "200":
            release = json.loads(body.read_bytes())
            if not release.get("draft") and release.get("assets"):
                return release
        elif code != "404":
            note(f"GET {url} answered HTTP {code}; retrying")
        if time.monotonic() >= deadline:
            raise MirrorError(
                f"no published Forgejo release for {tag} after {timeout:.0f}s"
            )
        time.sleep(interval)


def download(release: dict, scratch: Path) -> list[Path]:
    directory = scratch / "assets"
    directory.mkdir()
    paths = []
    for asset in release["assets"]:
        name = asset["name"]
        if not name or name != Path(name).name or name.startswith("."):
            raise MirrorError(f"refusing the asset name {name!r}")
        path = directory / name
        curl("-L", "-o", str(path), asset["browser_download_url"])
        if path.stat().st_size != asset["size"]:
            raise MirrorError(
                f"{name} arrived with {path.stat().st_size} bytes, "
                f"Forgejo lists {asset['size']}"
            )
        paths.append(path)
    return paths


class GitHub:
    def __init__(self, api: str, repository: str, token: str, scratch: Path) -> None:
        if any(c.isspace() or c in '"\\' for c in token):
            raise MirrorError("GH_TOKEN carries whitespace or quoting")
        self.api = f"{api.rstrip('/')}/repos/{repository}"
        self.scratch = scratch
        # A 0600 config file rather than -H: argv is readable from the process list.
        descriptor = os.open(
            scratch / "github.conf", os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600
        )
        with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
            handle.write(
                f'header = "Authorization: Bearer {token}"\n'
                'header = "Accept: application/vnd.github+json"\n'
                'header = "X-GitHub-Api-Version: 2022-11-28"\n'
            )
        self.config = scratch / "github.conf"

    def json(self, method: str, url: str, payload: object | None = None):  # type: ignore[no-untyped-def]
        args = ["-X", method, url]
        data = None
        if payload is not None:
            args += ["-H", "Content-Type: application/json", "--data-binary", "@-"]
            data = json.dumps(payload).encode("utf-8")
        answer = curl(*args, config=self.config, data=data)
        return json.loads(answer) if answer.strip() else None

    def lookup(self, tag: str):  # type: ignore[no-untyped-def]
        body = self.scratch / "lookup.json"
        url = f"{self.api}/releases/tags/{urllib.parse.quote(tag, safe='')}"
        code = fetch_status(url, body, self.config)
        if code == "404":
            return None
        if code != "200":
            raise MirrorError(f"GET {url} answered HTTP {code}")
        return json.loads(body.read_bytes())

    def upload(self, release: dict, asset: Path) -> None:
        base = release["upload_url"].split("{", 1)[0]
        name = urllib.parse.quote(asset.name, safe="")
        curl(
            "-X",
            "POST",
            f"{base}?name={name}",
            "-H",
            "Content-Type: application/octet-stream",
            "--data-binary",
            f"@{asset}",
            "-o",
            os.devnull,
            config=self.config,
        )


def mirror(github: GitHub, source: dict, assets: list[Path]) -> None:
    tag = source["tag_name"]
    names = {asset.name for asset in assets}
    existing = github.lookup(tag)
    if existing is not None:
        have = {asset["name"] for asset in existing.get("assets") or []}
        if names <= have:
            note(f"GitHub already publishes {tag} with every asset")
            return
        raise MirrorError(
            f"GitHub publishes {tag} without {sorted(names - have)}; refusing to replace it"
        )

    # The tag lookup does not see drafts, and GitHub accepts a second draft for
    # the same tag, so a rerun would otherwise leave the first one behind.
    for release in github.json("GET", f"{github.api}/releases?per_page=100") or []:
        if release.get("draft") and release.get("tag_name") == tag:
            github.json("DELETE", f"{github.api}/releases/{release['id']}")
            note(f"deleted the unfinished draft {release['id']} for {tag}")

    created = github.json(
        "POST",
        f"{github.api}/releases",
        {
            "tag_name": tag,
            "name": source.get("name") or tag,
            "body": source.get("body") or "",
            "draft": True,
            "prerelease": bool(source.get("prerelease")),
            # Semver order rather than creation order: rerunning an old tag
            # must not mark it latest over a newer release.
            "make_latest": "legacy",
        },
    )
    note(f"created draft {created['id']} for {tag}")
    for asset in assets:
        github.upload(created, asset)
        note(f"attached {asset.name}")

    assembled = github.json("GET", f"{github.api}/releases/{created['id']}")
    have = {asset["name"] for asset in assembled.get("assets") or []}
    if not names <= have:
        raise MirrorError(f"draft is missing {sorted(names - have)}; not publishing")
    github.json("PATCH", f"{github.api}/releases/{created['id']}", {"draft": False})
    note(f"published {tag}")


def main(argv: list[str]) -> int:
    if argv == ["--self-test"]:
        test = Path(__file__).with_name("mirror_test.py")
        os.execv(sys.executable, [sys.executable, str(test)])
    if len(argv) != 1 or argv[0].startswith("-"):
        print(USAGE, file=sys.stderr, end="")
        return 64
    tag = argv[0]

    try:
        missing = [
            name
            for name in ("MIRROR_SOURCE", "GITHUB_REPOSITORY", "GH_TOKEN")
            if not os.environ.get(name)
        ]
        if missing:
            raise MirrorError(f"the environment lacks {', '.join(missing)}")
        timeout = float(os.environ.get("MIRROR_TIMEOUT", "18000"))
        interval = float(os.environ.get("MIRROR_INTERVAL", "60"))
        with tempfile.TemporaryDirectory(dir=os.environ.get("RUNNER_TEMP")) as scratch:
            directory = Path(scratch)
            source = wait_for_source(
                os.environ["MIRROR_SOURCE"].rstrip("/"),
                tag,
                directory,
                timeout,
                interval,
            )
            if source.get("tag_name") != tag:
                raise MirrorError(
                    f"Forgejo answered {source.get('tag_name')!r} for {tag}"
                )
            assets = download(source, directory)
            github = GitHub(
                os.environ.get("GITHUB_API_URL", "https://api.github.com"),
                os.environ["GITHUB_REPOSITORY"],
                os.environ["GH_TOKEN"],
                directory,
            )
            mirror(github, source, assets)
    except (MirrorError, OSError, UnicodeError) as error:
        print(f"release-mirror: {error}", file=sys.stderr)
        return 1
    except (KeyError, TypeError, ValueError) as error:
        print(
            f"release-mirror: a forge answered an unexpected shape: {error!r}",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
