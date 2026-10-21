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
import threading
import urllib.parse
from pathlib import Path

import pytest

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
        # shutdown() waits out one poll interval, 0.5 s by default, on every teardown.
        self.thread = threading.Thread(
            target=self.server.serve_forever, args=(0.01,), daemon=True
        )
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


@pytest.fixture(scope="module")
def repo(tmp_path_factory: pytest.TempPathFactory) -> Path:
    repo = tmp_path_factory.mktemp("repo")

    def git(*args: str) -> None:
        subprocess.run(
            ["git", "-C", str(repo), *args], check=True, stdout=subprocess.DEVNULL
        )

    def commit(path: str, text: str, tag: str) -> None:
        file = repo / path
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text(text)
        git("add", path)
        git("commit", "-q", "-m", f"change {path}")
        git("tag", "-m", tag, tag)

    git("init", "-q")
    git("config", "user.email", "release@example.invalid")
    git("config", "user.name", "release test")
    git("config", "commit.gpgSign", "false")
    git("config", "tag.gpgSign", "false")
    commit("nix/package.nix", "{ }\n", "v0.1.0")
    commit("crates/core.rs", "fn a() {}\n", "v0.1.1")
    commit("share/applications/felis.desktop", "[Desktop Entry]\n", "v0.2.0-rc.1")
    commit("crates/core.rs", "fn b() {}\n", "v0.2.0")
    commit("crates/felis-daemon/terminfo/felis.terminfo", "xterm-felis|\n", "v0.2.1")
    return repo


@pytest.fixture
def github():
    github = FakeGitHub()
    github.branches = {"main": FORMULA}
    yield github
    github.close()


@pytest.fixture
def run_bump(github: FakeGitHub, repo: Path, tmp_path: Path):
    def run(
        *args: str, environment: dict[str, str] | None = None
    ) -> subprocess.CompletedProcess[str]:
        env = os.environ.copy()
        env.update(
            {
                "HOMEBREW_TAP": "felis-terminal/homebrew-tap",
                "GITHUB_SERVER_URL": github.origin,
                "GITHUB_API_URL": github.origin,
                "GITHUB_REPOSITORY": REPOSITORY,
                "GH_TOKEN": TOKEN,
                "RUNNER_TEMP": str(tmp_path),
            }
        )
        env.update(environment or {})
        return subprocess.run(
            [sys.executable, str(BUMP), *args],
            cwd=repo,
            env=env,
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )

    return run


def context(result: subprocess.CompletedProcess[str], github: FakeGitHub) -> str:
    seen = "\n".join(f"  {method} {path}" for method, path, _ in github.seen)
    return (
        f"exit {result.returncode}\n{result.stdout}\nrequests:\n{seen}\n"
        f"pulls: {github.pulls}\nbranches: {list(github.branches)}"
    )


def assert_bumped(github: FakeGitHub, branch: str, tag: str) -> None:
    text = github.branches.get(branch, "")
    url = f"{github.origin}/{REPOSITORY}/archive/refs/tags/{tag}.tar.gz"
    assert f'  url "{url}"\n' in text, text
    assert f'  sha256 "{ARCHIVE_SHA}"\n' in text, text
    assert "revision" not in text, text
    assert "bottle do" in text, text


def formula_on_release(github: FakeGitHub, tag: str) -> str:
    current = f"{github.origin}/{REPOSITORY}/archive/refs/tags/{tag}.tar.gz"
    text = re.sub(r'(?m)^  url "[^"]*"$', f'  url "{current}"', FORMULA)
    text = re.sub(r'(?m)^  sha256 "[0-9a-f]{64}"$', f'  sha256 "{ARCHIVE_SHA}"', text)
    return text.replace("  revision 1\n", "")


def assert_tap_untouched(
    github: FakeGitHub, result: subprocess.CompletedProcess[str]
) -> None:
    assert not github.pulls, context(result, github)
    assert list(github.branches) == ["main"], context(result, github)


def test_a_release_without_packaging_changes_opens_a_ready_pull_request(
    github, run_bump
):
    result = run_bump("v0.1.1")
    assert result.returncode == 0, context(result, github)
    assert_bumped(github, "bump-felis-0.1.1", "v0.1.1")
    assert github.commits == [("bump-felis-0.1.1", "felis 0.1.1")]
    assert len(github.pulls) == 1, context(result, github)
    pull = github.pulls[0]
    assert pull["title"] == "felis 0.1.1"
    assert pull["base"] == "main"
    assert pull["draft"] is False
    assert github.branches["main"] == FORMULA


def test_the_token_reaches_the_tap_api_and_never_the_log(github, run_bump):
    result = run_bump("v0.1.1")
    assert result.returncode == 0, context(result, github)
    assert all(
        auth == f"Bearer {TOKEN}"
        for _, path, auth in github.seen
        if path.startswith(TAP_API)
    ), github.seen
    assert TOKEN not in result.stdout, result.stdout


def test_a_rerun_finds_the_open_pull_request_and_changes_nothing(github, run_bump):
    first = run_bump("v0.1.1")
    assert first.returncode == 0, context(first, github)
    result = run_bump("v0.1.1")
    assert result.returncode == 0, context(result, github)
    assert len(github.pulls) == 1, context(result, github)
    assert len(github.commits) == 1, context(result, github)


def test_a_packaging_change_since_the_previous_final_release_opens_a_draft(
    github, run_bump
):
    result = run_bump("v0.2.0")
    assert result.returncode == 0, context(result, github)
    assert_bumped(github, "bump-felis-0.2.0", "v0.2.0")
    assert len(github.pulls) == 1, context(result, github)
    pull = github.pulls[0]
    assert pull["draft"] is True
    assert "v0.1.1" in pull["body"], pull["body"]
    assert "share/applications/felis.desktop" in pull["body"], pull["body"]


def test_a_change_to_the_terminfo_source_the_formula_compiles_opens_a_draft(
    github, run_bump
):
    result = run_bump("v0.2.1")
    assert result.returncode == 0, context(result, github)
    assert len(github.pulls) == 1, context(result, github)
    pull = github.pulls[0]
    assert pull["draft"] is True
    assert "crates/felis-daemon/terminfo/felis.terminfo" in pull["body"], pull["body"]


def test_a_branch_left_by_a_failed_run_is_finished_not_duplicated(github, run_bump):
    github.branches["bump-felis-0.1.1"] = FORMULA
    result = run_bump("v0.1.1")
    assert result.returncode == 0, context(result, github)
    assert_bumped(github, "bump-felis-0.1.1", "v0.1.1")
    assert len(github.pulls) == 1, context(result, github)


def test_the_first_release_has_nothing_to_compare_against_and_opens_a_ready_pull_request(
    github, run_bump
):
    result = run_bump("v0.1.0")
    assert result.returncode == 0, context(result, github)
    assert len(github.pulls) == 1, context(result, github)
    assert github.pulls[0]["draft"] is False


def test_a_formula_already_on_the_release_is_left_alone(github, run_bump):
    github.branches = {"main": formula_on_release(github, "v0.1.1")}
    result = run_bump("v0.1.1")
    assert result.returncode == 0, context(result, github)
    assert_tap_untouched(github, result)


def test_rerunning_an_older_release_never_moves_the_formula_back(github, run_bump):
    github.branches = {"main": formula_on_release(github, "v0.1.1")}
    result = run_bump("v0.1.0")
    assert result.returncode == 0, context(result, github)
    assert_tap_untouched(github, result)


def test_rerunning_the_release_keeps_a_revision_the_tap_added_since(github, run_bump):
    github.branches = {
        "main": formula_on_release(github, "v0.1.1").replace(
            '  license "Apache-2.0"\n', '  license "Apache-2.0"\n  revision 2\n'
        )
    }
    result = run_bump("v0.1.1")
    assert result.returncode == 0, context(result, github)
    assert_tap_untouched(github, result)


def test_a_prerelease_never_reaches_github(github, run_bump):
    result = run_bump("v0.2.0-rc.1")
    assert result.returncode == 0, context(result, github)
    assert not github.seen, context(result, github)


def test_a_missing_source_archive_fails_before_the_tap_is_touched(github, run_bump):
    github.archive_missing = True
    result = run_bump("v0.1.1")
    assert result.returncode == 1, context(result, github)
    assert_tap_untouched(github, result)


def test_a_formula_without_one_top_level_url_line_is_refused(github, run_bump):
    github.branches = {"main": FORMULA.replace('  url "', '  # url "')}
    result = run_bump("v0.1.1")
    assert result.returncode == 1, context(result, github)
    assert_tap_untouched(github, result)


def test_a_missing_token_fails(github, run_bump):
    result = run_bump("v0.1.1", environment={"GH_TOKEN": ""})
    assert result.returncode == 1, context(result, github)
    assert not github.seen, context(result, github)


def test_no_tag_prints_usage(github, run_bump):
    result = run_bump()
    assert result.returncode == 64, context(result, github)
