#!/usr/bin/env python3
"""Fixture tests for tools/release/crates.py: requirement checks and the publish plan."""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

CRATES = Path(__file__).with_name("crates.py")

ROOT_MANIFEST = """[workspace]
members = ["crates/*"]

[workspace.package]
version = "0.3.0"

[workspace.dependencies]
felis-protocol = {{ path = "crates/felis-protocol", version = "{protocol_requirement}" }}
serde = "1"
"""


class Suite:
    def __init__(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.failures = 0

    def close(self) -> None:
        self.temporary.cleanup()

    def tree(
        self,
        *,
        protocol_version: str = "0.1.1",
        protocol_requirement: str = "0.1.1",
        grid_dependency: str = "felis-protocol = { workspace = true }",
        daemon_dev_dependency: str = "felis-protocol = { workspace = true }",
        client_core_dev_dependencies: str = 'felis-daemon = { path = "../felis-daemon" }\n'
        'felis-client-core = { path = ".", features = ["daemon-tests"] }',
    ) -> Path:
        tree = Path(tempfile.mkdtemp(dir=self.root))
        tree.joinpath("Cargo.toml").write_text(
            ROOT_MANIFEST.format(protocol_requirement=protocol_requirement),
            encoding="utf-8",
        )
        manifests = {
            "felis-protocol": f'version = "{protocol_version}"\n',
            "felis-grid": 'version = "0.1.0"\n\n[dependencies]\n'
            + grid_dependency
            + "\n",
            "felis-daemon": "version.workspace = true\n\n[dev-dependencies]\n"
            + daemon_dev_dependency
            + "\n",
            "felis-client-core": 'version = "0.1.0"\n\n[dev-dependencies]\n'
            + client_core_dev_dependencies
            + "\n",
        }
        for name, body in manifests.items():
            directory = tree / "crates" / name
            directory.mkdir(parents=True)
            directory.joinpath("Cargo.toml").write_text(
                f'[package]\nname = "{name}"\n{body}', encoding="utf-8"
            )
        return tree

    def index(self, published: dict[str, list[str]]) -> Path:
        index = Path(tempfile.mkdtemp(dir=self.root))
        for name, versions in published.items():
            path = index / name[:2] / name[2:4] / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(
                "".join(json.dumps({"name": name, "vers": v}) + "\n" for v in versions),
                encoding="utf-8",
            )
        return index

    def run(
        self, tree: Path, *args: str, index: Path | None = None
    ) -> subprocess.CompletedProcess[str]:
        environment = dict(os.environ)
        if index is not None:
            environment["FELIS_CRATES_INDEX"] = index.as_uri()
        return subprocess.run(
            [sys.executable, str(CRATES), *args],
            cwd=tree,
            env=environment,
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )

    def expect(
        self,
        name: str,
        result: subprocess.CompletedProcess[str],
        code: int,
        stdout: str | None = None,
    ) -> None:
        if result.returncode == code and (stdout is None or result.stdout == stdout):
            print(f"  ok   {name}")
            return
        self.failures += 1
        print(f"  FAIL {name} (wanted exit {code}, got {result.returncode})")
        for line in result.stdout.splitlines():
            print(f"       {line}")

    def requirements(self, name: str, tree: Path, wanted: bool) -> None:
        sys.path.insert(0, str(CRATES.parent))
        import crates

        problems = crates.check_requirements(tree)
        if bool(problems) != (not wanted):
            self.failures += 1
            print(f"  FAIL {name}: {problems}")
            return
        print(f"  ok   {name}")

    def run_all(self) -> int:
        self.requirements(
            "requirements naming each crate's version pass", self.tree(), True
        )
        self.requirements(
            "a workspace requirement behind its crate's version fails",
            self.tree(protocol_requirement="0.1.0"),
            False,
        )
        self.requirements(
            "a member stating its own version on a felis dependency fails",
            self.tree(
                grid_dependency='felis-protocol = { path = "../felis-protocol", version = "0.1.1" }'
            ),
            False,
        )
        self.requirements(
            "a path-only felis dev-dependency fails",
            self.tree(
                daemon_dev_dependency='felis-protocol = { path = "../felis-protocol" }'
            ),
            False,
        )
        self.requirements(
            "a versioned client-core to daemon dev-dependency fails",
            self.tree(
                client_core_dev_dependencies="felis-daemon = { workspace = true }"
            ),
            False,
        )

        tree = self.tree()
        index = self.index(
            {
                "felis-protocol": ["0.1.0"],
                "felis-grid": ["0.1.0"],
                "felis-client-core": ["0.1.0"],
                "felis-daemon": ["0.2.0"],
            }
        )
        self.expect(
            "the plan lists exactly the crates whose version is not on the index",
            self.run(tree, "publish-plan", index=index),
            0,
            "felis-daemon\nfelis-protocol\n",
        )
        index = self.index(
            {
                "felis-protocol": ["0.1.1"],
                "felis-grid": ["0.1.0"],
                "felis-client-core": ["0.1.0"],
                "felis-daemon": ["0.3.0"],
            }
        )
        self.expect(
            "a fully published tree plans nothing",
            self.run(tree, "publish-plan", index=index),
            0,
            "",
        )
        index = self.index({"felis-protocol": ["0.1.0"], "felis-daemon": ["0.2.0"]})
        self.expect(
            "a never-published crate fails the plan",
            self.run(tree, "publish-plan", index=index),
            1,
        )
        self.expect("an unknown verb is a usage error", self.run(tree, "publish"), 64)

        if self.failures == 0:
            print("release-crates self-test: all cases passed")
            return 0
        print(
            f"release-crates self-test: {self.failures} case(s) failed", file=sys.stderr
        )
        return 1


def main() -> int:
    suite = Suite()
    try:
        return suite.run_all()
    finally:
        suite.close()


if __name__ == "__main__":
    raise SystemExit(main())
