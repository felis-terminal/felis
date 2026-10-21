"""Exit-code tests for tools/release/verify.py in a throwaway repository."""

from __future__ import annotations

import json
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

import pytest

VERIFY = Path(__file__).with_name("verify.py")


@dataclass(frozen=True)
class Repo:
    path: Path
    release: str
    after: str


def git(repo: Path, *args: str) -> str:
    result = subprocess.run(
        ["git", "-C", str(repo), *args],
        check=True,
        text=True,
        stdout=subprocess.PIPE,
    )
    return result.stdout.strip()


def verify(repo: Path, *args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, str(VERIFY), *args],
        cwd=repo,
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )


def assert_exit(result: subprocess.CompletedProcess[str], wanted: int) -> None:
    assert result.returncode == wanted, (
        f"wanted exit {wanted}, got {result.returncode}:\n{result.stdout}"
    )


def write_workspace(repo: Path, version: str) -> None:
    repo.joinpath("Cargo.toml").write_text(
        f'''[workspace]
members = ["crates/*"]

[workspace.package]
version = "{version}"
edition = "2024"
''',
        encoding="utf-8",
    )


def write_manifest(
    repo: Path, name: str, version_line: str = "version.workspace = true"
) -> None:
    directory = repo / "crates" / name
    directory.mkdir(parents=True, exist_ok=True)
    directory.joinpath("Cargo.toml").write_text(
        f'''[package]
name = "{name}"
{version_line}
edition.workspace = true

[dependencies]
serde = "1"
''',
        encoding="utf-8",
    )


def write_identity(
    path: Path,
    version: str,
    revision: str,
    dirty: bool,
    *,
    client: dict | None = None,
    client_status: str = "ok",
) -> Path:
    cli = {"version": version, "revision": revision, "dirty": dirty}
    report: dict = {
        "cli": cli,
        "client_status": client_status,
        "daemon_status": "not_running",
    }
    if client_status == "ok":
        report["client"] = cli if client is None else client
    path.write_text(json.dumps(report), encoding="utf-8")
    return path


@pytest.fixture(scope="module")
def base(tmp_path_factory: pytest.TempPathFactory) -> Repo:
    repo = tmp_path_factory.mktemp("verify") / "repo"
    repo.mkdir()
    git(repo, "init", "-q", "-b", "main")
    git(repo, "config", "user.email", "release@example.invalid")
    git(repo, "config", "user.name", "release test")
    git(repo, "config", "tag.forceSignAnnotated", "false")
    git(repo, "config", "tag.gpgSign", "false")
    write_workspace(repo, "0.1.0")
    for name in ("felis-cli", "felis-client", "felis-daemon", "felis-grid"):
        write_manifest(repo, name)
    repo.joinpath("CHANGELOG.md").write_text(
        """# Changelog

## [0.1.0] - 2026-09-04

### Added

- A first release.
""",
        encoding="utf-8",
    )
    git(repo, "add", "-A")
    git(repo, "commit", "-qm", "the release commit")
    release = git(repo, "rev-parse", "HEAD")
    git(repo, "tag", "-a", "-m", "felis 0.1.0", "v0.1.0")
    git(repo, "tag", "-a", "-m", "felis 0.1.0-rc.1", "v0.1.0-rc.1")
    git(repo, "tag", "-a", "-m", "felis 0.2.0", "v0.2.0")
    git(repo, "tag", "v0.1.1")
    git(repo, "commit", "-q", "--allow-empty", "-m", "a commit after the tag")
    after = git(repo, "rev-parse", "HEAD")
    return Repo(repo, release, after)


@pytest.fixture
def scratch(base: Repo, tmp_path: Path) -> Repo:
    repo = tmp_path / "repo"
    shutil.copytree(base.path, repo)
    return Repo(repo, base.release, base.after)


@pytest.mark.parametrize(
    ("tag", "commit", "wanted"),
    [
        pytest.param(
            "v0.1.0",
            "release",
            0,
            id="an annotated tag at the release commit passes",
        ),
        pytest.param(
            "v0.1.0-rc.1",
            "release",
            0,
            id="a release candidate needs no CHANGELOG section",
        ),
        pytest.param("v0.1.1", "release", 1, id="a lightweight tag fails"),
        pytest.param("v0.1.0", "after", 1, id="a tag pointing elsewhere fails"),
        pytest.param(
            "v0.2.0", "release", 1, id="a tag the manifests disagree with fails"
        ),
        pytest.param("v9.9.9", "release", 1, id="an unknown tag fails"),
        pytest.param("release-1", "release", 1, id="a non-semver tag name fails"),
        pytest.param(
            "v0.1.0-beta.1",
            "release",
            1,
            id="an unrecognized prerelease suffix fails",
        ),
    ],
)
def test_source_tag(base: Repo, tag: str, commit: str, wanted: int) -> None:
    assert_exit(verify(base.path, tag, getattr(base, commit)), wanted)


def test_a_dirty_working_tree_fails(scratch: Repo) -> None:
    with scratch.path.joinpath("CHANGELOG.md").open("a", encoding="utf-8") as handle:
        handle.write("\n# a stray edit\n")
    assert_exit(verify(scratch.path, "v0.1.0", scratch.release), 1)


def commit_and_tag(repo: Path, message: str, tag: str) -> str:
    git(repo, "commit", "-q", "-am", message)
    git(repo, "tag", "-a", "-m", f"felis {tag[1:]}", tag)
    return git(repo, "rev-parse", "HEAD")


def test_a_library_crate_on_its_own_version_passes(scratch: Repo) -> None:
    write_manifest(scratch.path, "felis-grid", 'version = "0.4.0"')
    library = commit_and_tag(
        scratch.path, "give a library crate its own version", "v0.1.0-rc.2"
    )
    assert_exit(verify(scratch.path, "v0.1.0-rc.2", library), 0)


def test_a_binary_crate_that_pins_its_own_version_fails(scratch: Repo) -> None:
    write_manifest(scratch.path, "felis-daemon", 'version = "0.2.0"')
    pinned = commit_and_tag(scratch.path, "pin a binary crate's version", "v0.1.0-rc.3")
    assert_exit(verify(scratch.path, "v0.1.0-rc.3", pinned), 1)


def test_a_missing_binary_crate_fails(scratch: Repo) -> None:
    git(scratch.path, "rm", "-qr", "crates/felis-client")
    missing = commit_and_tag(scratch.path, "drop a binary crate", "v0.1.0-rc.4")
    assert_exit(verify(scratch.path, "v0.1.0-rc.4", missing), 1)


@pytest.mark.parametrize(
    ("tag", "version", "revision", "dirty", "wanted"),
    [
        pytest.param(
            "v0.1.0",
            "0.1.0",
            "release",
            False,
            0,
            id="a clean artifact built from the tag passes",
        ),
        pytest.param(
            "v0.1.0-rc.1",
            "0.1.0",
            "release",
            False,
            0,
            id="the core semver is what a candidate's artifact reports",
        ),
        pytest.param(
            "v0.1.0", "0.1.0", "release", True, 1, id="a dirty artifact fails"
        ),
        pytest.param(
            "v0.1.0",
            "0.1.0",
            "after",
            False,
            1,
            id="an artifact built from another revision fails",
        ),
        pytest.param(
            "v0.1.0",
            "0.2.0",
            "release",
            False,
            1,
            id="an artifact reporting another version fails",
        ),
    ],
)
def test_identity(
    base: Repo,
    tmp_path: Path,
    tag: str,
    version: str,
    revision: str,
    dirty: bool,
    wanted: int,
) -> None:
    identity = write_identity(
        tmp_path / "version.json", version, getattr(base, revision), dirty
    )
    assert_exit(
        verify(base.path, "--identity", tag, base.release, str(identity)), wanted
    )


def test_a_bundled_client_reporting_the_clis_identity_passes(
    base: Repo, tmp_path: Path
) -> None:
    identity = write_identity(tmp_path / "version.json", "0.1.0", base.release, False)
    assert_exit(
        verify(
            base.path,
            "--identity",
            "--bundled-client",
            "v0.1.0",
            base.release,
            str(identity),
        ),
        0,
    )


def test_an_unavailable_client_passes_when_the_bundle_is_not_asked_about(
    base: Repo, tmp_path: Path
) -> None:
    identity = write_identity(
        tmp_path / "version.json",
        "0.1.0",
        base.release,
        False,
        client_status="unavailable",
    )
    assert_exit(
        verify(base.path, "--identity", "v0.1.0", base.release, str(identity)), 0
    )


def test_an_unavailable_bundled_client_fails(base: Repo, tmp_path: Path) -> None:
    identity = write_identity(
        tmp_path / "version.json",
        "0.1.0",
        base.release,
        False,
        client_status="unavailable",
    )
    assert_exit(
        verify(
            base.path,
            "--identity",
            "--bundled-client",
            "v0.1.0",
            base.release,
            str(identity),
        ),
        1,
    )


def test_a_bundled_client_built_from_another_revision_fails(
    base: Repo, tmp_path: Path
) -> None:
    identity = write_identity(
        tmp_path / "version.json",
        "0.1.0",
        base.release,
        False,
        client={"version": "0.1.0", "revision": base.after, "dirty": False},
    )
    assert_exit(
        verify(
            base.path,
            "--identity",
            "--bundled-client",
            "v0.1.0",
            base.release,
            str(identity),
        ),
        1,
    )


def test_bundled_client_without_identity_is_a_usage_error(base: Repo) -> None:
    assert_exit(verify(base.path, "--bundled-client", "v0.1.0", base.release), 64)
