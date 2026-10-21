"""Exit-code tests for tools/proto/compat.py in throwaway repositories."""

from __future__ import annotations

import contextlib
import http.server
import io
import json
import os
import shutil
import subprocess
import sys
import threading
import time
from pathlib import Path

import pytest

import compat

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
MISSING_SHA = "0123456789abcdef0123456789abcdef01234567"
NULL_SHA = "0" * 40
FETCH_ERROR = object()
# The developer's global config may sign with their key or install commit hooks.
GIT_CONFIG = (
    "-c",
    "user.name=t",
    "-c",
    "user.email=t@t",
    "-c",
    "commit.gpgSign=false",
    "-c",
    "tag.gpgSign=false",
    "-c",
    "core.hooksPath=/dev/null",
)


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


class Forge:
    """A one-origin HTTP server the transport tests can be pointed at."""

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
                    return
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


def schema(extra: str) -> str:
    return f"""syntax = "proto3";

package felis.v1;

message Hello {{
  uint32 mode = 1;
  string name = 2;
{extra}
}}
"""


def git(path: Path, *args: str) -> str:
    result = subprocess.run(
        ["git", "-C", str(path), *GIT_CONFIG, *args],
        check=True,
        text=True,
        stdout=subprocess.PIPE,
    )
    return result.stdout.strip()


def commit(path: Path, message: str) -> str:
    git(path, "add", "-A")
    git(path, "commit", "-q", "-m", message)
    return git(path, "rev-parse", "HEAD")


class Repo:
    """A throwaway clone of the template; `revisions` feeds `{name}` placeholders."""

    def __init__(self, path: Path, root: Path, revisions: dict[str, str]) -> None:
        self.path = path
        self.root = root
        self.revisions = dict(revisions)

    @property
    def base(self) -> str:
        return self.revisions["base"]

    @property
    def prose(self) -> str:
        return self.revisions["prose"]

    def git(self, *args: str) -> str:
        return git(self.path, *args)

    def commit(self, message: str) -> str:
        return commit(self.path, message)

    def write_schema(self, extra: str) -> None:
        self.path.joinpath(MODULE, "felis.proto").write_text(
            schema(extra), encoding="utf-8"
        )

    def replace_schema(self, old: str, new: str) -> None:
        path = self.path / MODULE / "felis.proto"
        text = path.read_text(encoding="utf-8")
        assert old in text, f"schema does not contain {old!r}"
        path.write_text(text.replace(old, new), encoding="utf-8")

    def write_ack(self, *lines: str) -> None:
        self.path.joinpath(MODULE, "BREAKING.md").write_text(
            "\n".join(lines) + "\n", encoding="utf-8"
        )

    def write_break(self) -> None:
        self.write_schema("  bool pull_paced = 3;")
        self.replace_schema("string name = 2;", "int64 name = 2;")

    def reset(self, state: str) -> str:
        self.git("reset", "-q", "--hard", self.revisions[state])
        return self.revisions[state]

    def tag(self, name: str) -> None:
        self.git("tag", "-a", "-m", name, name)

    def compat(
        self,
        *args: str,
        environment: dict[str, str] | None = None,
        cwd: Path | None = None,
    ) -> subprocess.CompletedProcess[str]:
        env = os.environ.copy()
        for key in SCRUBBED:
            env.pop(key, None)
        for key, value in (environment or {}).items():
            env[key] = value.format(**self.revisions)
        return subprocess.run(
            [sys.executable, str(COMPAT), *(a.format(**self.revisions) for a in args)],
            cwd=cwd or self.path,
            env=env,
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )


def assert_exit(result: subprocess.CompletedProcess[str], wanted: int) -> None:
    assert result.returncode == wanted, (
        f"exit {result.returncode}, wanted {wanted}\n{result.stdout}"
    )


def assert_compared_with(output: str, revision: str) -> None:
    assert f"proto-compat: comparing against {revision} " in output, (
        f"no 'comparing against {revision}' line\n{output}"
    )


@pytest.fixture(scope="module")
def template(tmp_path_factory: pytest.TempPathFactory) -> tuple[Path, dict[str, str]]:
    """Every shared starting state, each named in `revisions`; `main` stays at prose."""
    root = tmp_path_factory.mktemp("template")
    path = root / "repo"
    path.joinpath(MODULE).mkdir(parents=True)
    git(path, "init", "-q", "-b", "main")
    path.joinpath("crates/felis-protocol/buf.yaml").write_text(
        """version: v2
modules:
  - path: proto
breaking:
  use:
    - WIRE_JSON
""",
        encoding="utf-8",
    )
    repo = Repo(path, root, {})
    repo.write_schema("")
    repo.revisions["base"] = repo.commit("base")
    path.joinpath("README").write_text("notes\n", encoding="utf-8")
    repo.revisions["prose"] = repo.commit("prose only")
    repo.git("remote", "add", "origin", str(path))
    repo.git("fetch", "-q", "origin")

    repo.git("checkout", "-q", "-b", "acknowledged")
    repo.write_schema("  bool pull_paced = 3;")
    repo.revisions["additive"] = repo.commit("additive")
    repo.write_break()
    repo.revisions["retyped"] = repo.commit("retype a field")
    repo.write_ack("# Intended pre-release break", "", f"base: {repo.base}")
    repo.revisions["head_broken"] = repo.commit("acknowledge against the base")

    repo.git("checkout", "-q", "-b", "pr-base", repo.prose)
    repo.write_break()
    repo.revisions["pr_base_tip"] = repo.commit("the same break on the PR base branch")

    repo.git("checkout", "-q", "-b", "main-advanced", repo.prose)
    repo.write_schema("  string cwd = 3;")
    repo.revisions["main_tip"] = repo.commit("a sibling PR's additive field")

    repo.git("checkout", "-q", "main")
    return path, repo.revisions


@pytest.fixture
def repo(template: tuple[Path, dict[str, str]], tmp_path: Path) -> Repo:
    source, revisions = template
    path = tmp_path / "repo"
    shutil.copytree(source, path, symlinks=True)
    return Repo(path, tmp_path, revisions)


@pytest.mark.parametrize(
    ("args", "environment"),
    [
        pytest.param(
            ["{base}"], {}, id="an additive change passes against an explicit base"
        ),
        pytest.param(
            [], {"PROTO_COMPAT_BASE": "{base}"}, id="PROTO_COMPAT_BASE names the base"
        ),
        pytest.param(
            [],
            {},
            id="the local default compares against the merge-base with origin/main",
        ),
    ],
)
def test_additive_change(repo: Repo, args: list[str], environment: dict) -> None:
    repo.reset("additive")
    assert_exit(repo.compat(*args, environment=environment), 0)


@pytest.mark.parametrize(
    "revision",
    [
        pytest.param("{base}", id="a wire-incompatible change fails"),
        pytest.param(
            MISSING_SHA, id="a base missing from the clone fails rather than passes"
        ),
    ],
)
def test_unacknowledged_break(repo: Repo, revision: str) -> None:
    repo.reset("retyped")
    assert_exit(repo.compat(revision), 1)


@pytest.mark.parametrize(
    ("environment", "printed"),
    [
        pytest.param(
            {
                "GITHUB_EVENT_NAME": "pull_request",
                "PROTO_COMPAT_PR_BASE": "{pr_base_tip}",
            },
            "prose",
            id="pull_request fails the break against the merge-base, not the PR base tip",
        ),
        pytest.param(
            {"GITHUB_EVENT_NAME": "push", "PROTO_COMPAT_PUSH_BEFORE": "{base}"},
            "base",
            id="push fails the break against github.event.before",
        ),
        pytest.param(
            {},
            "prose",
            id="the local default fails the break against the merge-base with origin/main",
        ),
    ],
)
def test_event_base_unacknowledged_break(
    repo: Repo, environment: dict[str, str], printed: str
) -> None:
    repo.reset("retyped")
    result = repo.compat(environment=environment)
    assert_exit(result, 1)
    assert_compared_with(result.stdout, repo.revisions[printed])


@pytest.mark.parametrize(
    "revision",
    [
        pytest.param("{base}", id="the acknowledgment naming the base exactly passes"),
        pytest.param(
            "{prose}",
            id="the acknowledgment holds when the base moved by prose-only commits",
        ),
    ],
)
def test_acknowledged_break(repo: Repo, revision: str) -> None:
    repo.reset("head_broken")
    assert_exit(repo.compat(revision), 0)


def test_acknowledgment_line_with_trailing_whitespace_still_covers_the_base(
    repo: Repo,
) -> None:
    repo.reset("head_broken")
    repo.write_ack(f"base: {repo.base} ")
    assert_exit(repo.compat(repo.base), 0)


def test_acknowledgment_naming_a_non_ancestor_with_the_base_schema_does_not_cover_it(
    repo: Repo,
) -> None:
    repo.reset("head_broken")
    repo.git("checkout", "-q", "-b", "side", repo.base)
    repo.path.joinpath("README").write_text("aside\n", encoding="utf-8")
    side = repo.commit("prose only, off the base's history")
    repo.git("checkout", "-q", "main")
    repo.write_ack(f"base: {side}")
    assert_exit(repo.compat(repo.prose), 1)


def test_acknowledgment_the_base_already_carried_does_not_cover_a_later_break(
    repo: Repo,
) -> None:
    repo.write_ack("# Intended pre-release break", "", f"base: {repo.base}")
    inherited_tip = repo.commit("an acknowledgment committed with no break")
    repo.write_break()
    repo.commit("a later break under the inherited acknowledgment")
    result = repo.compat(
        environment={
            "GITHUB_EVENT_NAME": "push",
            "PROTO_COMPAT_PUSH_BEFORE": inherited_tip,
        }
    )
    assert_exit(result, 1)


def test_symlinked_acknowledgment_file_fails_rather_than_covering_a_break(
    repo: Repo,
) -> None:
    repo.path.joinpath(MODULE, "ACK.md").write_text(
        f"base: {repo.base}\n", encoding="utf-8"
    )
    repo.path.joinpath(MODULE, "BREAKING.md").symlink_to("ACK.md")
    symlink_tip = repo.commit("an acknowledgment behind a symlink")
    repo.write_break()
    repo.commit("a break under the symlinked acknowledgment")
    result = repo.compat(
        environment={
            "GITHUB_EVENT_NAME": "push",
            "PROTO_COMPAT_PUSH_BEFORE": symlink_tip,
        }
    )
    assert_exit(result, 1)


@pytest.mark.parametrize(
    ("environment", "printed"),
    [
        pytest.param(
            {"GITHUB_EVENT_NAME": "push", "PROTO_COMPAT_PUSH_BEFORE": "{main_tip}"},
            "main_tip",
            id="push passes on the acknowledgment when main only added fields since its base",
        ),
        pytest.param(
            {"GITHUB_EVENT_NAME": "pull_request", "PROTO_COMPAT_PR_BASE": "{main_tip}"},
            None,
            id="pull_request passes on the acknowledgment after a rebase over the additive field",
        ),
    ],
)
def test_acknowledged_break_after_main_added_a_field(
    repo: Repo, environment: dict[str, str], printed: str | None
) -> None:
    repo.reset("main_tip")
    repo.write_schema("  string cwd = 3;\n  bool pull_paced = 4;")
    repo.replace_schema("string name = 2;", "int64 name = 2;")
    repo.write_ack("# Intended pre-release break", "", f"base: {repo.base}")
    repo.commit("merge of the acknowledged break")
    result = repo.compat(environment=environment)
    assert_exit(result, 0)
    if printed is not None:
        assert_compared_with(result.stdout, repo.revisions[printed])


def test_push_fails_when_the_candidate_breaks_a_field_the_base_introduced_since_the_acknowledged_base(
    repo: Repo,
) -> None:
    main_tip = repo.reset("main_tip")
    repo.write_schema("  bytes cwd = 3;")
    repo.replace_schema("string name = 2;", "int64 name = 2;")
    repo.write_ack("# Intended pre-release break", "", f"base: {repo.base}")
    repo.commit("merge of the acknowledged break, plus an unacknowledged cwd break")
    result = repo.compat(
        environment={"GITHUB_EVENT_NAME": "push", "PROTO_COMPAT_PUSH_BEFORE": main_tip}
    )
    assert_exit(result, 1)


def test_push_fails_when_main_took_an_incompatible_change_since_the_acknowledged_base(
    repo: Repo,
) -> None:
    repo.reset("main_tip")
    repo.replace_schema("uint32 mode = 1;", "bytes mode = 1;")
    main_tip = repo.commit("an unacknowledged break on main")
    repo.write_schema("  string cwd = 3;\n  bool pull_paced = 4;")
    repo.replace_schema("string name = 2;", "int64 name = 2;")
    repo.replace_schema("uint32 mode = 1;", "bytes mode = 1;")
    repo.write_ack("# Intended pre-release break", "", f"base: {repo.base}")
    repo.commit("merge of the acknowledged break onto the broken main")
    result = repo.compat(
        environment={"GITHUB_EVENT_NAME": "push", "PROTO_COMPAT_PUSH_BEFORE": main_tip}
    )
    assert_exit(result, 1)


def test_schema_that_does_not_compile_fails_even_with_the_acknowledgment(
    repo: Repo,
) -> None:
    repo.reset("head_broken")
    repo.write_schema("  bool pull_paced = 3\n")
    assert_exit(repo.compat(repo.base), 1)


def test_buf_execution_error_fails_even_with_the_acknowledgment(repo: Repo) -> None:
    repo.reset("head_broken")
    fake_bin = repo.root / "fakebin"
    fake_bin.mkdir()
    fake_buf = fake_bin / "buf"
    fake_buf.write_text(
        f"#!{sys.executable}\n"
        "import sys\n"
        'print("fake buf: cannot fetch the ref", file=sys.stderr)\n'
        "raise SystemExit(1)\n",
        encoding="utf-8",
    )
    fake_buf.chmod(0o755)
    result = repo.compat(
        repo.base, environment={"PATH": f"{fake_bin}:{os.environ['PATH']}"}
    )
    assert_exit(result, 1)


def test_missing_buf_binary_fails_even_with_the_acknowledgment(repo: Repo) -> None:
    repo.reset("head_broken")
    no_buf = repo.root / "nobuf"
    no_buf.mkdir()
    git_path = shutil.which("git")
    assert git_path is not None, "git not found"
    no_buf.joinpath("git").symlink_to(git_path)
    assert_exit(repo.compat(repo.base, environment={"PATH": str(no_buf)}), 1)


def test_stale_acknowledgment_does_not_cover_a_new_break(repo: Repo) -> None:
    head_broken = repo.reset("head_broken")
    repo.write_break()
    repo.replace_schema("uint32 mode = 1;", "bytes mode = 1;")
    repo.commit("a second break under a stale acknowledgment")
    assert_exit(repo.compat(head_broken), 1)


@pytest.mark.parametrize(
    ("environment", "wanted", "printed"),
    [
        pytest.param(
            {
                "GITHUB_EVENT_NAME": "pull_request",
                "PROTO_COMPAT_PR_BASE": "{pr_base_tip}",
            },
            0,
            "prose",
            id="pull_request passes on the acknowledgment covering the merge-base",
        ),
        pytest.param(
            {"GITHUB_EVENT_NAME": "pull_request"},
            1,
            None,
            id="pull_request without the PR base sha fails",
        ),
        pytest.param(
            {"GITHUB_EVENT_NAME": "push", "PROTO_COMPAT_PUSH_BEFORE": "{base}"},
            0,
            "base",
            id="push passes on the acknowledgment naming github.event.before",
        ),
        pytest.param(
            {},
            0,
            "prose",
            id="the local default passes on the acknowledgment covering the merge-base with origin/main",
        ),
        pytest.param(
            {"GITHUB_EVENT_NAME": "push", "PROTO_COMPAT_PUSH_BEFORE": MISSING_SHA},
            1,
            None,
            id="push with a before sha outside the clone fails",
        ),
        pytest.param(
            {"GITHUB_EVENT_NAME": "push", "PROTO_COMPAT_PUSH_BEFORE": NULL_SHA},
            1,
            None,
            id="push with the null sha fails when no forge is named",
        ),
    ],
)
def test_event_base_acknowledged_break(
    repo: Repo, environment: dict[str, str], wanted: int, printed: str | None
) -> None:
    repo.reset("head_broken")
    result = repo.compat(environment=environment)
    assert_exit(result, wanted)
    if printed is not None:
        assert_compared_with(result.stdout, repo.revisions[printed])


def test_shallow_clone_fails_on_the_unresolvable_base(repo: Repo) -> None:
    repo.reset("head_broken")
    shallow = repo.root / "shallow"
    subprocess.run(
        ["git", "clone", "-q", "--depth", "1", f"file://{repo.path}", str(shallow)],
        check=True,
    )
    assert_exit(repo.compat(repo.base, cwd=shallow), 1)


@pytest.fixture
def run_gate(repo: Repo, monkeypatch: pytest.MonkeyPatch):
    """Run the tag path (or `event`) in-process with the release fetch replaced."""

    def run(
        *, tag: str, listing, event: dict[str, str] | None = None
    ) -> tuple[int, str]:
        def fetch(url: str, credential_origin: str) -> bytes:
            if "/releases?" not in url:
                raise compat.GateError(f"unexpected fetch of {url}")
            if listing is FETCH_ERROR:
                raise compat.GateError(f"cannot reach {url}: fixture refuses")
            if isinstance(listing, bytes):
                return listing
            return json.dumps(listing).encode("utf-8")

        monkeypatch.setattr(compat, "fetch_url", fetch)
        for key in SCRUBBED:
            monkeypatch.delenv(key, raising=False)
        environment = dict(event or {"GITHUB_REF_TYPE": "tag", "GITHUB_REF_NAME": tag})
        environment |= {"GITHUB_SERVER_URL": SERVER, "GITHUB_REPOSITORY": REPOSITORY}
        for key, value in environment.items():
            monkeypatch.setenv(key, value)
        monkeypatch.chdir(repo.path)
        stream = CapturedStream()
        with contextlib.redirect_stdout(stream), contextlib.redirect_stderr(stream):
            status = compat.main([])
        return status, stream.getvalue()

    return run


def assert_gate_exit(outcome: tuple[int, str], wanted: int) -> None:
    status, output = outcome
    assert status == wanted, f"exit {status}, wanted {wanted}\n{output}"


@pytest.mark.parametrize(
    ("tag", "listing", "wanted"),
    [
        pytest.param(
            "v0.1.0", [], 0, id="a tag build with no published release passes"
        ),
        pytest.param(
            "v0.1.0",
            FETCH_ERROR,
            1,
            id="a tag build fails when the release listing cannot be fetched",
        ),
        pytest.param(
            "v0.1.0",
            b"<html>forbidden</html>",
            1,
            id="a tag build fails when the release listing is not JSON",
        ),
        pytest.param(
            "v0.1.0",
            {"message": "token required"},
            1,
            id="a tag build fails when the listing is not an array of releases",
        ),
        pytest.param(
            "v0.2.0",
            [release("v0.1.0")],
            1,
            id="a published release whose tag is not in the clone fails",
        ),
    ],
)
def test_tag_build_before_the_release_is_tagged(
    repo: Repo, run_gate, tag: str, listing, wanted: int
) -> None:
    repo.reset("head_broken")
    assert_gate_exit(run_gate(tag=tag, listing=listing), wanted)


def test_tag_build_passes_against_its_own_released_schema(repo: Repo, run_gate) -> None:
    repo.reset("head_broken")
    repo.tag("v0.1.0")
    assert_gate_exit(run_gate(tag="v0.1.0", listing=[release("v0.1.0")]), 0)


NULL_SHA_PUSH = {"GITHUB_EVENT_NAME": "push", "PROTO_COMPAT_PUSH_BEFORE": NULL_SHA}


@pytest.mark.parametrize(
    ("tag", "event"),
    [
        pytest.param(
            "v0.2.0", None, id="an additive change passes against the released schema"
        ),
        pytest.param(
            "v0.2.0-rc.1",
            None,
            id="a prerelease tag is compared with the last final release",
        ),
        pytest.param(
            "",
            NULL_SHA_PUSH,
            id="a null-sha push is compared with the newest published release",
        ),
    ],
)
def test_additive_change_after_the_release(
    repo: Repo, run_gate, tag: str, event: dict[str, str] | None
) -> None:
    repo.reset("head_broken")
    repo.tag("v0.1.0")
    repo.write_schema("  bool pull_paced = 3;\n  string cwd = 4;")
    repo.replace_schema("string name = 2;", "int64 name = 2;")
    repo.commit("additive after the release")
    outcome = run_gate(tag=tag, listing=[release("v0.1.0")], event=event)
    assert_gate_exit(outcome, 0)


@pytest.mark.parametrize(
    ("tag", "event"),
    [
        pytest.param(
            "v0.2.0",
            None,
            id="a tag build ignores the acknowledgment and fails a post-release break",
        ),
        pytest.param(
            "", NULL_SHA_PUSH, id="a null-sha push fails a post-release break too"
        ),
    ],
)
def test_acknowledged_break_after_the_release(
    repo: Repo, run_gate, tag: str, event: dict[str, str] | None
) -> None:
    repo.reset("head_broken")
    repo.tag("v0.1.0")
    repo.write_schema("  bool pull_paced = 3;\n  string cwd = 4;")
    repo.replace_schema("string name = 2;", "int64 name = 2;")
    repo.commit("additive after the release")
    repo.replace_schema("uint32 mode = 1;", "bytes mode = 1;")
    repo.write_ack(f"base: {repo.git('rev-parse', 'HEAD')}")
    repo.commit("break after the release, acknowledged")
    outcome = run_gate(tag=tag, listing=[release("v0.1.0")], event=event)
    assert_gate_exit(outcome, 1)


TOKEN = "not-a-real-token"


@pytest.fixture(scope="module")
def servers():
    forge = Forge()
    elsewhere = Forge()
    forge.redirect_to = elsewhere.origin + "/api/v1/whatever"
    try:
        yield forge, elsewhere
    finally:
        forge.close()
        elsewhere.close()


@pytest.fixture
def forges(servers, monkeypatch: pytest.MonkeyPatch):
    forge, elsewhere = servers
    for server in servers:
        server.seen.clear()
    forge.redirect_all = False
    monkeypatch.setattr(compat, "FETCH_TIMEOUT", 1.0)
    monkeypatch.setenv("FORGEJO_TOKEN", TOKEN)
    monkeypatch.setenv("GITHUB_SERVER_URL", forge.origin)
    monkeypatch.setenv("GITHUB_REPOSITORY", REPOSITORY)
    return forge, elsewhere


def test_a_200_from_the_configured_origin_is_read(forges) -> None:
    forge, _ = forges
    assert compat.fetch_url(forge.origin + "/ok", forge.origin) == b"payload"


def test_the_token_is_sent_to_the_configured_origin(forges) -> None:
    forge, _ = forges
    compat.fetch_url(forge.origin + "/ok", forge.origin)
    assert forge.seen[-1][1] == f"token {TOKEN}"


def test_no_token_is_sent_to_any_other_origin(forges) -> None:
    forge, elsewhere = forges
    compat.fetch_url(elsewhere.origin + "/ok", forge.origin)
    assert elsewhere.seen[-1][1] is None


@pytest.mark.parametrize(
    "url",
    [
        pytest.param("{origin}/boom", id="a non-2xx answer fails"),
        pytest.param("file:///etc/passwd", id="a url that is not http(s) fails"),
        pytest.param(
            "{origin}/slow", id="a server that never answers fails on the timeout"
        ),
    ],
)
def test_fetch_failure(forges, url: str) -> None:
    forge, _ = forges
    with pytest.raises(compat.GateError):
        compat.fetch_url(url.format(origin=forge.origin), forge.origin)


def test_a_redirected_release_listing_fails_instead_of_answering_from_elsewhere(
    forges,
) -> None:
    forge, _ = forges
    forge.redirect_all = True
    with pytest.raises(compat.GateError):
        compat.CompatibilityGate().published_release()


def test_a_refused_redirect_never_reaches_the_other_origin(forges) -> None:
    forge, elsewhere = forges
    forge.redirect_all = True
    with pytest.raises(compat.GateError):
        compat.CompatibilityGate().published_release()
    assert elsewhere.seen == []


def test_an_empty_release_listing_over_http_means_no_release_is_published(
    forges,
) -> None:
    assert compat.CompatibilityGate().published_release() is None


def test_buf_lint_passes_on_the_committed_module() -> None:
    result = subprocess.run(
        ["buf", "lint"],
        cwd=REPO_ROOT / "crates/felis-protocol",
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )
    assert_exit(result, 0)
