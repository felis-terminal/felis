"""Exit-code tests for tools/release/mirror.py against a local fake Forgejo and GitHub."""

from __future__ import annotations

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

MIRROR = Path(__file__).with_name("mirror.py")
SOURCE_API = "/api/v1/repos/felis-terminal/felis"
GITHUB_API = "/repos/felis-terminal/felis"
TOKEN = "fixture-token"
TAG = "v0.1.0"
ASSETS = {
    "felis-x86_64-linux.tar.gz": b"linux bytes\r\n",
    "felis.proto": b'syntax = "proto3";\n',
}


class FakeForges:
    """The Forgejo release read and the GitHub release API the script calls."""

    def __init__(self) -> None:
        self.source: dict | None = None
        self.source_404s = 0
        self.files: dict[str, bytes] = {}
        self.short_file = ""
        self.releases: dict[int, dict] = {}
        self.uploaded: dict[tuple[int, str], bytes] = {}
        self.fail_upload = ""
        self.seen: list[tuple[str, str, str | None]] = []
        self.next_id = 1
        forges = self

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

            def body(self) -> bytes:
                return self.rfile.read(int(self.headers.get("Content-Length", "0")))

            def handle_any(self) -> None:
                forges.seen.append(
                    (self.command, self.path, self.headers.get("Authorization"))
                )
                with forges.lock:
                    forges.route(self)

            do_GET = do_POST = do_PATCH = do_DELETE = handle_any

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

    def publish_source(self, *, prerelease: bool = False, draft: bool = False) -> None:
        assets = []
        for name, content in ASSETS.items():
            path = f"/download/{name}"
            self.files[path] = content
            assets.append(
                {
                    "name": name,
                    "size": len(content),
                    "browser_download_url": f"{self.origin}{path}",
                }
            )
        self.source = {
            "tag_name": TAG,
            "name": TAG,
            "body": "felis notes\n",
            "draft": draft,
            "prerelease": prerelease,
            "assets": assets,
        }

    def add_release(self, *, draft: bool, names: list[str]) -> dict:
        release = {
            "id": self.next_id,
            "tag_name": TAG,
            "draft": draft,
            "assets": [{"name": name} for name in names],
            "upload_url": f"{self.origin}/uploads{GITHUB_API}/releases/{self.next_id}/assets{{?name,label}}",
        }
        self.releases[self.next_id] = release
        self.next_id += 1
        return release

    def route(self, h) -> None:  # type: ignore[no-untyped-def]
        url = urllib.parse.urlsplit(h.path)
        query = dict(urllib.parse.parse_qsl(url.query))
        path = url.path

        if path == f"{SOURCE_API}/releases/tags/{TAG}":
            if self.source_404s > 0 or self.source is None:
                self.source_404s -= 1
                h.reply(404, {"message": "no"})
                return
            h.reply(200, self.source)
            return
        if h.command == "GET" and path.startswith("/download/"):
            # Forgejo serves an attachment through a redirect.
            h.send_response(302)
            h.send_header("Location", path.replace("/download/", "/files/", 1))
            h.send_header("Content-Length", "0")
            h.end_headers()
            return
        if h.command == "GET" and path.startswith("/files/"):
            name = path[len("/files/") :]
            content = self.files[f"/download/{name}"]
            h.reply(200, raw=content[:-1] if name == self.short_file else content)
            return

        if match := re.fullmatch(
            rf"/uploads{GITHUB_API}/releases/([0-9]+)/assets", path
        ):
            release = self.releases.get(int(match[1]))
            name = query.get("name", "")
            data = h.body()
            if release is None:
                h.reply(404, {"message": "no"})
                return
            if name == self.fail_upload:
                h.reply(500, {"message": "fixture fails this upload"})
                return
            self.uploaded[(release["id"], name)] = data
            release["assets"].append({"name": name})
            h.reply(201, {"name": name})
            return
        if not path.startswith(GITHUB_API):
            h.reply(404, {"message": "no route"})
            return
        path = path[len(GITHUB_API) :]
        if path == f"/releases/tags/{TAG}":
            found = [r for r in self.releases.values() if not r["draft"]]
            h.reply(200, found[0]) if found else h.reply(404, {"message": "no"})
            return
        if path == "/releases" and h.command == "GET":
            h.reply(200, list(self.releases.values()))
            return
        if path == "/releases" and h.command == "POST":
            payload = json.loads(h.body())
            release = self.add_release(draft=payload["draft"], names=[])
            release.update(
                {k: payload[k] for k in ("name", "body", "prerelease", "make_latest")}
            )
            h.reply(201, release)
            return
        if match := re.fullmatch(r"/releases/([0-9]+)", path):
            number = int(match[1])
            if number not in self.releases:
                h.reply(404, {"message": "no"})
                return
            if h.command == "DELETE":
                del self.releases[number]
                h.reply(204, raw=b"")
                return
            if h.command == "PATCH":
                self.releases[number].update(json.loads(h.body()))
            h.reply(200, self.releases[number])
            return
        h.reply(404, {"message": "no route"})


@pytest.fixture
def forges():
    forges = FakeForges()
    yield forges
    forges.close()


@pytest.fixture
def run_mirror(forges: FakeForges, tmp_path: Path):
    def run(
        *args: str, environment: dict[str, str] | None = None
    ) -> subprocess.CompletedProcess[str]:
        env = os.environ.copy()
        env.update(
            {
                "MIRROR_SOURCE": f"{forges.origin}{SOURCE_API}",
                "GITHUB_API_URL": forges.origin,
                "GITHUB_REPOSITORY": "felis-terminal/felis",
                "GH_TOKEN": TOKEN,
                "MIRROR_TIMEOUT": "5",
                "MIRROR_INTERVAL": "0",
                "RUNNER_TEMP": str(tmp_path),
            }
        )
        env.update(environment or {})
        return subprocess.run(
            [sys.executable, str(MIRROR), *args],
            env=env,
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )

    return run


def context(result: subprocess.CompletedProcess[str], forges: FakeForges) -> str:
    seen = "\n".join(f"  {method} {path}" for method, path, _ in forges.seen)
    return f"exit {result.returncode}\n{result.stdout}\nrequests:\n{seen}"


def published(forges: FakeForges) -> list[dict]:
    return [r for r in forges.releases.values() if not r["draft"]]


def assert_mirrored_intact(
    forges: FakeForges, result: subprocess.CompletedProcess[str], *, prerelease: bool
) -> None:
    assert result.returncode == 0, context(result, forges)
    releases = published(forges)
    assert len(releases) == 1 and len(forges.releases) == 1, context(result, forges)
    release = releases[0]
    assert release["body"] == "felis notes\n"
    assert release["prerelease"] is prerelease
    assert release["make_latest"] == "legacy"
    uploaded = {
        name: data
        for (number, name), data in forges.uploaded.items()
        if number == release["id"]
    }
    assert uploaded == ASSETS


def test_a_published_forgejo_release_is_mirrored_byte_for_byte(forges, run_mirror):
    forges.publish_source()
    result = run_mirror(TAG)
    assert_mirrored_intact(forges, result, prerelease=False)


def test_the_github_token_reaches_github_and_never_forgejo(forges, run_mirror):
    forges.publish_source()
    result = run_mirror(TAG)
    assert result.returncode == 0, context(result, forges)
    forgejo_auth = [
        auth
        for _, path, auth in forges.seen
        if path.startswith(SOURCE_API) or path.startswith(("/download/", "/files/"))
    ]
    github_auth = [
        auth
        for _, path, auth in forges.seen
        if path.startswith((GITHUB_API, "/uploads/"))
    ]
    assert forgejo_auth and not any(forgejo_auth), forges.seen
    assert github_auth and all(auth == f"Bearer {TOKEN}" for auth in github_auth), (
        forges.seen
    )


def test_the_token_never_reaches_the_log(forges, run_mirror):
    forges.publish_source()
    result = run_mirror(TAG)
    assert result.returncode == 0, context(result, forges)
    assert TOKEN not in result.stdout, result.stdout


def test_a_prerelease_stays_a_prerelease(forges, run_mirror):
    forges.publish_source(prerelease=True)
    result = run_mirror(TAG)
    assert_mirrored_intact(forges, result, prerelease=True)


def test_a_forgejo_release_that_appears_later_is_waited_for(forges, run_mirror):
    forges.publish_source()
    forges.source_404s = 3
    result = run_mirror(TAG)
    assert_mirrored_intact(forges, result, prerelease=False)


def test_a_forgejo_release_that_never_appears_times_out_with_nothing_on_github(
    forges, run_mirror
):
    result = run_mirror(TAG, environment={"MIRROR_TIMEOUT": "0.5"})
    assert result.returncode == 1, context(result, forges)
    assert not forges.releases, context(result, forges)


def test_a_forgejo_draft_is_not_mirrored(forges, run_mirror):
    forges.publish_source(draft=True)
    result = run_mirror(TAG, environment={"MIRROR_TIMEOUT": "0.5"})
    assert result.returncode == 1, context(result, forges)
    assert not forges.releases, context(result, forges)


def test_an_asset_shorter_than_forgejo_lists_fails_before_github_is_touched(
    forges, run_mirror
):
    forges.publish_source()
    forges.short_file = "felis.proto"
    result = run_mirror(TAG)
    assert result.returncode == 1, context(result, forges)
    assert not forges.releases, context(result, forges)


def test_an_asset_name_with_a_path_is_refused(forges, run_mirror):
    forges.publish_source()
    assert forges.source is not None
    forges.source["assets"][0]["name"] = "../escape"
    result = run_mirror(TAG)
    assert result.returncode == 1, context(result, forges)
    assert not forges.releases, context(result, forges)


def test_a_failed_upload_leaves_the_github_release_a_draft(forges, run_mirror):
    forges.publish_source()
    forges.fail_upload = "felis.proto"
    result = run_mirror(TAG)
    assert result.returncode == 1, context(result, forges)
    assert not published(forges), context(result, forges)


def test_a_leftover_github_draft_is_replaced(forges, run_mirror):
    forges.publish_source()
    forges.add_release(draft=True, names=["felis.proto"])
    result = run_mirror(TAG)
    assert_mirrored_intact(forges, result, prerelease=False)


def test_a_complete_github_release_is_left_alone(forges, run_mirror):
    forges.publish_source()
    forges.add_release(draft=False, names=list(ASSETS))
    result = run_mirror(TAG)
    assert result.returncode == 0, context(result, forges)
    assert not [m for m, _, _ in forges.seen if m == "POST"], context(result, forges)


def test_an_incomplete_published_github_release_is_refused_not_replaced(
    forges, run_mirror
):
    forges.publish_source()
    forges.add_release(draft=False, names=["felis.proto"])
    result = run_mirror(TAG)
    assert result.returncode == 1, context(result, forges)
    assert len(forges.releases) == 1, context(result, forges)


def test_a_missing_token_fails(forges, run_mirror):
    forges.publish_source()
    result = run_mirror(TAG, environment={"GH_TOKEN": ""})
    assert result.returncode == 1, context(result, forges)
    assert not forges.releases, context(result, forges)


def test_no_tag_prints_usage(forges, run_mirror):
    result = run_mirror()
    assert result.returncode == 64, context(result, forges)
