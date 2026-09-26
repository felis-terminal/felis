#!/usr/bin/env python3
"""Exit-code tests for tools/release/verify.py in a throwaway repository."""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
from pathlib import Path

VERIFY = Path(__file__).with_name("verify.py")


class Suite:
    def __init__(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
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

    def expect(self, wanted: int, name: str, *args: str) -> None:
        result = subprocess.run(
            [sys.executable, str(VERIFY), *args],
            cwd=self.repo,
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        if result.returncode == wanted:
            print(f"  ok   {name}")
            return
        self.failures += 1
        print(f"  FAIL {name} (wanted exit {wanted}, got {result.returncode})")
        for line in result.stdout.splitlines():
            print(f"       {line}")

    def write_workspace(self, version: str) -> None:
        self.repo.joinpath("Cargo.toml").write_text(
            f'''[workspace]
members = ["crates/*"]

[workspace.package]
version = "{version}"
edition = "2024"
''',
            encoding="utf-8",
        )

    def write_manifest(
        self, name: str, version_line: str = "version.workspace = true"
    ) -> None:
        directory = self.repo / "crates" / name
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

    def identity(
        self,
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
        path = self.root / "version.json"
        path.write_text(json.dumps(report), encoding="utf-8")
        return path

    def run(self) -> int:
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.email", "release@example.invalid")
        self.git("config", "user.name", "release test")
        self.git("config", "tag.forceSignAnnotated", "false")
        self.git("config", "tag.gpgSign", "false")
        self.write_workspace("0.1.0")
        self.write_manifest("felis-cli")
        self.write_manifest("felis-grid")
        self.repo.joinpath("CHANGELOG.md").write_text(
            """# Changelog

## [0.1.0] - 2026-09-04

### Added

- A first release.
""",
            encoding="utf-8",
        )
        self.git("add", "-A")
        self.git("commit", "-qm", "the release commit")
        release = self.git("rev-parse", "HEAD", capture=True)
        self.git("tag", "-a", "-m", "felis 0.1.0", "v0.1.0")
        self.git("tag", "-a", "-m", "felis 0.1.0-rc.1", "v0.1.0-rc.1")
        self.git("tag", "-a", "-m", "felis 0.2.0", "v0.2.0")
        self.git("tag", "v0.1.1")
        self.git("commit", "-q", "--allow-empty", "-m", "a commit after the tag")
        after = self.git("rev-parse", "HEAD", capture=True)

        self.expect(
            0, "an annotated tag at the release commit passes", "v0.1.0", release
        )
        self.expect(
            0,
            "a release candidate needs no CHANGELOG section",
            "v0.1.0-rc.1",
            release,
        )
        self.expect(1, "a lightweight tag fails", "v0.1.1", release)
        self.expect(1, "a tag pointing elsewhere fails", "v0.1.0", after)
        self.expect(1, "a tag the manifests disagree with fails", "v0.2.0", release)
        self.expect(1, "an unknown tag fails", "v9.9.9", release)
        self.expect(1, "a non-semver tag name fails", "release-1", release)
        self.expect(
            1,
            "an unrecognized prerelease suffix fails",
            "v0.1.0-beta.1",
            release,
        )

        with self.repo.joinpath("CHANGELOG.md").open("a", encoding="utf-8") as handle:
            handle.write("\n# a stray edit\n")
        self.expect(1, "a dirty working tree fails", "v0.1.0", release)
        self.git("checkout", "-q", "--", "CHANGELOG.md")

        self.write_manifest("felis-grid", 'version = "0.2.0"')
        self.git("commit", "-q", "-am", "pin one crate's version")
        pinned = self.git("rev-parse", "HEAD", capture=True)
        self.git("tag", "-a", "-m", "felis 0.1.0-rc.2", "v0.1.0-rc.2")
        self.expect(
            1,
            "a crate that pins its own version fails",
            "v0.1.0-rc.2",
            pinned,
        )
        self.git("reset", "-q", "--hard", after)

        identity = self.identity("0.1.0", release, False)
        self.expect(
            0,
            "a clean artifact built from the tag passes",
            "--identity",
            "v0.1.0",
            release,
            str(identity),
        )
        self.expect(
            0,
            "the core semver is what a candidate's artifact reports",
            "--identity",
            "v0.1.0-rc.1",
            release,
            str(identity),
        )
        identity = self.identity("0.1.0", release, True)
        self.expect(
            1,
            "a dirty artifact fails",
            "--identity",
            "v0.1.0",
            release,
            str(identity),
        )
        identity = self.identity("0.1.0", after, False)
        self.expect(
            1,
            "an artifact built from another revision fails",
            "--identity",
            "v0.1.0",
            release,
            str(identity),
        )
        identity = self.identity("0.2.0", release, False)
        self.expect(
            1,
            "an artifact reporting another version fails",
            "--identity",
            "v0.1.0",
            release,
            str(identity),
        )

        identity = self.identity("0.1.0", release, False)
        self.expect(
            0,
            "a bundled client reporting the CLI's identity passes",
            "--identity",
            "--bundled-client",
            "v0.1.0",
            release,
            str(identity),
        )
        identity = self.identity("0.1.0", release, False, client_status="unavailable")
        self.expect(
            0,
            "an unavailable client passes when the bundle is not asked about",
            "--identity",
            "v0.1.0",
            release,
            str(identity),
        )
        self.expect(
            1,
            "an unavailable bundled client fails",
            "--identity",
            "--bundled-client",
            "v0.1.0",
            release,
            str(identity),
        )
        identity = self.identity(
            "0.1.0",
            release,
            False,
            client={"version": "0.1.0", "revision": after, "dirty": False},
        )
        self.expect(
            1,
            "a bundled client built from another revision fails",
            "--identity",
            "--bundled-client",
            "v0.1.0",
            release,
            str(identity),
        )
        self.expect(
            64,
            "--bundled-client without --identity is a usage error",
            "--bundled-client",
            "v0.1.0",
            release,
        )

        if self.failures == 0:
            print("release-verify self-test: all cases passed")
            return 0
        print(
            f"release-verify self-test: {self.failures} case(s) failed",
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
