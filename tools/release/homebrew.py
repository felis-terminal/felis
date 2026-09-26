#!/usr/bin/env python3
from __future__ import annotations

import base64
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
import urllib.parse
from pathlib import Path

USAGE = """usage: tools/release/homebrew.py <tag>
       tools/release/homebrew.py --self-test

Opens the pull request that moves the Homebrew formula in HOMEBREW_TAP to the
source archive of <tag>, as a draft when the packaging changed since the
previous release. Run from a full-history checkout. Reads HOMEBREW_TAP,
GITHUB_SERVER_URL, GITHUB_API_URL, GITHUB_REPOSITORY and GH_TOKEN.
"""

FORMULA = "Formula/felis.rb"
FINAL = re.compile(r"v(\d+)\.(\d+)\.(\d+)")
# What the formula's install block mirrors: a change here may need the formula
# changed too, which a green build does not prove.
PACKAGING = (
    "nix/package.nix",
    "nix/make-macos-app.sh",
    "nix/compile-terminfo.sh",
    "share",
)


class BumpError(Exception):
    pass


def note(message: str) -> None:
    print(f"release-homebrew: {message}", flush=True)


def git(*args: str) -> str:
    try:
        result = subprocess.run(
            ["git", *args], stdout=subprocess.PIPE, check=False, text=True
        )
    except OSError as error:
        raise BumpError(f"cannot run git: {error}") from error
    if result.returncode != 0:
        raise BumpError(f"git {' '.join(args)} exited {result.returncode}")
    return result.stdout


def curl(*args: str, config: Path | None = None, data: bytes | None = None) -> bytes:
    command = ["curl", "-sS", "-f", *(["-K", str(config)] if config else []), *args]
    try:
        result = subprocess.run(
            command, input=data, stdout=subprocess.PIPE, check=False
        )
    except OSError as error:
        raise BumpError(f"cannot run curl: {error}") from error
    if result.returncode != 0:
        raise BumpError(f"curl {' '.join(args)} exited {result.returncode}")
    return result.stdout


def fetch_status(url: str, body: Path, config: Path) -> str:
    command = [
        "curl",
        "-sS",
        "-K",
        str(config),
        "-o",
        str(body),
        "-w",
        "%{http_code}",
        url,
    ]
    try:
        result = subprocess.run(command, stdout=subprocess.PIPE, check=False)
    except OSError as error:
        raise BumpError(f"cannot run curl: {error}") from error
    if result.returncode != 0:
        raise BumpError(f"curl {url} exited {result.returncode}")
    return result.stdout.decode()


def version_of(tag: str) -> tuple[int, ...]:
    return tuple(int(part) for part in FINAL.fullmatch(tag).groups())  # type: ignore[union-attr]


def previous_release(tag: str) -> str | None:
    current = version_of(tag)
    earlier = [
        (version_of(name), name)
        for name in git("tag", "--list", "v*").split()
        if FINAL.fullmatch(name) and version_of(name) < current
    ]
    return max(earlier)[1] if earlier else None


def packaging_changes(previous: str | None, tag: str) -> str:
    if previous is None:
        return ""
    return git("diff", "--stat", previous, tag, "--", *PACKAGING).strip()


def bump_formula(text: str, url: str, sha256: str) -> str:
    text, urls = re.subn(r'(?m)^  url "[^"]*"$', f'  url "{url}"', text)
    text, sums = re.subn(r'(?m)^  sha256 "[0-9a-f]{64}"$', f'  sha256 "{sha256}"', text)
    if urls != 1 or sums != 1:
        raise BumpError(
            f"{FORMULA} carries {urls} top-level url and {sums} sha256 lines, not one each"
        )
    # A new version restarts the revision count; brew audit rejects a stale one.
    return re.sub(r"(?m)^  revision \d+\n", "", text)


def pull_request_body(
    tag: str, server: str, repository: str, previous: str | None, changes: str
) -> str:
    release = f"{server}/{repository}/releases/tag/{tag}"
    body = f"Moves the formula to [felis {tag[1:]}]({release}).\n"
    if not changes:
        return body
    return (
        body
        + f"\nThe packaging changed since {previous}, so this pull request is a draft and"
        " will not publish on its own. Check the formula's install block against:\n\n"
        f"```\n{changes}\n```\n\n"
        "Then mark it ready for review and either push the formula fix (test-bot"
        " publishes once it passes) or dispatch brew pr-pull for this pull request.\n"
    )


class Tap:
    def __init__(self, api: str, repository: str, token: str, scratch: Path) -> None:
        if any(c.isspace() or c in '"\\' for c in token):
            raise BumpError("GH_TOKEN carries whitespace or quoting")
        self.owner = repository.split("/", 1)[0]
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

    def branch_head(self, branch: str) -> str | None:
        body = self.scratch / "ref.json"
        url = f"{self.api}/git/ref/heads/{urllib.parse.quote(branch, safe='')}"
        code = fetch_status(url, body, self.config)
        if code == "404":
            return None
        if code != "200":
            raise BumpError(f"GET {url} answered HTTP {code}")
        return json.loads(body.read_bytes())["object"]["sha"]

    def formula(self, branch: str) -> tuple[str, str]:
        answer = self.json(
            "GET",
            f"{self.api}/contents/{FORMULA}?ref={urllib.parse.quote(branch, safe='')}",
        )
        return base64.b64decode(answer["content"]).decode("utf-8"), answer["sha"]


def bump(tap: Tap, tag: str, url: str, sha256: str, body: str, draft: bool) -> None:
    version = tag[1:]
    current, _ = tap.formula("main")
    shipped = re.search(r'(?m)^  url "[^"]*/tags/(v[^"/]+)\.tar\.gz"$', current)
    if (
        shipped
        and FINAL.fullmatch(shipped[1])
        # Equal counts too: the tap may have added a revision since.
        and version_of(shipped[1]) >= version_of(tag)
    ):
        note(f"the formula on main already builds {shipped[1]}")
        return
    wanted = bump_formula(current, url, sha256)
    if wanted == current:
        note(f"the formula on main already builds {tag}")
        return

    # Never felis-<version>: brew pr-pull names the bottle release tag that way,
    # and a branch sharing it makes plain git ref names ambiguous.
    branch = f"bump-felis-{version}"
    if tap.branch_head(branch) is None:
        main = tap.branch_head("main")
        if main is None:
            raise BumpError("the tap has no main branch")
        tap.json(
            "POST", f"{tap.api}/git/refs", {"ref": f"refs/heads/{branch}", "sha": main}
        )
        note(f"created {branch}")

    on_branch, blob = tap.formula(branch)
    if on_branch != bump_formula(on_branch, url, sha256):
        tap.json(
            "PUT",
            f"{tap.api}/contents/{FORMULA}",
            {
                "message": f"felis {version}",
                "content": base64.b64encode(
                    bump_formula(on_branch, url, sha256).encode("utf-8")
                ).decode("ascii"),
                "sha": blob,
                "branch": branch,
            },
        )
        note(f"committed felis {version} to {branch}")

    head = urllib.parse.quote(f"{tap.owner}:{branch}", safe="")
    if tap.json("GET", f"{tap.api}/pulls?state=open&head={head}"):
        note(f"a pull request from {branch} is already open")
        return
    created = tap.json(
        "POST",
        f"{tap.api}/pulls",
        {
            "title": f"felis {version}",
            "head": branch,
            "base": "main",
            "body": body,
            "draft": draft,
        },
    )
    note(f"opened {created['html_url']}" + (" as a draft" if draft else ""))


def main(argv: list[str]) -> int:
    if argv == ["--self-test"]:
        test = Path(__file__).with_name("homebrew_test.py")
        os.execv(sys.executable, [sys.executable, str(test)])
    if len(argv) != 1 or argv[0].startswith("-"):
        print(USAGE, file=sys.stderr, end="")
        return 64
    tag = argv[0]
    if not FINAL.fullmatch(tag):
        note(f"{tag} is not a final release; the formula stays where it is")
        return 0

    try:
        missing = [
            name
            for name in ("HOMEBREW_TAP", "GITHUB_REPOSITORY", "GH_TOKEN")
            if not os.environ.get(name)
        ]
        if missing:
            raise BumpError(f"the environment lacks {', '.join(missing)}")
        server = os.environ.get("GITHUB_SERVER_URL", "https://github.com").rstrip("/")
        repository = os.environ["GITHUB_REPOSITORY"]
        previous = previous_release(tag)
        changes = packaging_changes(previous, tag)
        url = f"{server}/{repository}/archive/refs/tags/{tag}.tar.gz"
        with tempfile.TemporaryDirectory(dir=os.environ.get("RUNNER_TEMP")) as scratch:
            directory = Path(scratch)
            archive = directory / "source.tar.gz"
            curl("-L", "-o", str(archive), url)
            sha256 = hashlib.sha256(archive.read_bytes()).hexdigest()
            tap = Tap(
                os.environ.get("GITHUB_API_URL", "https://api.github.com"),
                os.environ["HOMEBREW_TAP"],
                os.environ["GH_TOKEN"],
                directory,
            )
            body = pull_request_body(tag, server, repository, previous, changes)
            bump(tap, tag, url, sha256, body, draft=bool(changes))
    except (BumpError, OSError, UnicodeError) as error:
        print(f"release-homebrew: {error}", file=sys.stderr)
        return 1
    except (KeyError, TypeError, ValueError) as error:
        print(
            f"release-homebrew: GitHub answered an unexpected shape: {error!r}",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
