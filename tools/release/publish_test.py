#!/usr/bin/env python3
"""Exit-code tests for tools/release/publish.py against a local fake forge."""

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
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
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


class Suite:
    def __init__(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.failures = 0
        self.cases = 0
        self.forge = FakeForge()
        self.out = ""

    def close(self) -> None:
        self.forge.close()
        self.temporary.cleanup()

    def record(self, passed: bool, name: str) -> None:
        self.cases += 1
        if passed:
            print(f"  ok   {name}")
            return
        self.failures += 1
        print(f"  FAIL {name}")
        for line in self.out.splitlines():
            print(f"       | {line}")

    def run_publish(
        self, cwd: Path, *args: str, environment: dict[str, str] | None = None
    ) -> int:
        env = os.environ.copy()
        for key in ("FORGEJO_SERVER_URL", "GITHUB_REPOSITORY", "RELEASE_TOKEN"):
            env.pop(key, None)
        env.update(
            {
                "FORGEJO_SERVER_URL": self.forge.origin,
                "GITHUB_REPOSITORY": REPOSITORY,
                "RELEASE_TOKEN": TOKEN,
                "RUNNER_TEMP": str(self.root),
            }
        )
        env.update(environment or {})
        result = subprocess.run(
            [sys.executable, str(PUBLISH), *args],
            cwd=cwd,
            env=env,
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        self.out = result.stdout
        return result.returncode

    def reset_forge(self) -> None:
        self.forge.releases.clear()
        self.forge.files.clear()
        self.forge.seen.clear()
        self.forge.drop_upload = ""
        self.forge.fail_upload = ""
        self.forge.fail_lookup = False

    # -- release ----------------------------------------------------------

    def release_fixture(self) -> tuple[Path, list[str]]:
        work = self.root / "release"
        (work / "artifacts").mkdir(parents=True, exist_ok=True)
        (work / "release-notes.md").write_text("felis notes\n", encoding="utf-8")
        assets = []
        for name in ("felis-x86_64-linux.tar.gz", "felis.proto"):
            (work / "artifacts" / name).write_bytes(f"{name} bytes\r\n".encode())
            assets.append(f"artifacts/{name}")
        return work, assets

    def release_of(self, tag: str) -> dict | None:
        found = [r for r in self.forge.releases.values() if r["tag_name"] == tag]
        return found[0] if found else None

    def expect_release(self, wanted: int, name: str, tag: str, check) -> None:  # type: ignore[no-untyped-def]
        work, assets = self.release_fixture()
        code = self.run_publish(work, "release", tag, "release-notes.md", *assets)
        passed = code == wanted and check(self.release_of(tag))
        self.record(passed, f"{name} (exit {code}, wanted {wanted})")

    def release_cases(self) -> None:
        def published(release: dict | None) -> bool:
            return (
                release is not None
                and release["draft"] is False
                and release["prerelease"] is False
                and release["body"] == "felis notes\n"
                and {a["name"] for a in release["assets"]}
                == {"felis-x86_64-linux.tar.gz", "felis.proto"}
                and self.forge.files[f"/attachments/{release['id']}/felis.proto"]
                == b"felis.proto bytes\r\n"
            )

        self.reset_forge()
        self.expect_release(
            0, "a fresh tag is published with every asset", "v0.1.0", published
        )
        self.record(
            all(auth == f"token {TOKEN}" for _, _, auth in self.forge.seen),
            "every forge request carries the token",
        )
        methods = [method for method, *_ in self.forge.seen]
        self.record(
            methods.index("PATCH") == len(methods) - 1
            and methods.index("POST") < methods.index("PATCH"),
            "the release is created as a draft and published last",
        )

        self.reset_forge()
        self.expect_release(
            0,
            "a candidate tag is published as a prerelease",
            "v0.1.0-rc.1",
            lambda r: r is not None and r["prerelease"] is True and not r["draft"],
        )

        self.reset_forge()
        stale = self.forge.add_release("v0.1.0", draft=True)
        self.expect_release(
            0,
            "a rerun deletes the unfinished draft and cuts the release again",
            "v0.1.0",
            lambda r: published(r) and r["id"] != stale["id"],
        )

        self.reset_forge()
        done = self.forge.add_release("v0.1.0")
        self.expect_release(
            1,
            "a published release is never replaced",
            "v0.1.0",
            lambda r: (
                r is done
                and not any(
                    m in {"POST", "DELETE", "PATCH"} for m, *_ in self.forge.seen
                )
            ),
        )

        self.reset_forge()
        self.forge.drop_upload = "felis.proto"
        self.expect_release(
            1,
            "a draft missing an asset stays a draft",
            "v0.1.0",
            lambda r: r is not None and r["draft"] is True,
        )

        self.reset_forge()
        self.forge.fail_upload = "felis-x86_64-linux.tar.gz"
        self.expect_release(
            1,
            "a failed upload leaves the draft unpublished",
            "v0.1.0",
            lambda r: r is not None and r["draft"] is True,
        )

        self.reset_forge()
        self.forge.fail_lookup = True
        self.expect_release(
            1,
            "a failed lookup of the tag's release is not read as no release",
            "v0.1.0",
            lambda r: r is None,
        )

        self.reset_forge()
        work, assets = self.release_fixture()
        code = self.run_publish(
            work,
            "release",
            "v0.1.0",
            "release-notes.md",
            *assets,
            "artifacts/absent.zip",
        )
        self.record(
            code == 1 and not self.forge.seen,
            f"an absent asset file fails before the forge is touched (exit {code})",
        )

        code = self.run_publish(
            work,
            "release",
            "v0.1.0",
            "release-notes.md",
            *assets,
            environment={"RELEASE_TOKEN": ""},
        )
        self.record(code == 1, f"a missing token fails (exit {code})")
        code = self.run_publish(work, "release", "v0.1.0")
        self.record(
            code == 64, f"a release with no assets is a usage error (exit {code})"
        )

    def run(self) -> int:
        self.release_cases()
        if self.failures == 0:
            print(f"release-publish self-test: all {self.cases} cases passed")
            return 0
        print(
            f"release-publish self-test: {self.failures} of {self.cases} case(s) failed",
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
