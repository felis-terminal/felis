from __future__ import annotations

import pytest

from prose_check import check_file

MD_FIXTURE = """# Sample page

This line has an em dash — right here.

Previously the daemon owned the grid.

It's important to note that the socket is per user.

See https://example.com/a/very/long/url/that/runs/past/eighty/columns/for/sure/really/ok

| col | a -- b |

```
previously this was different
```

An `a — b` code span is fine.

- **Term** — a glossary entry keeps its separator.

<!-- prose-check: off -->
previously — it is important to note
<!-- prose-check: on -->

This mentions previously but is skipped. prose-check: ignore
"""

RS_FIXTURE = """// Previously the client owned the buffer.
fn a() {}
/* this is safe because the lock is held */
// an em dash — in a comment
// for now, keep the retry loop
// a `felis -- cmd` code span is fine
let s = "previously — really";
// temporary shim. prose-check: ignore
fn z() {}
// one
// two
// three
// four
// five
// six
fn b() {}
// SAFETY: one
// two
// three
// four
// five
// six
fn c() {}
/// one
/// two
/// three
/// four
/// five
fn d() {}
"""

MD_EXPECTED = {
    (3, "em-dash"),
    (5, "history"),
    (7, "filler"),
}
RS_EXPECTED = {
    (1, "comment-history"),
    (3, "comment-history"),
    (4, "em-dash"),
    (5, "comment-history"),
    (10, "comment-length"),
}


@pytest.mark.parametrize(
    ("kind", "fixture", "expected"),
    [
        ("markdown", MD_FIXTURE, MD_EXPECTED),
        ("rust", RS_FIXTURE, RS_EXPECTED),
    ],
    ids=["markdown", "rust"],
)
def test_fixture_hits_exactly_the_expected_rules(kind, fixture, expected):
    hits = check_file(kind, kind, fixture.splitlines(), None)
    assert {(hit.line, hit.rule) for hit in hits} == expected
