from __future__ import annotations

import pytest

from dep_bans import BANS, OS, ROOT, violations

FIXTURE = """\
felis-protocol v0.2.0 (/src/crates/felis-protocol)
bytes v1.12.1
serde v1.0.229
serde_derive v1.0.229 (proc-macro)
[build-dependencies]
prost-build v0.14.4
windows-sys v0.61.2
libc v0.2.190
libc v0.2.190 (*)
"""


@pytest.mark.parametrize(
    ("bans", "expected"),
    [
        pytest.param(
            OS, ["libc", "windows-sys"], id="ban hits across normal and build sections"
        ),
        pytest.param(
            ("felis-protocol",), [], id="crate root is not its own dependency"
        ),
        pytest.param(
            ("serde_der", "windows"), [], id="names match exactly, not by prefix"
        ),
        pytest.param(
            ("[build-dependencies]",), [], id="section headers are not crate names"
        ),
    ],
)
def test_violations(bans, expected):
    assert violations(bans, FIXTURE) == expected


def test_bans_name_only_crates_present_in_the_workspace():
    present = {p.name for p in (ROOT / "crates").glob("felis-*")}
    assert sorted(set(BANS) - present) == []
