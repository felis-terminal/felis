#!/usr/bin/env python3
from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
import tomllib
import urllib.parse
from pathlib import Path

from verify import BINARY_CRATES

USAGE = """usage: tools/release/crates.py check
       tools/release/crates.py publish-plan
       tools/release/crates.py --self-test

check: refuses a tree whose library versions are not ready to publish: a
changed crate left unbumped (`release-plz update` would edit the tree), a bump
smaller than `cargo semver-checks` asks for, or an in-workspace version
requirement that is not the version its crate carries.

publish-plan: prints, in workspace order, the crates whose local version is not
on crates.io, one per line. Fails when one of them was never published, since
trusted publishing cannot create a crate.

FELIS_CRATES_INDEX overrides the sparse index base (default
https://index.crates.io).
"""
INDEX = "https://index.crates.io"
SEMVER_CHECKED_BINARY = "felis-daemon"


class CratesError(Exception):
    pass


def note(message: str) -> None:
    print(f"release-crates: {message}", flush=True)


def run(args: list[str], *, cwd: Path | None = None) -> None:
    try:
        result = subprocess.run(args, cwd=cwd, check=False)
    except OSError as error:
        raise CratesError(f"cannot run {args[0]}: {error}") from error
    if result.returncode != 0:
        raise CratesError(f"{' '.join(args)} exited {result.returncode}")


def output(args: list[str], *, cwd: Path | None = None) -> str:
    try:
        result = subprocess.run(
            args, cwd=cwd, check=False, text=True, stdout=subprocess.PIPE
        )
    except OSError as error:
        raise CratesError(f"cannot run {args[0]}: {error}") from error
    if result.returncode != 0:
        raise CratesError(f"{' '.join(args)} exited {result.returncode}")
    return result.stdout


def load_toml(path: Path) -> dict:
    try:
        with path.open("rb") as handle:
            return tomllib.load(handle)
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise CratesError(f"cannot read {path}: {error}") from error


class Crate:
    def __init__(self, manifest: Path, data: dict, workspace_version: str) -> None:
        package = data.get("package", {})
        self.manifest = manifest
        self.data = data
        self.name: str = package.get("name", "")
        version = package.get("version")
        self.version: str = (
            workspace_version if version == {"workspace": True} else str(version)
        )
        self.publishable = package.get("publish", True) is not False


def workspace(root: Path) -> tuple[dict, list[Crate]]:
    data = load_toml(root / "Cargo.toml")
    try:
        version = data["workspace"]["package"]["version"]
    except (KeyError, TypeError) as error:
        raise CratesError(
            "Cargo.toml declares no [workspace.package] version"
        ) from error
    crates = [
        Crate(manifest, load_toml(manifest), version)
        for manifest in sorted((root / "crates").glob("*/Cargo.toml"))
    ]
    return data, crates


def dependency_tables(data: dict) -> list[tuple[str, dict]]:
    tables = [
        (kind, data.get(kind, {}))
        for kind in ("dependencies", "build-dependencies", "dev-dependencies")
    ]
    for target, section in data.get("target", {}).items():
        for kind in ("dependencies", "build-dependencies", "dev-dependencies"):
            if kind in section:
                tables.append((f"target.{target}.{kind}", section[kind]))
    return tables


def check_requirements(root: Path) -> list[str]:
    data, crates = workspace(root)
    versions = {crate.name: crate.version for crate in crates}
    problems: list[str] = []
    inherited = data.get("workspace", {}).get("dependencies", {})
    for name, entry in inherited.items():
        if name not in versions:
            continue
        wanted = versions[name]
        found = entry.get("version") if isinstance(entry, dict) else entry
        if found != wanted:
            problems.append(
                f"[workspace.dependencies] {name} requires {found}, the crate is at {wanted}"
            )
    for crate in crates:
        for table, deps in dependency_tables(crate.data):
            for name, entry in deps.items():
                if name not in versions:
                    continue
                where = f"{crate.manifest} [{table}] {name}"
                if table.endswith("dev-dependencies"):
                    if (
                        not isinstance(entry, dict)
                        or "path" not in entry
                        or "version" in entry
                        or "workspace" in entry
                    ):
                        problems.append(
                            f"{where} must name `path` alone, so cargo package drops it"
                        )
                elif not (isinstance(entry, dict) and entry.get("workspace") is True):
                    problems.append(
                        f"{where} must inherit [workspace.dependencies] (`workspace = true`)"
                    )
    return problems


def published_versions(name: str) -> set[str] | None:
    base = os.environ.get("FELIS_CRATES_INDEX", INDEX).rstrip("/")
    lower = name.lower()
    if len(lower) <= 2:
        path = f"{len(lower)}/{lower}"
    elif len(lower) == 3:
        path = f"3/{lower[0]}/{lower}"
    else:
        path = f"{lower[:2]}/{lower[2:4]}/{lower}"
    if base.startswith("file:"):
        local = Path(urllib.parse.urlparse(f"{base}/{path}").path)
        if not local.is_file():
            return None
        lines = local.read_text(encoding="utf-8").splitlines()
    else:
        with tempfile.TemporaryDirectory() as scratch:
            body = Path(scratch) / "index"
            try:
                result = subprocess.run(
                    [
                        "curl",
                        "-sS",
                        "-o",
                        str(body),
                        "-w",
                        "%{http_code}",
                        f"{base}/{path}",
                    ],
                    stdout=subprocess.PIPE,
                    text=True,
                    check=False,
                )
            except OSError as error:
                raise CratesError(f"cannot run curl: {error}") from error
            if result.returncode != 0:
                raise CratesError(f"curl {base}/{path} exited {result.returncode}")
            status = result.stdout.strip()
            if status == "404":
                return None
            if status != "200":
                raise CratesError(f"the index answered {status} for {name}")
            lines = body.read_text(encoding="utf-8").splitlines()
    return {json.loads(line)["vers"] for line in lines if line.strip()}


def publish_plan(root: Path) -> list[str]:
    _, crates = workspace(root)
    pending: list[str] = []
    never: list[str] = []
    for crate in crates:
        if not crate.publishable:
            continue
        versions = published_versions(crate.name)
        if versions is None:
            never.append(crate.name)
        elif crate.version not in versions:
            pending.append(crate.name)
    if never:
        raise CratesError(
            f"never published, so trusted publishing cannot create them: {', '.join(never)}; "
            "publish these by hand first"
        )
    return pending


def check(root: Path) -> None:
    problems = check_requirements(root)
    for problem in problems:
        print(f"release-crates: {problem}", file=sys.stderr)
    if problems:
        raise CratesError("in-workspace version requirements disagree with the crates")
    note("every in-workspace requirement names its crate's version")

    _, crates = workspace(root)
    checked = [
        crate
        for crate in crates
        if crate.publishable
        and (crate.name not in BINARY_CRATES or crate.name == SEMVER_CHECKED_BINARY)
        and published_versions(crate.name)
    ]
    for crate in checked:
        run(["cargo", "semver-checks", "--package", crate.name], cwd=root)
    note(
        f"semver-checks passed for {', '.join(c.name for c in checked) or 'no published crate'}"
    )

    with tempfile.TemporaryDirectory() as scratch:
        tree = Path(scratch) / "tree"
        run(
            [
                "git",
                "-C",
                str(root),
                "worktree",
                "add",
                "--quiet",
                "--detach",
                str(tree),
                "HEAD",
            ]
        )
        try:
            run(["release-plz", "update"], cwd=tree)
            changed = output(["git", "status", "--porcelain"], cwd=tree)
            if changed:
                print(output(["git", "diff"], cwd=tree), file=sys.stderr)
                raise CratesError(
                    "release-plz update would still change the tree; run `just release-prepare` "
                    "and commit the result"
                )
        finally:
            run(["git", "-C", str(root), "worktree", "remove", "--force", str(tree)])
            shutil.rmtree(tree, ignore_errors=True)
    note("release-plz update leaves the tree unchanged")


def main(argv: list[str]) -> int:
    if argv == ["--self-test"]:
        test = Path(__file__).with_name("crates_test.py")
        os.execv(sys.executable, [sys.executable, str(test)])
    if len(argv) != 1 or argv[0] not in {"check", "publish-plan"}:
        print(USAGE, file=sys.stderr, end="")
        return 64
    root = Path.cwd()
    try:
        if argv[0] == "check":
            check(root)
        else:
            for name in publish_plan(root):
                print(name)
    except (CratesError, UnicodeError) as error:
        print(f"release-crates: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
