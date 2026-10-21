#!/usr/bin/env python3
from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import urllib.parse
from pathlib import Path

from verify import BINARY_CRATES

USAGE = """usage: tools/release/crates.py check
       tools/release/crates.py publish-plan
       tools/release/crates.py package-tests
       tools/release/crates.py publish-dry-run

check: refuses a tree whose library versions are not ready to publish: a
changed crate left unbumped (`release-plz update` would edit the tree), a bump
smaller than `cargo semver-checks` asks for, or an in-workspace version
requirement that is not the version its crate carries.

publish-plan: prints, in workspace order, the crates whose local version is not
on crates.io, one per line. Fails when one of them was never published, since
trusted publishing cannot create a crate.

package-tests: packages every publishable crate and builds each package's
tests from the extracted tree, the way a crates.io consumer would, with the
other felis packages patched in for their unpublished versions.

publish-dry-run: runs `cargo publish --workspace --dry-run`, which packages
every publishable crate and builds each against the others, from a fresh
unpack of this tree's packages.

FELIS_CRATES_INDEX overrides the sparse index base (default
https://index.crates.io).
"""
INDEX = "https://index.crates.io"
SEMVER_CHECKED_BINARY = "felis-daemon"
# The one dev-dependency edge that stays `path` alone: with a version on it,
# felis-client-core and felis-daemon (which dev-depends back) could not be
# packaged before each other. cargo package drops it, along with the
# self-dependency that turns on the tests needing it.
PATH_ONLY_DEV_DEPENDENCY = ("felis-client-core", "felis-daemon")


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
                path_only = isinstance(entry, dict) and set(entry) - {"features"} == {
                    "path"
                }
                if table.endswith("dev-dependencies") and (
                    (crate.name, name) == PATH_ONLY_DEV_DEPENDENCY
                    or (
                        name == crate.name
                        and isinstance(entry, dict)
                        and entry.get("path") == "."
                    )
                ):
                    if not path_only:
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


def package_tests(root: Path) -> None:
    _, crates = workspace(root)
    published = [crate for crate in crates if crate.publishable]
    target = Path(
        json.loads(
            output(
                ["cargo", "metadata", "--format-version", "1", "--no-deps"], cwd=root
            )
        )["target_directory"]
    )
    args = ["cargo", "package", "--no-verify", "--allow-dirty"]
    for crate in published:
        args += ["-p", crate.name]
    run(args, cwd=root)
    with tempfile.TemporaryDirectory() as scratch:
        extracted: dict[str, Path] = {}
        for crate in published:
            archive = target / "package" / f"{crate.name}-{crate.version}.crate"
            try:
                with tarfile.open(archive) as handle:
                    handle.extractall(scratch, filter="data")
            except (OSError, tarfile.TarError) as error:
                raise CratesError(f"cannot extract {archive}: {error}") from error
            extracted[crate.name] = Path(scratch) / f"{crate.name}-{crate.version}"
        patches: list[str] = []
        for name, path in extracted.items():
            patches += [
                "--config",
                f"patch.crates-io.{name}.path={json.dumps(str(path))}",
            ]
        for name, path in extracted.items():
            note(f"building the packaged tests of {name}")
            try:
                run(
                    [
                        "cargo",
                        *patches,
                        "test",
                        "--no-run",
                        "--target-dir",
                        str(target / "package-tests"),
                    ],
                    cwd=path,
                )
            except CratesError as error:
                raise CratesError(
                    f"the packaged tests of {name} do not build"
                ) from error
    note(f"the packaged tests build for {', '.join(extracted)}")


# Cargo names a local registry's source directory with an empty host, so the
# overlay `cargo publish --workspace` builds against is `-<hash>` (crates.io's
# starts with `index.`). An overlay crate is neither unpacked again nor rebuilt
# while its name and version stand, so a library changed without a bump would
# build as whichever tree first packaged that version.
def unpacked_packages(registry_src: Path, crates: list[Crate]) -> list[Path]:
    wanted = {f"{crate.name}-{crate.version}" for crate in crates if crate.publishable}
    return sorted(
        path
        for overlay in registry_src.glob("-*")
        for path in overlay.iterdir()
        if path.name in wanted
    )


def publish_dry_run(root: Path) -> None:
    _, crates = workspace(root)
    cargo_home = Path(os.environ.get("CARGO_HOME") or Path.home() / ".cargo")
    for path in unpacked_packages(cargo_home / "registry" / "src", crates):
        shutil.rmtree(path)
    target = (
        Path(
            json.loads(
                output(
                    ["cargo", "metadata", "--format-version", "1", "--no-deps"],
                    cwd=root,
                )
            )["target_directory"]
        )
        / "publish-dry-run"
    )
    # Its own target directory: cleaning the felis units out of the shared one
    # would throw away the test build of every crate.
    clean = ["cargo", "clean", "--target-dir", str(target)]
    for crate in crates:
        if crate.publishable:
            clean += ["-p", crate.name]
    run(clean, cwd=root)
    run(
        [
            "cargo",
            "publish",
            "--workspace",
            "--dry-run",
            "--allow-dirty",
            "--target-dir",
            str(target),
        ],
        cwd=root,
    )


def main(argv: list[str]) -> int:
    if len(argv) != 1 or argv[0] not in {
        "check",
        "publish-plan",
        "package-tests",
        "publish-dry-run",
    }:
        print(USAGE, file=sys.stderr, end="")
        return 64
    root = Path.cwd()
    try:
        if argv[0] == "check":
            check(root)
        elif argv[0] == "package-tests":
            package_tests(root)
        elif argv[0] == "publish-dry-run":
            publish_dry_run(root)
        else:
            for name in publish_plan(root):
                print(name)
    except (CratesError, UnicodeError) as error:
        print(f"release-crates: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
