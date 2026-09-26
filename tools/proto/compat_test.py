#!/usr/bin/env python3
"""Exit-code tests for tools/proto/compat.py in a throwaway repository."""

from __future__ import annotations

import contextlib
import http.server
import importlib.util
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

COMPAT = Path(__file__).with_name("compat.py")
REPO_ROOT = Path(__file__).resolve().parents[2]
MODULE = Path("crates/felis-protocol/proto")
SCRUBBED = (
    "GITHUB_EVENT_NAME",
    "GITHUB_REF_TYPE",
    "GITHUB_REF_NAME",
    "PROTO_COMPAT_BASE",
    "PROTO_COMPAT_PR_BASE",
    "PROTO_COMPAT_PUSH_BEFORE",
    "PROTO_COMPAT_BUF_MODULE_DIR",
    "GITHUB_SERVER_URL",
    "GITHUB_REPOSITORY",
    "FORGEJO_SERVER_URL",
    "FORGEJO_TOKEN",
)
SERVER = "https://forge.invalid"
REPOSITORY = "felis-terminal/felis"
FETCH_ERROR = object()


class BinaryView:
    def __init__(self, text: io.StringIO) -> None:
        self.text = text

    def write(self, data: bytes) -> int:
        self.text.write(data.decode("utf-8", "replace"))
        return len(data)

    def flush(self) -> None:
        pass


class CapturedStream(io.StringIO):
    """A stderr stand-in: the gate writes buf's diagnostics to `.buffer`."""

    @property
    def buffer(self) -> BinaryView:
        return BinaryView(self)


def load_compat():
    spec = importlib.util.spec_from_file_location("compat_under_test", COMPAT)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load {COMPAT}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class Forge:
    """A one-origin HTTP server the transport cases can be pointed at."""

    def __init__(self) -> None:
        self.seen: list[tuple[str, str | None]] = []
        self.redirect_to = ""
        self.redirect_all = False
        forge = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.0"

            def do_GET(self) -> None:
                forge.seen.append((self.path, self.headers.get("Authorization")))
                if self.path == "/slow":
                    time.sleep(2)
                if self.path == "/boom":
                    self.send_error(500)
                    return
                if forge.redirect_all or self.path == "/redirect":
                    self.send_response(302)
                    self.send_header("Location", forge.redirect_to)
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                    return
                body = b"[]" if self.path.startswith("/api/") else b"payload"
                self.send_response(200)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *args: object) -> None:
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.origin = "http://127.0.0.1:%d" % self.server.server_address[1]
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)


def release(tag: str) -> dict:
    return {"tag_name": tag, "draft": False, "prerelease": False, "assets": []}


class Suite:
    def __init__(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.out = ""
        self.cases = 0
        self.failures = 0

    def close(self) -> None:
        self.temporary.cleanup()

    def git(self, *args: str, capture: bool = False) -> str:
        result = subprocess.run(
            ["git", "-C", str(self.repo), *args],
            check=True,
            text=True,
            stdout=subprocess.PIPE if capture else subprocess.DEVNULL,
        )
        return result.stdout.strip() if capture else ""

    def commit(self, message: str) -> None:
        self.git("add", "-A")
        self.git(
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            message,
        )

    def pass_case(self, name: str) -> None:
        self.cases += 1
        print(f"  ok   {name}")

    def fail_case(self, name: str) -> None:
        self.cases += 1
        self.failures += 1
        print(f"  FAIL {name}")
        for line in self.out.splitlines():
            print(f"       | {line}")

    def expect(
        self,
        wanted: int,
        name: str,
        *args: str,
        environment: dict[str, str] | None = None,
        cwd: Path | None = None,
    ) -> None:
        env = os.environ.copy()
        for key in SCRUBBED:
            env.pop(key, None)
        if environment:
            env.update(environment)
        result = subprocess.run(
            [sys.executable, str(COMPAT), *args],
            cwd=cwd or self.repo,
            env=env,
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        self.out = result.stdout
        if result.returncode == wanted:
            self.pass_case(name)
        else:
            self.fail_case(f"{name} (exit {result.returncode}, wanted {wanted})")

    def check(self, condition: bool, name: str) -> None:
        if condition:
            self.pass_case(name)
        else:
            self.fail_case(name)

    def raises(self, module, name: str, call) -> None:
        try:
            call()
        except module.GateError as error:
            self.out = str(error)
            self.pass_case(name)
        else:
            self.fail_case(f"{name} (no GateError was raised)")

    def expect_tag(
        self,
        wanted: int,
        name: str,
        *,
        tag: str,
        listing,
        event: dict[str, str] | None = None,
    ) -> None:
        """Run the tag path (or `event`) in-process with the release fetch replaced."""
        module = load_compat()

        def fetch(url: str, credential_origin: str) -> bytes:
            if "/releases?" not in url:
                raise module.GateError(f"unexpected fetch of {url}")
            if listing is FETCH_ERROR:
                raise module.GateError(f"cannot reach {url}: fixture refuses")
            if isinstance(listing, bytes):
                return listing
            return json.dumps(listing).encode("utf-8")

        module.fetch_url = fetch
        environment = {
            key: value for key, value in os.environ.items() if key not in SCRUBBED
        }
        environment.update(event or {"GITHUB_REF_TYPE": "tag", "GITHUB_REF_NAME": tag})
        environment.update(
            {"GITHUB_SERVER_URL": SERVER, "GITHUB_REPOSITORY": REPOSITORY}
        )
        stream = CapturedStream()
        saved_env = os.environ.copy()
        saved_cwd = os.getcwd()
        try:
            os.chdir(self.repo)
            os.environ.clear()
            os.environ.update(environment)
            with contextlib.redirect_stdout(stream), contextlib.redirect_stderr(stream):
                status = module.main([])
        finally:
            os.environ.clear()
            os.environ.update(saved_env)
            os.chdir(saved_cwd)
        self.out = stream.getvalue()
        if status == wanted:
            self.pass_case(name)
        else:
            self.fail_case(f"{name} (exit {status}, wanted {wanted})")

    def expect_base(self, revision: str, name: str) -> None:
        if f"proto-compat: comparing against {revision} " in self.out:
            self.pass_case(name)
        else:
            self.fail_case(f"{name} (no 'comparing against {revision}' line)")

    @staticmethod
    def schema(extra: str) -> str:
        return f"""syntax = "proto3";

package felis.v1;

message Hello {{
  uint32 mode = 1;
  string name = 2;
{extra}
}}
"""

    def write_schema(self, extra: str) -> None:
        self.repo.joinpath(MODULE, "felis.proto").write_text(
            self.schema(extra), encoding="utf-8"
        )

    def replace_schema(self, old: str, new: str) -> None:
        path = self.repo / MODULE / "felis.proto"
        text = path.read_text(encoding="utf-8")
        if old not in text:
            raise RuntimeError(f"schema does not contain {old!r}")
        path.write_text(text.replace(old, new), encoding="utf-8")

    def write_ack(self, *lines: str) -> None:
        self.repo.joinpath(MODULE, "BREAKING.md").write_text(
            "\n".join(lines) + "\n", encoding="utf-8"
        )

    def make_fake(self, directory: Path, name: str, body: str) -> None:
        directory.mkdir(parents=True, exist_ok=True)
        path = directory / name
        path.write_text(f"#!{sys.executable}\n{body}", encoding="utf-8")
        path.chmod(0o755)

    def transport_cases(self) -> None:
        print("compat.py reaching the forge over HTTP")
        module = load_compat()
        module.FETCH_TIMEOUT = 1.0
        forge = Forge()
        elsewhere = Forge()
        forge.redirect_to = elsewhere.origin + "/api/v1/whatever"
        token = "not-a-real-token"
        saved = os.environ.copy()
        try:
            os.environ["FORGEJO_TOKEN"] = token
            os.environ["GITHUB_SERVER_URL"] = forge.origin
            os.environ["GITHUB_REPOSITORY"] = REPOSITORY
            gate = module.CompatibilityGate()

            self.check(
                module.fetch_url(forge.origin + "/ok", forge.origin) == b"payload",
                "a 200 from the configured origin is read",
            )
            self.check(
                forge.seen[-1][1] == f"token {token}",
                "the token is sent to the configured origin",
            )
            module.fetch_url(elsewhere.origin + "/ok", forge.origin)
            self.check(
                elsewhere.seen[-1][1] is None,
                "no token is sent to any other origin",
            )
            self.raises(
                module,
                "a non-2xx answer fails",
                lambda: module.fetch_url(forge.origin + "/boom", forge.origin),
            )
            self.raises(
                module,
                "a url that is not http(s) fails",
                lambda: module.fetch_url("file:///etc/passwd", forge.origin),
            )
            self.raises(
                module,
                "a server that never answers fails on the timeout",
                lambda: module.fetch_url(forge.origin + "/slow", forge.origin),
            )

            before = len(elsewhere.seen)
            forge.redirect_all = True
            self.raises(
                module,
                "a redirected release listing fails instead of answering from elsewhere",
                gate.published_release,
            )
            self.check(
                len(elsewhere.seen) == before,
                "a refused redirect never reaches the other origin",
            )
            forge.redirect_all = False
            self.check(
                gate.published_release() is None,
                "an empty release listing over HTTP means no release is published",
            )
        finally:
            os.environ.clear()
            os.environ.update(saved)
            forge.close()
            elsewhere.close()

    def run(self) -> int:
        self.repo.joinpath(MODULE).mkdir(parents=True)
        self.git("init", "-q", "-b", "main")
        self.repo.joinpath("crates/felis-protocol/buf.yaml").write_text(
            """version: v2
modules:
  - path: proto
breaking:
  use:
    - WIRE_JSON
""",
            encoding="utf-8",
        )
        self.write_schema("")
        self.commit("base")
        base = self.git("rev-parse", "HEAD", capture=True)
        self.repo.joinpath("README").write_text("notes\n", encoding="utf-8")
        self.commit("prose only")
        prose = self.git("rev-parse", "HEAD", capture=True)
        self.git("remote", "add", "origin", str(self.repo))
        self.git("fetch", "-q", "origin")

        print("compat.py against a git base")
        self.write_schema("  bool pull_paced = 3;")
        self.commit("additive")
        self.expect(0, "an additive change passes against an explicit base", base)
        self.expect(
            0,
            "PROTO_COMPAT_BASE names the base",
            environment={"PROTO_COMPAT_BASE": base},
        )
        self.expect(
            0, "the local default compares against the merge-base with origin/main"
        )

        self.write_schema("  bool pull_paced = 3;")
        self.replace_schema("string name = 2;", "int64 name = 2;")
        self.commit("retype a field")
        self.expect(1, "a wire-incompatible change fails", base)
        self.expect(
            1,
            "a base missing from the clone fails rather than passes",
            "0123456789abcdef0123456789abcdef01234567",
        )

        self.git("branch", "-q", "pr-base", prose)
        self.git("checkout", "-q", "pr-base")
        self.write_schema("  bool pull_paced = 3;")
        self.replace_schema("string name = 2;", "int64 name = 2;")
        self.commit("the same break on the PR base branch")
        pr_base_tip = self.git("rev-parse", "HEAD", capture=True)
        self.git("checkout", "-q", "main")
        print("compat.py selecting the base from the event (unacknowledged break)")
        self.expect(
            1,
            "pull_request fails the break against the merge-base, not the PR base tip",
            environment={
                "GITHUB_EVENT_NAME": "pull_request",
                "PROTO_COMPAT_PR_BASE": pr_base_tip,
            },
        )
        self.expect_base(prose, "pull_request printed the merge-base")
        self.expect(
            1,
            "push fails the break against github.event.before",
            environment={
                "GITHUB_EVENT_NAME": "push",
                "PROTO_COMPAT_PUSH_BEFORE": base,
            },
        )
        self.expect_base(base, "push printed github.event.before")
        self.expect(
            1,
            "the local default fails the break against the merge-base with origin/main",
        )
        self.expect_base(
            prose, "the local default printed the merge-base with origin/main"
        )

        self.write_ack("# Intended pre-release break", "", f"base: {base}")
        self.commit("acknowledge against the base")
        self.expect(0, "the acknowledgment naming the base exactly passes", base)
        self.expect(
            0,
            "the acknowledgment holds when the base moved by prose-only commits",
            prose,
        )
        head_broken = self.git("rev-parse", "HEAD", capture=True)
        self.write_ack(f"base: {base} ")
        self.expect(
            0,
            "an acknowledgment line with trailing whitespace still covers the base",
            base,
        )
        self.git("checkout", "-q", "--", str(MODULE / "BREAKING.md"))
        self.git("branch", "-q", "side", base)
        self.git("checkout", "-q", "side")
        self.repo.joinpath("README").write_text("aside\n", encoding="utf-8")
        self.commit("prose only, off the base's history")
        side = self.git("rev-parse", "HEAD", capture=True)
        self.git("checkout", "-q", "main")
        self.write_ack(f"base: {side}")
        self.expect(
            1,
            "an acknowledgment naming a non-ancestor with the base's schema does not cover it",
            prose,
        )
        self.git("checkout", "-q", "--", str(MODULE / "BREAKING.md"))

        print("compat.py on an acknowledgment the base already carried")
        self.git("checkout", "-q", "-B", "inherited", prose)
        self.write_ack("# Intended pre-release break", "", f"base: {base}")
        self.commit("an acknowledgment committed with no break")
        inherited_tip = self.git("rev-parse", "HEAD", capture=True)
        self.write_schema("  bool pull_paced = 3;")
        self.replace_schema("string name = 2;", "int64 name = 2;")
        self.commit("a later break under the inherited acknowledgment")
        self.expect(
            1,
            "an acknowledgment the base already carried does not cover a later break",
            environment={
                "GITHUB_EVENT_NAME": "push",
                "PROTO_COMPAT_PUSH_BEFORE": inherited_tip,
            },
        )
        self.git("checkout", "-q", "-B", "acksymlink", prose)
        self.repo.joinpath(MODULE, "ACK.md").write_text(
            f"base: {base}\n", encoding="utf-8"
        )
        self.repo.joinpath(MODULE, "BREAKING.md").symlink_to("ACK.md")
        self.commit("an acknowledgment behind a symlink")
        symlink_tip = self.git("rev-parse", "HEAD", capture=True)
        self.write_schema("  bool pull_paced = 3;")
        self.replace_schema("string name = 2;", "int64 name = 2;")
        self.commit("a break under the symlinked acknowledgment")
        self.expect(
            1,
            "a symlinked acknowledgment file fails rather than covering a break",
            environment={
                "GITHUB_EVENT_NAME": "push",
                "PROTO_COMPAT_PUSH_BEFORE": symlink_tip,
            },
        )
        self.git("checkout", "-q", "main")

        print("compat.py after main took another schema change (post-merge push)")
        self.git("branch", "-q", "main-advanced", prose)
        self.git("checkout", "-q", "main-advanced")
        self.write_schema("  string cwd = 3;")
        self.commit("a sibling PR's additive field")
        main_tip = self.git("rev-parse", "HEAD", capture=True)
        self.write_schema("  string cwd = 3;\n  bool pull_paced = 4;")
        self.replace_schema("string name = 2;", "int64 name = 2;")
        self.write_ack("# Intended pre-release break", "", f"base: {base}")
        self.commit("merge of the acknowledged break")
        self.expect(
            0,
            "push passes on the acknowledgment when main only added fields since its base",
            environment={
                "GITHUB_EVENT_NAME": "push",
                "PROTO_COMPAT_PUSH_BEFORE": main_tip,
            },
        )
        self.expect_base(main_tip, "push printed github.event.before")
        self.expect(
            0,
            "pull_request passes on the acknowledgment after a rebase over the additive field",
            environment={
                "GITHUB_EVENT_NAME": "pull_request",
                "PROTO_COMPAT_PR_BASE": main_tip,
            },
        )

        self.git("reset", "-q", "--hard", main_tip)
        self.write_schema("  bytes cwd = 3;")
        self.replace_schema("string name = 2;", "int64 name = 2;")
        self.write_ack("# Intended pre-release break", "", f"base: {base}")
        self.commit("merge of the acknowledged break, plus an unacknowledged cwd break")
        self.expect(
            1,
            "push fails when the candidate breaks a field the base introduced since the acknowledged base",
            environment={
                "GITHUB_EVENT_NAME": "push",
                "PROTO_COMPAT_PUSH_BEFORE": main_tip,
            },
        )
        self.git("reset", "-q", "--hard", main_tip)
        self.replace_schema("uint32 mode = 1;", "bytes mode = 1;")
        self.commit("an unacknowledged break on main")
        main_tip = self.git("rev-parse", "HEAD", capture=True)
        self.write_schema("  string cwd = 3;\n  bool pull_paced = 4;")
        self.replace_schema("string name = 2;", "int64 name = 2;")
        self.replace_schema("uint32 mode = 1;", "bytes mode = 1;")
        self.write_ack("# Intended pre-release break", "", f"base: {base}")
        self.commit("merge of the acknowledged break onto the broken main")
        self.expect(
            1,
            "push fails when main took an incompatible change since the acknowledged base",
            environment={
                "GITHUB_EVENT_NAME": "push",
                "PROTO_COMPAT_PUSH_BEFORE": main_tip,
            },
        )
        self.git("checkout", "-q", "main")

        print("compat.py on a buf failure under a live acknowledgment")
        self.write_schema("  bool pull_paced = 3\n")
        self.expect(
            1,
            "a schema that does not compile fails even with the acknowledgment",
            base,
        )
        self.git("checkout", "-q", "--", str(MODULE / "felis.proto"))
        fake_bin = self.root / "fakebin"
        self.make_fake(
            fake_bin,
            "buf",
            'import sys\nprint("fake buf: cannot fetch the ref", file=sys.stderr)\nraise SystemExit(1)\n',
        )
        self.expect(
            1,
            "a buf execution error (exit 1) fails even with the acknowledgment",
            base,
            environment={"PATH": f"{fake_bin}:{os.environ['PATH']}"},
        )
        no_buf = self.root / "nobuf"
        no_buf.mkdir()
        git_path = shutil.which("git")
        if git_path is None:
            raise RuntimeError("git not found")
        no_buf.joinpath("git").symlink_to(git_path)
        self.expect(
            1,
            "a missing buf binary fails even with the acknowledgment",
            base,
            environment={"PATH": str(no_buf)},
        )

        self.write_schema("  bool pull_paced = 3;")
        self.replace_schema("string name = 2;", "int64 name = 2;")
        self.replace_schema("uint32 mode = 1;", "bytes mode = 1;")
        self.commit("a second break under a stale acknowledgment")
        self.expect(
            1,
            "a stale acknowledgment does not cover a new break",
            head_broken,
        )
        self.git("reset", "-q", "--hard", head_broken)

        print("compat.py selecting the base from the event (acknowledged break)")
        self.expect(
            0,
            "pull_request passes on the acknowledgment covering the merge-base",
            environment={
                "GITHUB_EVENT_NAME": "pull_request",
                "PROTO_COMPAT_PR_BASE": pr_base_tip,
            },
        )
        self.expect_base(prose, "pull_request printed the merge-base")
        self.expect(
            1,
            "pull_request without the PR base sha fails",
            environment={"GITHUB_EVENT_NAME": "pull_request"},
        )
        self.expect(
            0,
            "push passes on the acknowledgment naming github.event.before",
            environment={
                "GITHUB_EVENT_NAME": "push",
                "PROTO_COMPAT_PUSH_BEFORE": base,
            },
        )
        self.expect_base(base, "push printed github.event.before")
        self.expect(
            0,
            "the local default passes on the acknowledgment covering the merge-base with origin/main",
        )
        self.expect_base(
            prose, "the local default printed the merge-base with origin/main"
        )
        self.expect(
            1,
            "push with a before sha outside the clone fails",
            environment={
                "GITHUB_EVENT_NAME": "push",
                "PROTO_COMPAT_PUSH_BEFORE": "0123456789abcdef0123456789abcdef01234567",
            },
        )
        self.expect(
            1,
            "push with the null sha fails when no forge is named",
            environment={
                "GITHUB_EVENT_NAME": "push",
                "PROTO_COMPAT_PUSH_BEFORE": "0" * 40,
            },
        )

        print("compat.py on a shallow clone")
        shallow = self.root / "shallow"
        subprocess.run(
            ["git", "clone", "-q", "--depth", "1", f"file://{self.repo}", str(shallow)],
            check=True,
        )
        self.expect(
            1,
            "a shallow clone fails on the unresolvable base",
            base,
            cwd=shallow,
        )

        print("compat.py against the newest published release")
        self.expect_tag(
            0,
            "a tag build with no published release passes",
            tag="v0.1.0",
            listing=[],
        )
        self.expect_tag(
            1,
            "a tag build fails when the release listing cannot be fetched",
            tag="v0.1.0",
            listing=FETCH_ERROR,
        )
        self.expect_tag(
            1,
            "a tag build fails when the release listing is not JSON",
            tag="v0.1.0",
            listing=b"<html>forbidden</html>",
        )
        self.expect_tag(
            1,
            "a tag build fails when the listing is not an array of releases",
            tag="v0.1.0",
            listing={"message": "token required"},
        )
        self.expect_tag(
            1,
            "a published release whose tag is not in the clone fails",
            tag="v0.2.0",
            listing=[release("v0.1.0")],
        )
        self.git(
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "tag",
            "-a",
            "-m",
            "v0.1.0",
            "v0.1.0",
        )
        self.expect_tag(
            0,
            "a tag build passes against its own released schema",
            tag="v0.1.0",
            listing=[release("v0.1.0")],
        )
        self.write_schema("  bool pull_paced = 3;\n  string cwd = 4;")
        self.replace_schema("string name = 2;", "int64 name = 2;")
        self.commit("additive after the release")
        self.expect_tag(
            0,
            "an additive change passes against the released schema",
            tag="v0.2.0",
            listing=[release("v0.1.0")],
        )
        self.expect_tag(
            0,
            "a prerelease tag is compared with the last final release",
            tag="v0.2.0-rc.1",
            listing=[release("v0.1.0")],
        )
        self.expect_tag(
            0,
            "a null-sha push is compared with the newest published release",
            tag="",
            listing=[release("v0.1.0")],
            event={"GITHUB_EVENT_NAME": "push", "PROTO_COMPAT_PUSH_BEFORE": "0" * 40},
        )
        self.replace_schema("uint32 mode = 1;", "bytes mode = 1;")
        self.write_ack(f"base: {self.git('rev-parse', 'HEAD', capture=True)}")
        self.commit("break after the release, acknowledged")
        self.expect_tag(
            1,
            "a tag build ignores the acknowledgment and fails a post-release break",
            tag="v0.2.0",
            listing=[release("v0.1.0")],
        )
        self.expect_tag(
            1,
            "a null-sha push fails a post-release break too",
            tag="",
            listing=[release("v0.1.0")],
            event={"GITHUB_EVENT_NAME": "push", "PROTO_COMPAT_PUSH_BEFORE": "0" * 40},
        )

        self.transport_cases()

        print("the committed layout")
        result = subprocess.run(
            ["buf", "lint"],
            cwd=REPO_ROOT / "crates/felis-protocol",
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        self.out = result.stdout
        if result.returncode == 0:
            self.pass_case("buf lint passes on the committed module")
        else:
            self.fail_case("buf lint on the committed module")

        if self.failures:
            print(f"{self.failures} of {self.cases} proto-compat cases failed")
            return 1
        print(f"all {self.cases} proto-compat cases passed")
        return 0


def main() -> int:
    suite = Suite()
    try:
        return suite.run()
    finally:
        suite.close()


if __name__ == "__main__":
    raise SystemExit(main())
