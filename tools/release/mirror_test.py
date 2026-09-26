#!/usr/bin/env python3
"""Exit-code tests for tools/release/mirror.py against a local fake Forgejo and GitHub."""

from __future__ import annotations

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
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
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


class Suite:
    def __init__(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.failures = 0
        self.cases = 0
        self.forges = FakeForges()
        self.out = ""

    def close(self) -> None:
        self.forges.close()
        self.temporary.cleanup()

    def reset(self) -> None:
        self.forges.source = None
        self.forges.source_404s = 0
        self.forges.files.clear()
        self.forges.short_file = ""
        self.forges.releases.clear()
        self.forges.uploaded.clear()
        self.forges.fail_upload = ""
        self.forges.seen.clear()

    def record(self, passed: bool, name: str) -> None:
        self.cases += 1
        if passed:
            print(f"  ok   {name}")
            return
        self.failures += 1
        print(f"  FAIL {name}")
        for line in self.out.splitlines():
            print(f"       | {line}")

    def run_mirror(self, *args: str, environment: dict[str, str] | None = None) -> int:
        env = os.environ.copy()
        env.update(
            {
                "MIRROR_SOURCE": f"{self.forges.origin}{SOURCE_API}",
                "GITHUB_API_URL": self.forges.origin,
                "GITHUB_REPOSITORY": "felis-terminal/felis",
                "GH_TOKEN": TOKEN,
                "MIRROR_TIMEOUT": "5",
                "MIRROR_INTERVAL": "0",
                "RUNNER_TEMP": str(self.root),
            }
        )
        env.update(environment or {})
        result = subprocess.run(
            [sys.executable, str(MIRROR), *args],
            env=env,
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        self.out = result.stdout
        return result.returncode

    def published(self) -> list[dict]:
        return [r for r in self.forges.releases.values() if not r["draft"]]

    def mirrored_intact(self, *, prerelease: bool) -> bool:
        releases = self.published()
        if len(releases) != 1 or len(self.forges.releases) != 1:
            return False
        release = releases[0]
        return (
            release["body"] == "felis notes\n"
            and release["prerelease"] is prerelease
            and release["make_latest"] == "legacy"
            and {
                name: data
                for (number, name), data in self.forges.uploaded.items()
                if number == release["id"]
            }
            == ASSETS
        )

    def cases_run(self) -> None:
        self.reset()
        self.forges.publish_source()
        code = self.run_mirror(TAG)
        self.record(
            code == 0 and self.mirrored_intact(prerelease=False),
            f"a published Forgejo release is mirrored byte for byte (exit {code})",
        )
        forgejo_auth = [
            auth
            for _, path, auth in self.forges.seen
            if path.startswith(SOURCE_API) or path.startswith(("/download/", "/files/"))
        ]
        github_auth = [
            auth
            for _, path, auth in self.forges.seen
            if path.startswith((GITHUB_API, "/uploads/"))
        ]
        self.record(
            forgejo_auth
            and not any(forgejo_auth)
            and github_auth
            and all(auth == f"Bearer {TOKEN}" for auth in github_auth),
            "the GitHub token reaches GitHub and never Forgejo",
        )
        self.record(
            all(TOKEN not in line for line in self.out.splitlines()),
            "the token never reaches the log",
        )

        self.reset()
        self.forges.publish_source(prerelease=True)
        code = self.run_mirror(TAG)
        self.record(
            code == 0 and self.mirrored_intact(prerelease=True),
            f"a prerelease stays a prerelease (exit {code})",
        )

        self.reset()
        self.forges.publish_source()
        self.forges.source_404s = 3
        code = self.run_mirror(TAG)
        self.record(
            code == 0 and self.mirrored_intact(prerelease=False),
            f"a Forgejo release that appears later is waited for (exit {code})",
        )

        self.reset()
        code = self.run_mirror(TAG, environment={"MIRROR_TIMEOUT": "0.5"})
        self.record(
            code == 1 and not self.forges.releases,
            f"a Forgejo release that never appears times out with nothing on GitHub (exit {code})",
        )

        self.reset()
        self.forges.publish_source(draft=True)
        code = self.run_mirror(TAG, environment={"MIRROR_TIMEOUT": "0.5"})
        self.record(
            code == 1 and not self.forges.releases,
            f"a Forgejo draft is not mirrored (exit {code})",
        )

        self.reset()
        self.forges.publish_source()
        self.forges.short_file = "felis.proto"
        code = self.run_mirror(TAG)
        self.record(
            code == 1 and not self.forges.releases,
            f"an asset shorter than Forgejo lists fails before GitHub is touched (exit {code})",
        )

        self.reset()
        self.forges.publish_source()
        assert self.forges.source is not None
        self.forges.source["assets"][0]["name"] = "../escape"
        code = self.run_mirror(TAG)
        self.record(
            code == 1 and not self.forges.releases,
            f"an asset name with a path is refused (exit {code})",
        )

        self.reset()
        self.forges.publish_source()
        self.forges.fail_upload = "felis.proto"
        code = self.run_mirror(TAG)
        self.record(
            code == 1 and not self.published(),
            f"a failed upload leaves the GitHub release a draft (exit {code})",
        )

        self.reset()
        self.forges.publish_source()
        self.forges.add_release(draft=True, names=["felis.proto"])
        code = self.run_mirror(TAG)
        self.record(
            code == 0 and self.mirrored_intact(prerelease=False),
            f"a leftover GitHub draft is replaced (exit {code})",
        )

        self.reset()
        self.forges.publish_source()
        self.forges.add_release(draft=False, names=list(ASSETS))
        code = self.run_mirror(TAG)
        posts = [m for m, p, _ in self.forges.seen if m == "POST"]
        self.record(
            code == 0 and not posts,
            f"a complete GitHub release is left alone (exit {code})",
        )

        self.reset()
        self.forges.publish_source()
        self.forges.add_release(draft=False, names=["felis.proto"])
        code = self.run_mirror(TAG)
        self.record(
            code == 1 and len(self.forges.releases) == 1,
            f"an incomplete published GitHub release is refused, not replaced (exit {code})",
        )

        self.reset()
        self.forges.publish_source()
        code = self.run_mirror(TAG, environment={"GH_TOKEN": ""})
        self.record(
            code == 1 and not self.forges.releases,
            f"a missing token fails (exit {code})",
        )

        code = self.run_mirror()
        self.record(code == 64, f"no tag prints usage (exit {code})")

    def run(self) -> int:
        self.cases_run()
        if self.failures == 0:
            print(f"release-mirror self-test: all {self.cases} cases passed")
            return 0
        print(
            f"release-mirror self-test: {self.failures} of {self.cases} case(s) failed",
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
