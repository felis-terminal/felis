"""Exit-code tests for tools/release/publish.py against a local fake forge."""

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

PUBLISH = Path(__file__).with_name("publish.py")
REPOSITORY = "felis-terminal/felis"
TOKEN = "fixture-token"
API = f"/api/v1/repos/{REPOSITORY}"


class FakeForge:
    """The slice of the Forgejo release API the script calls."""

    def __init__(self) -> None:
        self.releases: dict[int, dict] = {}
        self.files: dict[str, bytes] = {}
        self.seen: list[tuple[str, str, str | None]] = []
        self.drop_upload = ""
        self.fail_upload = ""
        self.fail_lookup = False
        self.next_id = 1
        forge = self

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
                forge.seen.append(
                    (
                        self.command,
                        self.path,
                        self.headers.get("Authorization"),
                    )
                )
                with forge.lock:
                    forge.route(self)

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

    def add_release(
        self, tag: str, *, draft: bool = False, prerelease: bool = False
    ) -> dict:
        release = {
            "id": self.next_id,
            "tag_name": tag,
            "name": tag,
            "body": "",
            "draft": draft,
            "prerelease": prerelease,
            "assets": [],
        }
        self.releases[self.next_id] = release
        self.next_id += 1
        return release

    def attach(self, release: dict, name: str, content: bytes) -> None:
        path = f"/attachments/{release['id']}/{name}"
        self.files[path] = content
        release["assets"].append(
            {"name": name, "browser_download_url": f"{self.origin}{path}"}
        )

    def route(self, h) -> None:  # type: ignore[no-untyped-def]
        url = urllib.parse.urlsplit(h.path)
        query = dict(urllib.parse.parse_qsl(url.query))
        path = url.path
        if h.command == "GET" and path.startswith("/attachments/"):
            # Forgejo serves an attachment through a redirect.
            h.send_response(302)
            h.send_header("Location", path.replace("/attachments/", "/files/", 1))
            h.send_header("Content-Length", "0")
            h.end_headers()
            return
        if h.command == "GET" and path.startswith("/files/"):
            h.reply(200, raw=self.files["/attachments/" + path[len("/files/") :]])
            return
        if not path.startswith(API):
            h.reply(404, {"message": "not found"})
            return
        path = path[len(API) :]

        if match := re.fullmatch(r"/releases/tags/(.+)", path):
            if self.fail_lookup:
                h.reply(500, {"message": "fixture fails the lookup"})
                return
            found = [r for r in self.releases.values() if r["tag_name"] == match[1]]
            h.reply(200, found[0]) if found else h.reply(404, {"message": "no"})
            return
        if path == "/releases" and h.command == "GET":
            listing = sorted(
                (
                    r
                    for r in self.releases.values()
                    if not r["draft"] and not r["prerelease"]
                ),
                key=lambda r: -r["id"],
            )
            h.reply(200, listing[: int(query.get("limit", "30"))])
            return
        if path == "/releases" and h.command == "POST":
            payload = json.loads(h.body())
            if any(
                r["tag_name"] == payload["tag_name"] for r in self.releases.values()
            ):
                h.reply(409, {"message": "release exists"})
                return
            release = self.add_release(
                payload["tag_name"],
                draft=payload["draft"],
                prerelease=payload["prerelease"],
            )
            release["name"] = payload["name"]
            release["body"] = payload["body"]
            h.reply(201, release)
            return
        if match := re.fullmatch(r"/releases/([0-9]+)/assets", path):
            release = self.releases.get(int(match[1]))
            data = h.body()
            name = query.get("name", "")
            if release is None:
                h.reply(404, {"message": "no"})
                return
            if name == self.fail_upload:
                h.reply(500, {"message": "fixture fails this upload"})
                return
            if (
                b'name="attachment"' not in data
                or f'filename="{name}"'.encode() not in data
            ):
                h.reply(422, {"message": "no attachment field"})
                return
            content = data.split(b"\r\n\r\n", 1)[1].rsplit(b"\r\n--", 1)[0]
            if name != self.drop_upload:
                self.attach(release, name, content)
            h.reply(201, {"name": name})
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


ASSETS = ["artifacts/felis-x86_64-linux.tar.gz", "artifacts/felis.proto"]


@pytest.fixture
def forge():
    forge = FakeForge()
    yield forge
    forge.close()


@pytest.fixture
def work(tmp_path: Path) -> Path:
    work = tmp_path / "release"
    (work / "artifacts").mkdir(parents=True)
    (work / "release-notes.md").write_text("felis notes\n", encoding="utf-8")
    for asset in ASSETS:
        name = Path(asset).name
        (work / asset).write_bytes(f"{name} bytes\r\n".encode())
    return work


@pytest.fixture
def run_publish(forge: FakeForge, work: Path, tmp_path: Path):
    def run(
        *args: str, environment: dict[str, str] | None = None
    ) -> subprocess.CompletedProcess[str]:
        env = os.environ.copy()
        env.update(
            {
                "FORGEJO_SERVER_URL": forge.origin,
                "GITHUB_REPOSITORY": REPOSITORY,
                "RELEASE_TOKEN": TOKEN,
                "RUNNER_TEMP": str(tmp_path),
            }
        )
        env.update(environment or {})
        return subprocess.run(
            [sys.executable, str(PUBLISH), *args],
            cwd=work,
            env=env,
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )

    return run


@pytest.fixture
def release(run_publish):
    def run(tag: str, *extra: str) -> subprocess.CompletedProcess[str]:
        return run_publish("release", tag, "release-notes.md", *ASSETS, *extra)

    return run


def release_of(forge: FakeForge, tag: str) -> dict | None:
    found = [r for r in forge.releases.values() if r["tag_name"] == tag]
    return found[0] if found else None


def context(result: subprocess.CompletedProcess[str], forge: FakeForge) -> str:
    seen = "\n".join(f"  {method} {path}" for method, path, _ in forge.seen)
    return f"exit {result.returncode}\n{result.stdout}\nrequests:\n{seen}"


def assert_published(forge: FakeForge, release: dict | None) -> None:
    assert release is not None
    assert release["draft"] is False
    assert release["prerelease"] is False
    assert release["body"] == "felis notes\n"
    assert {a["name"] for a in release["assets"]} == {
        "felis-x86_64-linux.tar.gz",
        "felis.proto",
    }
    assert (
        forge.files[f"/attachments/{release['id']}/felis.proto"]
        == b"felis.proto bytes\r\n"
    )


def test_a_fresh_tag_is_published_with_every_asset(forge, release):
    result = release("v0.1.0")
    assert result.returncode == 0, context(result, forge)
    assert_published(forge, release_of(forge, "v0.1.0"))


def test_every_forge_request_carries_the_token(forge, release):
    result = release("v0.1.0")
    assert result.returncode == 0, context(result, forge)
    assert forge.seen
    assert all(auth == f"token {TOKEN}" for _, _, auth in forge.seen), context(
        result, forge
    )


def test_the_release_is_created_as_a_draft_and_published_last(forge, release):
    result = release("v0.1.0")
    assert result.returncode == 0, context(result, forge)
    methods = [method for method, *_ in forge.seen]
    assert methods.index("PATCH") == len(methods) - 1, context(result, forge)
    assert methods.index("POST") < methods.index("PATCH"), context(result, forge)


def test_a_candidate_tag_is_published_as_a_prerelease(forge, release):
    result = release("v0.1.0-rc.1")
    assert result.returncode == 0, context(result, forge)
    published = release_of(forge, "v0.1.0-rc.1")
    assert published is not None
    assert published["prerelease"] is True
    assert not published["draft"]


def test_a_rerun_deletes_the_unfinished_draft_and_cuts_the_release_again(
    forge, release
):
    stale = forge.add_release("v0.1.0", draft=True)
    result = release("v0.1.0")
    assert result.returncode == 0, context(result, forge)
    published = release_of(forge, "v0.1.0")
    assert_published(forge, published)
    assert published["id"] != stale["id"]


def test_a_published_release_is_never_replaced(forge, release):
    done = forge.add_release("v0.1.0")
    result = release("v0.1.0")
    assert result.returncode == 1, context(result, forge)
    assert release_of(forge, "v0.1.0") is done
    assert not any(m in {"POST", "DELETE", "PATCH"} for m, *_ in forge.seen), context(
        result, forge
    )


def test_a_draft_missing_an_asset_stays_a_draft(forge, release):
    forge.drop_upload = "felis.proto"
    result = release("v0.1.0")
    assert result.returncode == 1, context(result, forge)
    draft = release_of(forge, "v0.1.0")
    assert draft is not None and draft["draft"] is True, context(result, forge)


def test_a_failed_upload_leaves_the_draft_unpublished(forge, release):
    forge.fail_upload = "felis-x86_64-linux.tar.gz"
    result = release("v0.1.0")
    assert result.returncode == 1, context(result, forge)
    draft = release_of(forge, "v0.1.0")
    assert draft is not None and draft["draft"] is True, context(result, forge)


def test_a_failed_lookup_of_the_tags_release_is_not_read_as_no_release(forge, release):
    forge.fail_lookup = True
    result = release("v0.1.0")
    assert result.returncode == 1, context(result, forge)
    assert release_of(forge, "v0.1.0") is None, context(result, forge)


def test_an_absent_asset_file_fails_before_the_forge_is_touched(forge, release):
    result = release("v0.1.0", "artifacts/absent.zip")
    assert result.returncode == 1, context(result, forge)
    assert not forge.seen, context(result, forge)


def test_a_missing_token_fails(forge, run_publish):
    result = run_publish(
        "release",
        "v0.1.0",
        "release-notes.md",
        *ASSETS,
        environment={"RELEASE_TOKEN": ""},
    )
    assert result.returncode == 1, context(result, forge)


def test_a_release_with_no_assets_is_a_usage_error(forge, run_publish):
    result = run_publish("release", "v0.1.0")
    assert result.returncode == 64, context(result, forge)
