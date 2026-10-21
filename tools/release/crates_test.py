"""Fixture tests for tools/release/crates.py: requirement checks and the publish plan."""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

import crates
import pytest

CRATES = Path(__file__).with_name("crates.py")

ROOT_MANIFEST = """[workspace]
members = ["crates/*"]

[workspace.package]
version = "0.3.0"

[workspace.dependencies]
felis-protocol = {{ path = "crates/felis-protocol", version = "{protocol_requirement}" }}
serde = "1"
"""


def make_tree(
    root: Path,
    *,
    protocol_version: str = "0.1.1",
    protocol_requirement: str = "0.1.1",
    grid_dependency: str = "felis-protocol = { workspace = true }",
    daemon_dev_dependency: str = "felis-protocol = { workspace = true }",
    client_core_dev_dependencies: str = 'felis-daemon = { path = "../felis-daemon" }\n'
    'felis-client-core = { path = ".", features = ["daemon-tests"] }',
) -> Path:
    tree = Path(tempfile.mkdtemp(dir=root))
    tree.joinpath("Cargo.toml").write_text(
        ROOT_MANIFEST.format(protocol_requirement=protocol_requirement),
        encoding="utf-8",
    )
    manifests = {
        "felis-protocol": f'version = "{protocol_version}"\n',
        "felis-grid": 'version = "0.1.0"\n\n[dependencies]\n' + grid_dependency + "\n",
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


def make_index(root: Path, published: dict[str, list[str]]) -> Path:
    index = Path(tempfile.mkdtemp(dir=root))
    for name, versions in published.items():
        path = index / name[:2] / name[2:4] / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(
            "".join(json.dumps({"name": name, "vers": v}) + "\n" for v in versions),
            encoding="utf-8",
        )
    return index


def run(
    tree: Path, *args: str, index: Path | None = None
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


def assert_result(
    result: subprocess.CompletedProcess[str], code: int, stdout: str | None = None
) -> None:
    assert result.returncode == code, (
        f"wanted exit {code}, got {result.returncode}:\n{result.stdout}"
    )
    if stdout is not None:
        assert result.stdout == stdout, (
            f"wanted stdout {stdout!r}, got {result.stdout!r}"
        )


def test_requirements_naming_each_crates_version_pass(tmp_path: Path) -> None:
    problems = crates.check_requirements(make_tree(tmp_path))
    assert not problems, f"unexpected requirement problems: {problems}"


@pytest.mark.parametrize(
    "overrides",
    [
        pytest.param(
            {"protocol_requirement": "0.1.0"},
            id="a workspace requirement behind its crate's version fails",
        ),
        pytest.param(
            {
                "grid_dependency": 'felis-protocol = { path = "../felis-protocol", version = "0.1.1" }'
            },
            id="a member stating its own version on a felis dependency fails",
        ),
        pytest.param(
            {
                "daemon_dev_dependency": 'felis-protocol = { path = "../felis-protocol" }'
            },
            id="a path-only felis dev-dependency fails",
        ),
        pytest.param(
            {"client_core_dev_dependencies": "felis-daemon = { workspace = true }"},
            id="a versioned client-core to daemon dev-dependency fails",
        ),
    ],
)
def test_requirement_problem(tmp_path: Path, overrides: dict[str, str]) -> None:
    problems = crates.check_requirements(make_tree(tmp_path, **overrides))
    assert problems, "wanted a requirement problem, got none"


def test_the_plan_lists_exactly_the_crates_whose_version_is_not_on_the_index(
    tmp_path: Path,
) -> None:
    index = make_index(
        tmp_path,
        {
            "felis-protocol": ["0.1.0"],
            "felis-grid": ["0.1.0"],
            "felis-client-core": ["0.1.0"],
            "felis-daemon": ["0.2.0"],
        },
    )
    assert_result(
        run(make_tree(tmp_path), "publish-plan", index=index),
        0,
        "felis-daemon\nfelis-protocol\n",
    )


def test_a_fully_published_tree_plans_nothing(tmp_path: Path) -> None:
    index = make_index(
        tmp_path,
        {
            "felis-protocol": ["0.1.1"],
            "felis-grid": ["0.1.0"],
            "felis-client-core": ["0.1.0"],
            "felis-daemon": ["0.3.0"],
        },
    )
    assert_result(run(make_tree(tmp_path), "publish-plan", index=index), 0, "")


def test_a_never_published_crate_fails_the_plan(tmp_path: Path) -> None:
    index = make_index(
        tmp_path, {"felis-protocol": ["0.1.0"], "felis-daemon": ["0.2.0"]}
    )
    assert_result(run(make_tree(tmp_path), "publish-plan", index=index), 1)


def test_an_unknown_verb_is_a_usage_error(tmp_path: Path) -> None:
    assert_result(run(make_tree(tmp_path), "publish"), 64)
