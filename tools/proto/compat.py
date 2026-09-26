#!/usr/bin/env python3
from __future__ import annotations

import json
import os
import re
import subprocess
import sys
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

USAGE = """usage: tools/proto/compat.py [<base revision>]
  --self-test   run the hermetic exit-code suite (tools/proto/compat_test.py)
"""
NULL_SHA = "0" * 40
ACK_SHA = re.compile(r"base: ([0-9a-f]{40})\s*")
POSITION_FIELDS = re.compile(
    rb'"start_line":[0-9]+,"start_column":[0-9]+,'
    rb'"end_line":[0-9]+,"end_column":[0-9]+,'
)


RELEASE_LISTING = (
    "/api/v1/repos/{repository}/releases?draft=false&pre-release=false&limit=1"
)
# Forgejo answers urllib's default user agent with 403.
USER_AGENT = "felis-proto-compat"
FETCH_TIMEOUT = 30.0


class GateError(Exception):
    pass


def origin_of(url: str) -> str:
    parts = urllib.parse.urlsplit(url)
    if parts.scheme not in {"http", "https"} or not parts.netloc:
        raise GateError(f"{url} is not an http(s) url")
    return f"{parts.scheme}://{parts.netloc}"


class RefuseRedirects(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):  # type: ignore[no-untyped-def]
        raise GateError(
            f"{req.full_url} redirects to {newurl}; only the configured forge "
            "may answer whether a release is published"
        )


def fetch_url(url: str, credential_origin: str) -> bytes:
    """Read a URL, or raise. The self-test replaces this to stay offline."""
    headers = {"User-Agent": USER_AGENT, "Accept": "*/*"}
    token = os.environ.get("FORGEJO_TOKEN")
    if token and origin_of(url) == credential_origin:
        headers["Authorization"] = f"token {token}"
    opener = urllib.request.build_opener(RefuseRedirects())
    try:
        with opener.open(
            urllib.request.Request(url, headers=headers), timeout=FETCH_TIMEOUT
        ) as response:
            return response.read()
    except urllib.error.HTTPError as error:
        raise GateError(f"{url} answered HTTP {error.code}") from error
    except (urllib.error.URLError, OSError, ValueError) as error:
        raise GateError(f"cannot reach {url}: {error}") from error


def run(
    args: list[str],
    *,
    check: bool = True,
    stderr: int | None = None,
    stdout: int | None = subprocess.PIPE,
    input_bytes: bytes | None = None,
) -> subprocess.CompletedProcess[bytes]:
    try:
        result = subprocess.run(
            args,
            check=False,
            input=input_bytes,
            stdout=stdout,
            stderr=stderr,
        )
    except OSError as error:
        raise GateError(f"cannot run {args[0]}: {error}") from error
    if check and result.returncode != 0:
        raise GateError(f"{' '.join(args)} exited {result.returncode}")
    return result


def output(args: list[str]) -> str:
    return run(args).stdout.decode().strip()


class CompatibilityGate:
    def __init__(self) -> None:
        self.module_dir = Path(
            os.environ.get("PROTO_COMPAT_MODULE_DIR", "crates/felis-protocol/proto")
        )
        self.buf_module_dir = os.environ.get(
            "PROTO_COMPAT_BUF_MODULE_DIR", "crates/felis-protocol"
        )
        self.ack_file = self.module_dir / "BREAKING.md"
        self.buf_diag = b""
        self.candidate_base_diag = b""

    def ack_lines(self) -> list[str]:
        if not self.ack_file.is_file():
            return []
        try:
            lines = self.ack_file.read_text(encoding="utf-8").splitlines()
        except OSError as error:
            raise GateError(f"cannot read {self.ack_file}") from error
        return [line for line in lines if ACK_SHA.fullmatch(line)]

    def ack_shas_at(self, revision: str) -> set[str]:
        result = run(
            ["git", "ls-tree", "--full-tree", revision, "--", str(self.ack_file)],
            check=False,
            stderr=subprocess.PIPE,
        )
        if result.returncode != 0:
            raise GateError(f"cannot read {self.ack_file} out of {revision}")
        entry = result.stdout.decode().strip()
        if not entry:
            return set()
        fields = entry.split(maxsplit=3)
        if len(fields) < 3:
            raise GateError(f"cannot parse {self.ack_file} out of {revision}")
        mode, object_type, object_id = fields[:3]
        if (mode, object_type) not in {("100644", "blob"), ("100755", "blob")}:
            raise GateError(
                f"{self.ack_file} at {revision} is a {mode} {object_type}, not a "
                "regular file; the acknowledgment must be a plain committed file"
            )
        blob = run(["git", "cat-file", "blob", object_id]).stdout.decode()
        return {
            match.group(1)
            for line in blob.splitlines()
            if (match := ACK_SHA.fullmatch(line))
        }

    def commit_exists(self, revision: str) -> bool:
        result = run(
            ["git", "cat-file", "-e", f"{revision}^{{commit}}"],
            check=False,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        return result.returncode == 0

    def is_ancestor(self, ancestor: str, descendant: str) -> bool:
        result = run(
            ["git", "merge-base", "--is-ancestor", ancestor, descendant],
            check=False,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        return result.returncode == 0

    def buf_breaking_against(self, against: str, input_dir: str | None = None) -> int:
        source = input_dir or self.buf_module_dir
        try:
            result = subprocess.run(
                [
                    "buf",
                    "breaking",
                    source,
                    "--against",
                    against,
                    "--error-format=json",
                ],
                check=False,
                input=b"",
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
            )
            status = result.returncode
            self.buf_diag = result.stdout
        except FileNotFoundError:
            status = 127
            self.buf_diag = b""
        except OSError as error:
            raise GateError(f"cannot run buf: {error}") from error
        if status == 0:
            return 0
        sys.stderr.buffer.write(self.buf_diag)
        if status != 100:
            raise GateError(
                f"buf breaking exited {status} without comparing {source} against {against}"
            )
        if b'"type":"COMPILE"' in self.buf_diag:
            raise GateError(
                f"{source} (or the schema at {against}) does not compile; nothing was compared"
            )
        return 100

    @staticmethod
    def diagnostic_identity(diagnostics: bytes) -> set[bytes]:
        return {POSITION_FIELDS.sub(b"", line) for line in diagnostics.splitlines()}

    def candidate_only_breaks_sha(self, revision: str) -> bool:
        if self.buf_breaking_against(f".git#ref={revision}") != 100:
            return False
        return self.diagnostic_identity(self.candidate_base_diag).issubset(
            self.diagnostic_identity(self.buf_diag)
        )

    def ack_covers_base(self, base: str) -> bool:
        inherited = self.ack_shas_at(base)
        for line in self.ack_lines():
            match = ACK_SHA.fullmatch(line)
            if match is None:
                continue
            revision = match.group(1)
            if revision in inherited:
                continue
            if revision == base:
                return True
            if not self.commit_exists(revision) or not self.is_ancestor(revision, base):
                continue
            if (
                self.buf_breaking_against(f".git#ref={revision}", f".git#ref={base}")
                != 0
            ):
                continue
            if self.candidate_only_breaks_sha(revision):
                return True
        return False

    def require_commit(self, revision: str) -> None:
        if not self.commit_exists(revision):
            raise GateError(
                f"base revision {revision} is not in this clone (shallow checkout? "
                "unfetched ref?); refusing to pass without a comparison"
            )

    def compare_with_git_base(self, revision: str, why: str) -> None:
        self.require_commit(revision)
        base = output(["git", "rev-parse", f"{revision}^{{commit}}"])
        print(f"proto-compat: comparing against {base} ({why})")
        if self.buf_breaking_against(f".git#ref={base}") == 0:
            print("proto-compat: wire-compatible with the base")
            return
        self.candidate_base_diag = self.buf_diag
        if self.ack_covers_base(base):
            print(
                f"proto-compat: break acknowledged in {self.ack_file} against {base} "
                "(pre-release, rides the current major)"
            )
            return
        raise GateError(
            f"felis.proto is wire-incompatible with {base} and {self.ack_file} "
            "does not acknowledge a break against it"
        )

    def forge(self) -> tuple[str, str]:
        server = os.environ.get("GITHUB_SERVER_URL") or os.environ.get(
            "FORGEJO_SERVER_URL"
        )
        repository = os.environ.get("GITHUB_REPOSITORY")
        if not server or not repository:
            raise GateError(
                "GITHUB_SERVER_URL and GITHUB_REPOSITORY must name the repository "
                "whose releases decide which schema a tag answers to"
            )
        return server.rstrip("/"), repository

    def published_release(self) -> str | None:
        server, repository = self.forge()
        url = server + RELEASE_LISTING.format(repository=repository)
        payload = fetch_url(url, origin_of(server))
        try:
            listing = json.loads(payload.decode("utf-8"))
        except (UnicodeError, ValueError) as error:
            raise GateError(f"{url} did not answer with JSON: {error}") from error
        if not isinstance(listing, list):
            raise GateError(f"{url} answered with {type(listing).__name__}, not a list")
        if not listing:
            return None
        release = listing[0]
        if not isinstance(release, dict):
            raise GateError(f"{url} listed a release that is not an object")
        tag = release.get("tag_name")
        if not isinstance(tag, str) or not tag.strip():
            raise GateError(f"{url} listed a release with no tag_name")
        return tag.strip()

    def compare_with_release(self, why: str) -> None:
        tag = self.published_release()
        if tag is None:
            print(
                f"proto-compat: no final release is published, so no schema is "
                f"owed compatibility yet ({why})"
            )
            return
        if not self.commit_exists(f"refs/tags/{tag}"):
            raise GateError(
                f"{tag} is the newest published release but its tag is not in this "
                "clone (shallow checkout? tags not fetched?); refusing to pass "
                "without a comparison"
            )
        released = output(["git", "rev-parse", f"refs/tags/{tag}^{{commit}}"])
        print(f"proto-compat: comparing against the schema released as {tag} ({why})")
        if self.buf_breaking_against(f".git#ref={released}") != 0:
            raise GateError(
                f"felis.proto is wire-incompatible with the schema released as {tag}; "
                "a post-release break requires a PROTOCOL_MAJOR bump"
            )
        print(f"proto-compat: wire-compatible with {tag}")

    def execute(self, explicit_base: str | None) -> None:
        result = run(
            ["git", "rev-parse", "--is-inside-work-tree"],
            check=False,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        if result.returncode != 0:
            raise GateError("not inside a git work tree; the comparison needs history")
        if self.ack_file.is_symlink():
            raise GateError(
                f"{self.ack_file} is a symlink; the acknowledgment must be a regular file"
            )

        base = explicit_base or os.environ.get("PROTO_COMPAT_BASE")
        if base:
            self.compare_with_git_base(base, "explicit base")
            return
        if os.environ.get("GITHUB_REF_TYPE") == "tag":
            self.compare_with_release(
                f"tag build {os.environ.get('GITHUB_REF_NAME', '')}"
            )
            return

        event = os.environ.get("GITHUB_EVENT_NAME")
        if event == "pull_request":
            pr_base = os.environ.get("PROTO_COMPAT_PR_BASE")
            if not pr_base:
                raise GateError(
                    "pull_request event without PROTO_COMPAT_PR_BASE "
                    "(github.event.pull_request.base.sha)"
                )
            self.require_commit(pr_base)
            result = run(
                ["git", "merge-base", "HEAD", pr_base],
                check=False,
                stderr=subprocess.PIPE,
            )
            if result.returncode != 0:
                raise GateError(f"HEAD and the PR base {pr_base} share no history")
            merge_base = result.stdout.decode().strip()
            self.compare_with_git_base(
                merge_base, f"merge-base of HEAD and the PR base {pr_base}"
            )
            return
        if event == "push":
            before = os.environ.get("PROTO_COMPAT_PUSH_BEFORE")
            if not before:
                raise GateError(
                    "push event without PROTO_COMPAT_PUSH_BEFORE (github.event.before)"
                )
            if before == NULL_SHA:
                self.compare_with_release(
                    "push with no prior revision (github.event.before is the null sha)"
                )
            else:
                self.compare_with_git_base(before, "github.event.before")
            return

        origin = run(
            ["git", "rev-parse", "--verify", "--quiet", "origin/main"],
            check=False,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        if origin.returncode != 0:
            raise GateError(
                "origin/main is not fetched; pass the base explicitly "
                "(just proto-compat <rev>)"
            )
        result = run(
            ["git", "merge-base", "HEAD", "origin/main"],
            check=False,
            stderr=subprocess.PIPE,
        )
        if result.returncode != 0:
            raise GateError("HEAD and origin/main share no history")
        self.compare_with_git_base(
            result.stdout.decode().strip(), "merge-base of HEAD and origin/main"
        )


def main(argv: list[str]) -> int:
    if argv and argv[0] in {"-h", "--help"}:
        print(USAGE, file=sys.stderr, end="")
        return 64
    if argv and argv[0] == "--self-test":
        if len(argv) != 1:
            print(USAGE, file=sys.stderr, end="")
            return 64
        test = Path(__file__).with_name("compat_test.py")
        os.execv(sys.executable, [sys.executable, str(test)])
    if len(argv) > 1:
        print(USAGE, file=sys.stderr, end="")
        return 64
    try:
        CompatibilityGate().execute(argv[0] if argv else None)
    except (GateError, OSError, UnicodeError) as error:
        print(f"proto-compat: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
