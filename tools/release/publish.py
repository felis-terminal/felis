#!/usr/bin/env python3
from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import urllib.parse
from pathlib import Path

USAGE = """usage: tools/release/publish.py release <tag> <notes-file> <asset>...
       tools/release/publish.py --self-test

Reads FORGEJO_SERVER_URL, GITHUB_REPOSITORY and RELEASE_TOKEN from the environment.
"""


class PublishError(Exception):
    pass


def note(message: str) -> None:
    print(f"release-publish: {message}")


def write_private(path: Path, text: str) -> Path:
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
        handle.write(text)
    return path


class Forge:
    def __init__(self, server: str, repository: str, token: str, scratch: Path) -> None:
        if any(c.isspace() or c in '"\\' for c in token):
            raise PublishError("RELEASE_TOKEN carries whitespace or quoting")
        self.server = server.rstrip("/")
        self.repository = repository
        self.token = token
        self.scratch = scratch
        self.api = f"{self.server}/api/v1/repos/{repository}"
        # A 0600 config file rather than -H: argv is readable from the process list.
        self.config = write_private(
            scratch / "curl.conf", f'header = "Authorization: token {token}"\n'
        )

    def curl(self, *args: str, fail: bool = True, data: bytes | None = None) -> bytes:
        command = [
            "curl",
            "-sS",
            "-K",
            str(self.config),
            *(["-f"] if fail else []),
            *args,
        ]
        try:
            result = subprocess.run(
                command, input=data, stdout=subprocess.PIPE, check=False
            )
        except OSError as error:
            raise PublishError(f"cannot run curl: {error}") from error
        if result.returncode != 0:
            raise PublishError(f"curl {' '.join(args)} exited {result.returncode}")
        return result.stdout

    def json(self, method: str, url: str, payload: object | None = None):  # type: ignore[no-untyped-def]
        args = ["-X", method, url]
        data = None
        if payload is not None:
            args += ["-H", "Content-Type: application/json", "--data-binary", "@-"]
            data = json.dumps(payload).encode("utf-8")
        answer = self.curl(*args, data=data)
        if not answer.strip():
            return None
        try:
            return json.loads(answer)
        except json.JSONDecodeError as error:
            raise PublishError(f"{method} {url} answered with no JSON") from error

    def lookup(self, url: str):  # type: ignore[no-untyped-def]
        body = self.scratch / "lookup.json"
        code = self.curl("-o", str(body), "-w", "%{http_code}", url, fail=False)
        if code == b"404":
            return None
        if code != b"200":
            raise PublishError(f"GET {url} answered HTTP {code.decode()}")
        return json.loads(body.read_bytes())

    def upload(self, release_id: int, asset: Path) -> None:
        name = urllib.parse.quote(asset.name, safe="")
        self.curl(
            "-X",
            "POST",
            f"{self.api}/releases/{release_id}/assets?name={name}",
            "-F",
            f"attachment=@{asset}",
            "-o",
            os.devnull,
        )


def forge_from_environment(scratch: Path) -> Forge:
    missing = [
        name
        for name in ("FORGEJO_SERVER_URL", "GITHUB_REPOSITORY", "RELEASE_TOKEN")
        if not os.environ.get(name)
    ]
    if missing:
        raise PublishError(f"the environment lacks {', '.join(missing)}")
    return Forge(
        os.environ["FORGEJO_SERVER_URL"],
        os.environ["GITHUB_REPOSITORY"],
        os.environ["RELEASE_TOKEN"],
        scratch,
    )


def publish_release(forge: Forge, tag: str, notes: Path, assets: list[Path]) -> None:
    absent = [str(asset) for asset in assets if not asset.is_file()]
    if absent:
        raise PublishError(f"no such asset: {', '.join(absent)}")
    names = [asset.name for asset in assets]
    if len(set(names)) != len(names):
        raise PublishError(f"two assets share a name: {names}")
    body = notes.read_text(encoding="utf-8")

    # Forgejo refuses a second release for the same tag, so a rerun deletes the
    # unfinished draft; a published release is never this run's to replace.
    existing = forge.lookup(f"{forge.api}/releases/tags/{tag}")
    if existing is not None:
        if not existing.get("draft"):
            raise PublishError(f"{tag} is already published; refusing to replace it")
        forge.curl(
            "-X", "DELETE", f"{forge.api}/releases/{existing['id']}", "-o", os.devnull
        )
        note(f"deleted the unfinished draft {existing['id']} for {tag}")

    # Draft first: a release page is visible the moment it exists, and one
    # whose assets arrived halfway claims a build nobody can install.
    created = forge.json(
        "POST",
        f"{forge.api}/releases",
        {
            "tag_name": tag,
            "name": tag,
            "body": body,
            "draft": True,
            "prerelease": "-" in tag,
        },
    )
    release_id = created["id"]
    note(f"created draft {release_id} for {tag}")
    for asset in assets:
        forge.upload(release_id, asset)
        note(f"attached {asset.name}")

    assembled = forge.json("GET", f"{forge.api}/releases/{release_id}")
    have = {asset["name"] for asset in assembled.get("assets") or []}
    missing = [name for name in names if name not in have]
    if missing:
        raise PublishError(f"draft is missing {missing}; not publishing")
    forge.json("PATCH", f"{forge.api}/releases/{release_id}", {"draft": False})
    note(f"published {tag}")


def main(argv: list[str]) -> int:
    if argv and argv[0] in {"-h", "--help"}:
        print(USAGE, file=sys.stderr, end="")
        return 64
    if argv == ["--self-test"]:
        test = Path(__file__).with_name("publish_test.py")
        os.execv(sys.executable, [sys.executable, str(test)])
    if not (len(argv) >= 4 and argv[0] == "release"):
        print(USAGE, file=sys.stderr, end="")
        return 64

    try:
        with tempfile.TemporaryDirectory(dir=os.environ.get("RUNNER_TEMP")) as scratch:
            forge = forge_from_environment(Path(scratch))
            assets = [Path(a) for a in argv[3:]]
            publish_release(forge, argv[1], Path(argv[2]), assets)
    except (PublishError, OSError, UnicodeError) as error:
        print(f"release-publish: {error}", file=sys.stderr)
        return 1
    except (KeyError, TypeError, ValueError) as error:
        print(
            f"release-publish: the forge answered an unexpected shape: {error!r}",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
