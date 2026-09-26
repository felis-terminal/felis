#!/usr/bin/env python3
from __future__ import annotations

import json
import os
import re
import subprocess
import sys
import tomllib
from pathlib import Path

USAGE = """usage: tools/release/verify.py <tag> [<commit>]
       tools/release/verify.py --identity [--bundled-client] <tag> <commit> <version-json>
       tools/release/verify.py --self-test
"""
TAG_RE = re.compile(
    r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-rc\.[1-9][0-9]*)?"
)


class VerifyError(Exception):
    pass


def command(args: list[str], *, check: bool = True) -> subprocess.CompletedProcess[str]:
    try:
        result = subprocess.run(
            args,
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )
    except OSError as error:
        raise VerifyError(f"cannot run {args[0]}: {error}") from error
    if check and result.returncode != 0:
        raise VerifyError(f"{' '.join(args)} exited {result.returncode}")
    return result


def git_output(*args: str) -> str:
    return command(["git", *args]).stdout.strip()


def note(message: str) -> None:
    print(f"release-verify: {message}")


def check_tag_name(tag: str) -> None:
    if TAG_RE.fullmatch(tag) is None:
        raise VerifyError(
            f"tag {tag} is not `v<semver>` with an optional `-rc.<n>` suffix"
        )


def core_of(tag: str) -> str:
    return tag.removeprefix("v").split("-", maxsplit=1)[0]


def load_toml(path: Path) -> dict:
    try:
        with path.open("rb") as handle:
            return tomllib.load(handle)
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise VerifyError(f"cannot read {path}: {error}") from error


def check_manifests(core: str) -> None:
    workspace = load_toml(Path("Cargo.toml"))
    try:
        found = workspace["workspace"]["package"]["version"]
    except (KeyError, TypeError) as error:
        raise VerifyError(
            "Cargo.toml declares no [workspace.package] version"
        ) from error
    if found != core:
        raise VerifyError(f"Cargo.toml says {found}, tag says {core}")

    pinned: list[Path] = []
    for manifest in sorted(Path("crates").glob("*/Cargo.toml")):
        package = load_toml(manifest).get("package", {})
        if package.get("version") != {"workspace": True}:
            pinned.append(manifest)
    for manifest in pinned:
        print(
            f"release-verify: {manifest} does not inherit the workspace version",
            file=sys.stderr,
        )
    if pinned:
        raise VerifyError("every crate must take its version from [workspace.package]")
    note(f"the workspace is at {core} and every crate inherits it")


def check_changelog(core: str) -> None:
    try:
        changelog = Path("CHANGELOG.md").read_text(encoding="utf-8")
    except OSError as error:
        raise VerifyError("cannot read CHANGELOG.md") from error
    heading = re.compile(
        rf"^## \[{re.escape(core)}\] - [0-9]{{4}}-[0-9]{{2}}-[0-9]{{2}}", re.M
    )
    if heading.search(changelog) is None:
        raise VerifyError(f"CHANGELOG.md has no `## [{core}] - <date>` section")
    note(f"CHANGELOG.md carries the {core} section")


def resolve_commit(revision: str) -> str:
    result = command(
        ["git", "rev-parse", "--verify", f"{revision}^{{commit}}"], check=False
    )
    if result.returncode != 0:
        raise VerifyError(f"{revision} does not name a commit")
    return result.stdout.strip()


def verify_source(tag: str, revision: str) -> None:
    check_tag_name(tag)
    core = core_of(tag)

    tag_type = command(["git", "cat-file", "-t", f"refs/tags/{tag}"], check=False)
    if tag_type.returncode != 0:
        raise VerifyError(
            f"no tag object named {tag} in this clone (unfetched tag? shallow checkout?)"
        )
    object_type = tag_type.stdout.strip()
    if object_type != "tag":
        raise VerifyError(
            f"{tag} is a {object_type}, not an annotated tag; a release tag carries "
            "a tagger and a message"
        )

    points_at = resolve_commit(f"refs/tags/{tag}")
    commit = resolve_commit(revision)
    if points_at != commit:
        raise VerifyError(
            f"{tag} points at {points_at}, but the release commit is {commit}"
        )
    note(f"{tag} is an annotated tag at {commit}")

    check_manifests(core)
    if "-" in tag:
        note(f"{tag} is a release candidate; the CHANGELOG section is not owed yet")
    else:
        check_changelog(core)

    if git_output("status", "--porcelain"):
        raise VerifyError(
            "the working tree has uncommitted changes; a release is cut from a clean tree"
        )
    note("the working tree is clean")


def verify_identity(
    tag: str, revision: str, identity_path: str, *, bundled_client: bool = False
) -> None:
    check_tag_name(tag)
    core = core_of(tag)
    commit = resolve_commit(revision)
    try:
        with Path(identity_path).open(encoding="utf-8") as handle:
            report = json.load(handle)
        cli = report["cli"]
        version = cli["version"]
        artifact_revision = cli["revision"]
        dirty = cli["dirty"]
        if not isinstance(version, str) or not isinstance(artifact_revision, str):
            raise TypeError
        if not isinstance(dirty, bool):
            raise TypeError
    except (OSError, KeyError, TypeError, json.JSONDecodeError) as error:
        raise VerifyError(
            f"{identity_path} is not a `felis version --format json` object"
        ) from error

    if version != core:
        raise VerifyError(
            f"the artifact reports version {version}, the tag says {core}"
        )
    if artifact_revision != commit:
        raise VerifyError(
            f"the artifact was built from {artifact_revision}, the tag points at {commit}"
        )
    if dirty:
        raise VerifyError(
            "the artifact was built from a dirty tree; a release identity must name "
            "its source exactly"
        )
    note(f"the artifact reports {version} ({artifact_revision}), clean")
    if bundled_client:
        check_bundled_client(report, identity_path)


def check_bundled_client(report: dict, identity_path: str) -> None:
    try:
        status = report["client_status"]
        client = report["client"]
    except KeyError as error:
        raise VerifyError(
            f"{identity_path} carries no `client` or `client_status`"
        ) from error
    if status != "ok":
        raise VerifyError(f"the bundled client is {status}, not ok")
    if client != report["cli"]:
        raise VerifyError(
            f"the bundled client is {client}, not the CLI's {report['cli']}"
        )
    note("the bundled client reports the same identity")


def main(argv: list[str]) -> int:
    if argv and argv[0] in {"-h", "--help"}:
        print(USAGE, file=sys.stderr, end="")
        return 64
    if argv == ["--self-test"]:
        test = Path(__file__).with_name("verify_test.py")
        os.execv(sys.executable, [sys.executable, str(test)])

    identity = bool(argv and argv[0] == "--identity")
    bundled_client = identity and argv[1:2] == ["--bundled-client"]
    if bundled_client:
        argv = [argv[0], *argv[2:]]
    source = bool(argv and not argv[0].startswith("-") and len(argv) <= 2)
    if (identity and len(argv) != 4) or not (identity or source):
        print(USAGE, file=sys.stderr, end="")
        return 64

    try:
        if identity:
            verify_identity(argv[1], argv[2], argv[3], bundled_client=bundled_client)
        else:
            verify_source(argv[0], argv[1] if len(argv) == 2 else "HEAD")
    except (VerifyError, UnicodeError) as error:
        print(f"release-verify: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
