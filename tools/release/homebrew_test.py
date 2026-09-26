#!/usr/bin/env python3
"""Exit-code tests for tools/release/homebrew.py against a throwaway repo and a local fake GitHub."""

from __future__ import annotations

import base64
import hashlib
import http.server
import json
import os
import re
import subprocess
import sys
import tempfile
import threading
import urllib.parse
from pathlib import Path

BUMP = Path(__file__).with_name("homebrew.py")
REPOSITORY = "felis-terminal/felis"
TAP_API = "/repos/felis-terminal/homebrew-tap"
TOKEN = "fixture-token"
ARCHIVE = b"source archive bytes\n"
ARCHIVE_SHA = hashlib.sha256(ARCHIVE).hexdigest()
FORMULA = """class Felis < Formula
  desc "Terminal for your toolkit, not an environment"
  homepage "https://github.com/felis-terminal/felis"
  url "https://github.com/felis-terminal/felis/archive/refs/tags/v0.0.9.tar.gz"
  sha256 "1111111111111111111111111111111111111111111111111111111111111111"
  license "Apache-2.0"
  revision 1

  bottle do
    root_url "https://github.com/felis-terminal/homebrew-tap/releases/download/felis-0.0.9_1"
    sha256 cellar: :any, x86_64_linux: "2222222222222222222222222222222222222222222222222222222222222222"
  end
end
"""


def blob(text: str) -> str:
    return hashlib.sha1(text.encode("utf-8")).hexdigest()


class FakeGitHub:
    """The tag archive download and the tap's refs, contents and pulls API."""

    def __init__(self) -> None:
        self.branches: dict[str, str] = {}
        self.commits: list[tuple[str, str]] = []
        self.pulls: list[dict] = []
        self.archive_missing = False
        self.seen: list[tuple[str, str, str | None]] = []
        github = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.0"

            def log_message(self, *args: object) -> None:
                pass

            def reply(
                self, code: int, payload: object = None, raw: bytes = b""
            ) -> None:
                body = raw if payload is None else json.dumps(payload).encode("utf-8")
                self.send_response(code)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def body(self) -> dict:
                return json.loads(
                    self.rfile.read(int(self.headers.get("Content-Length", "0")))
                )

            def handle_any(self) -> None:
                github.seen.append(
                    (self.command, self.path, self.headers.get("Authorization"))
                )
                with github.lock:
                    github.route(self)

            do_GET = do_POST = do_PUT = handle_any

        self.lock = threading.Lock()
        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.origin = "http://127.0.0.1:%d" % self.server.server_address[1]
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)

    def route(self, h) -> None:  # type: ignore[no-untyped-def]
        url = urllib.parse.urlsplit(h.path)
        query = dict(urllib.parse.parse_qsl(url.query))
        path = url.path

        if match := re.fullmatch(
            rf"/{REPOSITORY}/archive/refs/tags/(.+)\.tar\.gz", path
        ):
            if self.archive_missing:
                h.reply(404, raw=b"Not Found")
            else:
                h.reply(200, raw=ARCHIVE)
            return
        if not path.startswith(TAP_API):
            h.reply(404, {"message": "no route"})
            return
        path = path[len(TAP_API) :]

        if match := re.fullmatch(r"/git/ref/heads/(.+)", path):
            branch = urllib.parse.unquote(match[1])
            if branch in self.branches:
                h.reply(200, {"object": {"sha": blob(self.branches[branch])}})
            else:
                h.reply(404, {"message": "Not Found"})
            return
        if path == "/git/refs" and h.command == "POST":
            payload = h.body()
            branch = payload["ref"].removeprefix("refs/heads/")
            self.branches[branch] = self.branches["main"]
            h.reply(201, {"ref": payload["ref"]})
            return
        if path == "/contents/Formula/felis.rb" and h.command == "GET":
            text = self.branches.get(query.get("ref", ""))
            if text is None:
                h.reply(404, {"message": "Not Found"})
                return
            content = base64.encodebytes(text.encode("utf-8")).decode("ascii")
            h.reply(200, {"content": content, "sha": blob(text)})
            return
        if path == "/contents/Formula/felis.rb" and h.command == "PUT":
            payload = h.body()
            branch = payload["branch"]
            if payload["sha"] != blob(self.branches[branch]):
                h.reply(409, {"message": "sha does not match"})
                return
            self.branches[branch] = base64.b64decode(payload["content"]).decode("utf-8")
            self.commits.append((branch, payload["message"]))
            h.reply(200, {"content": {"sha": blob(self.branches[branch])}})
            return
        if path == "/pulls" and h.command == "GET":
            owner, _, branch = query.get("head", "").partition(":")
            h.reply(
                200,
                [
                    p
                    for p in self.pulls
                    if owner == "felis-terminal"
                    and p["head"] == branch
                    and p["state"] == "open"
                ],
            )
            return
        if path == "/pulls" and h.command == "POST":
            payload = h.body()
            payload.update(
                {
                    "state": "open",
                    "html_url": f"https://github.test/pull/{len(self.pulls) + 1}",
                }
            )
            self.pulls.append(payload)
            h.reply(201, payload)
            return
        h.reply(404, {"message": "no route"})


class Suite:
    def __init__(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.failures = 0
        self.cases = 0
        self.github = FakeGitHub()
        self.out = ""

    def close(self) -> None:
        self.github.close()
        self.temporary.cleanup()

    def git(self, *args: str) -> None:
        subprocess.run(
            ["git", "-C", str(self.repo), *args],
            check=True,
            stdout=subprocess.DEVNULL,
        )

    def commit(self, path: str, text: str, *tags: str) -> None:
        file = self.repo / path
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text(text)
        self.git("add", path)
        self.git("commit", "-q", "-m", f"change {path}")
        for tag in tags:
            self.git("tag", "-m", tag, tag)

    def build_history(self) -> None:
        self.git("init", "-q")
        self.git("config", "user.email", "release@example.invalid")
        self.git("config", "user.name", "release test")
        self.git("config", "commit.gpgSign", "false")
        self.git("config", "tag.gpgSign", "false")
        self.commit("nix/package.nix", "{ }\n", "v0.1.0")
        self.commit("crates/core.rs", "fn a() {}\n", "v0.1.1")
        self.commit(
            "share/applications/felis.desktop", "[Desktop Entry]\n", "v0.2.0-rc.1"
        )
        self.commit("crates/core.rs", "fn b() {}\n", "v0.2.0")

    def reset(self, formula: str = FORMULA) -> None:
        self.github.branches = {"main": formula}
        self.github.commits.clear()
        self.github.pulls.clear()
        self.github.archive_missing = False
        self.github.seen.clear()

    def record(self, passed: bool, name: str) -> None:
        self.cases += 1
        if passed:
            print(f"  ok   {name}")
            return
        self.failures += 1
        print(f"  FAIL {name}")
        for line in self.out.splitlines():
            print(f"       | {line}")

    def run_bump(self, *args: str, environment: dict[str, str] | None = None) -> int:
        env = os.environ.copy()
        env.update(
            {
                "HOMEBREW_TAP": "felis-terminal/homebrew-tap",
                "GITHUB_SERVER_URL": self.github.origin,
                "GITHUB_API_URL": self.github.origin,
                "GITHUB_REPOSITORY": REPOSITORY,
                "GH_TOKEN": TOKEN,
                "RUNNER_TEMP": str(self.root),
            }
        )
        env.update(environment or {})
        result = subprocess.run(
            [sys.executable, str(BUMP), *args],
            cwd=self.repo,
            env=env,
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        self.out = result.stdout
        return result.returncode

    def bumped(self, branch: str, tag: str) -> bool:
        text = self.github.branches.get(branch, "")
        url = f"{self.github.origin}/{REPOSITORY}/archive/refs/tags/{tag}.tar.gz"
        return (
            f'  url "{url}"\n' in text
            and f'  sha256 "{ARCHIVE_SHA}"\n' in text
            and "revision" not in text
            and "bottle do" in text
        )

    def cases_run(self) -> None:
        self.build_history()

        self.reset()
        code = self.run_bump("v0.1.1")
        pulls = self.github.pulls
        self.record(
            code == 0
            and self.bumped("bump-felis-0.1.1", "v0.1.1")
            and self.github.commits == [("bump-felis-0.1.1", "felis 0.1.1")]
            and len(pulls) == 1
            and pulls[0]["title"] == "felis 0.1.1"
            and pulls[0]["base"] == "main"
            and pulls[0]["draft"] is False
            and self.github.branches["main"] == FORMULA,
            f"a release without packaging changes opens a ready pull request (exit {code})",
        )
        self.record(
            all(
                auth == f"Bearer {TOKEN}"
                for _, path, auth in self.github.seen
                if path.startswith(TAP_API)
            )
            and all(TOKEN not in line for line in self.out.splitlines()),
            "the token reaches the tap API and never the log",
        )

        code = self.run_bump("v0.1.1")
        self.record(
            code == 0 and len(self.github.pulls) == 1 and len(self.github.commits) == 1,
            f"a rerun finds the open pull request and changes nothing (exit {code})",
        )

        self.reset()
        code = self.run_bump("v0.2.0")
        pulls = self.github.pulls
        self.record(
            code == 0
            and self.bumped("bump-felis-0.2.0", "v0.2.0")
            and len(pulls) == 1
            and pulls[0]["draft"] is True
            and "v0.1.1" in pulls[0]["body"]
            and "share/applications/felis.desktop" in pulls[0]["body"],
            f"a packaging change since the previous final release opens a draft (exit {code})",
        )

        self.reset()
        self.github.branches["bump-felis-0.1.1"] = FORMULA
        code = self.run_bump("v0.1.1")
        self.record(
            code == 0
            and self.bumped("bump-felis-0.1.1", "v0.1.1")
            and len(self.github.pulls) == 1,
            f"a branch left by a failed run is finished, not duplicated (exit {code})",
        )

        self.reset()
        code = self.run_bump("v0.1.0")
        self.record(
            code == 0
            and len(self.github.pulls) == 1
            and self.github.pulls[0]["draft"] is False,
            f"the first release has nothing to compare against and opens a ready pull request (exit {code})",
        )

        self.reset()
        current = f"{self.github.origin}/{REPOSITORY}/archive/refs/tags/v0.1.1.tar.gz"
        on_main = re.sub(r'(?m)^  url "[^"]*"$', f'  url "{current}"', FORMULA)
        on_main = re.sub(
            r'(?m)^  sha256 "[0-9a-f]{64}"$', f'  sha256 "{ARCHIVE_SHA}"', on_main
        )
        on_main = on_main.replace("  revision 1\n", "")
        self.reset(on_main)
        code = self.run_bump("v0.1.1")
        self.record(
            code == 0
            and not self.github.pulls
            and list(self.github.branches) == ["main"],
            f"a formula already on the release is left alone (exit {code})",
        )

        self.reset(on_main)
        code = self.run_bump("v0.1.0")
        self.record(
            code == 0
            and not self.github.pulls
            and list(self.github.branches) == ["main"],
            f"rerunning an older release never moves the formula back (exit {code})",
        )

        self.reset(
            on_main.replace(
                '  license "Apache-2.0"\n', '  license "Apache-2.0"\n  revision 2\n'
            )
        )
        code = self.run_bump("v0.1.1")
        self.record(
            code == 0
            and not self.github.pulls
            and list(self.github.branches) == ["main"],
            f"rerunning the release keeps a revision the tap added since (exit {code})",
        )

        self.reset()
        code = self.run_bump("v0.2.0-rc.1")
        self.record(
            code == 0 and not self.github.seen,
            f"a prerelease never reaches GitHub (exit {code})",
        )

        self.reset()
        self.github.archive_missing = True
        code = self.run_bump("v0.1.1")
        self.record(
            code == 1
            and not self.github.pulls
            and list(self.github.branches) == ["main"],
            f"a missing source archive fails before the tap is touched (exit {code})",
        )

        self.reset(FORMULA.replace('  url "', '  # url "'))
        code = self.run_bump("v0.1.1")
        self.record(
            code == 1
            and not self.github.pulls
            and list(self.github.branches) == ["main"],
            f"a formula without one top-level url line is refused (exit {code})",
        )

        self.reset()
        code = self.run_bump("v0.1.1", environment={"GH_TOKEN": ""})
        self.record(
            code == 1 and not self.github.seen,
            f"a missing token fails (exit {code})",
        )

        code = self.run_bump()
        self.record(code == 64, f"no tag prints usage (exit {code})")

    def run(self) -> int:
        self.cases_run()
        if self.failures == 0:
            print(f"release-homebrew self-test: all {self.cases} cases passed")
            return 0
        print(
            f"release-homebrew self-test: {self.failures} of {self.cases} case(s) failed",
            file=sys.stderr,
        )
        return 1


def main() -> int:
    suite = Suite()
    try:
        return suite.run()
    finally:
        suite.close()


if __name__ == "__main__":
    raise SystemExit(main())
