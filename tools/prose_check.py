#!/usr/bin/env python3
"""Mechanical subset of the felis prose norms (`.agents/skills/doc-prose`).

Only the rules a machine can decide without reading the argument: dashes,
history narration, the unambiguous filler phrases, and inside code comments
the same history rule plus a length ceiling. Markdown formatting and wrapping
are handled automatically by `oxfmt` via `treefmt` / `nix fmt`.
Everything else in the skill stays a human judgment.

Scope is the *added* lines of a diff rather than whole files: the committed
tree carries legacy hits, and a whole-tree gate would have to be preceded by
a cleanup commit before anyone could commit anything.
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys

# Five lines holds a why-not with its failure or an invariant with its
# citation; anything longer is arguing, and the argument has a doc.
MAX_COMMENT_LINES = 5

# Fixed advice per rule: a reviewer reporting these by hand always had to say
# how to fix the line, so the checker says it instead of naming the offense.
ADVICE = {
    "em-dash": (
        "Split into two sentences, or use a colon, comma, or parenthesis. "
        "If the dash meant 'that is' or 'because', write that word. "
        "If the aside adds nothing, delete it."
    ),
    "history": (
        "State the current contract in the present tense as if it had always "
        "held; the past belongs to git log and CHANGELOG.md. If the sentence "
        "only says what changed, delete it. A rejected alternative is argued "
        "in the present tense ('a sibling-field pair would...')."
    ),
    "filler": (
        "Delete the phrase. If the sentence loses nothing, it was filler; if "
        "it loses the claim, state the claim directly."
    ),
    "comment-history": (
        "Comments carry a why-not or an invariant the code cannot show, in "
        "the present tense. Justifying the change belongs in the commit body; "
        "the past belongs in git log."
    ),
    "comment-length": (
        f"A comment longer than {MAX_COMMENT_LINES} lines is an argument, not "
        "a why-not or an invariant. Keep the one sentence the reader needs "
        "at the site; move the argument to the owning explanation doc and "
        "cite it, or to the commit body if it only justifies this change. "
        "A `// SAFETY:` block is exempt."
    ),
}

HISTORY_PHRASES = [
    "previously",
    "originally",
    "used to",
    "no longer",
    "renamed from",
    "was removed",
    "were removed",
    "was renamed",
    "formerly",
    "historically",
    "review round",
    "as of round",
]
HISTORY_PATTERNS = [
    r"\bthe old \w+",
    # Version- and schema-bump stamps, e.g. "took DUMP_SCHEMA_VERSION to 7".
    r"\btook \w+ to \d+",
]

COMMENT_EXTRA_PHRASES = [
    "this is safe because",
    "for now",
    "todo: remove",
    "temporary",
]

FILLER_PHRASES = [
    "it's important to note",
    "it is important to note",
    "in this section we",
    "let's take a closer look",
    "delve",
    "leverage",
    "utilize",
    "serves as",
    "aims to",
    "is designed to",
    "in terms of",
    "when it comes to",
    "strikes a balance",
    "it's worth noting",
    "it is worth noting",
    "plays a pivotal role",
    "stands as a testament",
    "in conclusion",
    "moreover",
    "furthermore",
]
FILLER_PATTERNS = [r"not only\b.{0,120}?\bbut also"]

IGNORE_MARKER = "prose-check: ignore"
REGION_OFF = "<!-- prose-check: off -->"
REGION_ON = "<!-- prose-check: on -->"

URL_RE = re.compile(r"https?://|\bwww\.")
CODE_SPAN_RE = re.compile(r"`[^`]*`")
EM_DASH_RE = re.compile("—| – | -- ")
# A glossary or index line spells its item and description apart with a dash;
# that is the list's format, not a dash inside a sentence.
DEF_TERM_RE = re.compile(r"^\s*[-*+]\s+(?:\[[^\]]+\]\([^)]+\)|\*\*[^*]+\*\*)\s+—\s")
FENCE_RE = re.compile(r"^\s*(```|~~~)")


def phrase_regex(phrases: list[str], extra: list[str]) -> re.Pattern[str]:
    parts = []
    for phrase in phrases:
        body = r"\s+".join(re.escape(word) for word in phrase.split())
        prefix = r"\b" if phrase[0].isalnum() else ""
        suffix = r"\b" if phrase[-1].isalnum() else ""
        parts.append(prefix + body + suffix)
    parts.extend(extra)
    return re.compile("|".join(parts), re.IGNORECASE)


HISTORY_RE = phrase_regex(HISTORY_PHRASES, HISTORY_PATTERNS)
COMMENT_HISTORY_RE = phrase_regex(
    HISTORY_PHRASES + COMMENT_EXTRA_PHRASES, HISTORY_PATTERNS
)
FILLER_RE = phrase_regex(FILLER_PHRASES, FILLER_PATTERNS)


class Hit:
    def __init__(self, path: str, line: int, rule: str, text: str) -> None:
        self.path = path
        self.line = line
        self.rule = rule
        self.text = text


def classify(path: str) -> str | None:
    """Return the checked file kind, or None when the path is out of scope."""
    p = path.replace("\\", "/")
    if p.startswith("./"):
        p = p[2:]
    if p.endswith(".md"):
        if p.startswith(("docs/", "skills/", ".agents/skills/", ".claude/skills/")):
            return "markdown"
        # CHANGELOG.md legitimately narrates history, so it is never checked.
        if "/" not in p and p != "CHANGELOG.md":
            return "markdown"
        return None
    if p.startswith("crates/") and p.endswith(".rs"):
        if "/src/generated/" in p:
            return None
        return "rust"
    if p.endswith(".proto"):
        return "proto"
    if p.startswith(".forgejo/") and p.endswith((".yml", ".yaml")):
        return "yaml"
    return None


def strip_code_spans(line: str) -> str:
    return CODE_SPAN_RE.sub(lambda m: " " * len(m.group(0)), line)


def check_markdown(path: str, lines: list[str], targets: set[int] | None) -> list[Hit]:
    hits: list[Hit] = []
    in_fence = False
    suppressed = False
    start = 0
    # Starlight frontmatter is metadata, not prose.
    if lines and lines[0].strip() == "---":
        for idx in range(1, len(lines)):
            if lines[idx].strip() == "---":
                start = idx + 1
                break

    for idx in range(start, len(lines)):
        line = lines[idx].rstrip("\n")
        number = idx + 1
        stripped = line.strip()

        if stripped == REGION_OFF:
            suppressed = True
            continue
        if stripped == REGION_ON:
            suppressed = False
            continue
        if FENCE_RE.match(line):
            in_fence = not in_fence
            continue
        if in_fence or suppressed:
            continue
        if IGNORE_MARKER in line:
            continue
        if targets is not None and number not in targets:
            continue
        if stripped.startswith("|") or URL_RE.search(line):
            continue

        prose = strip_code_spans(DEF_TERM_RE.sub("", line))
        match = EM_DASH_RE.search(prose)
        if match:
            hits.append(Hit(path, number, "em-dash", stripped))
        match = HISTORY_RE.search(prose)
        if match:
            hits.append(Hit(path, number, "history", match.group(0)))
        match = FILLER_RE.search(prose)
        if match:
            hits.append(Hit(path, number, "filler", match.group(0)))
    return hits


def comment_lines(kind: str, lines: list[str]) -> dict[int, str]:
    """Map 1-based line numbers to their comment text."""
    found: dict[int, str] = {}
    in_block = False
    for idx, raw in enumerate(lines):
        line = raw.rstrip("\n")
        stripped = line.strip()
        number = idx + 1
        if kind == "yaml":
            if stripped.startswith("#"):
                found[number] = stripped
            continue
        if in_block:
            found[number] = stripped
            if "*/" in stripped:
                in_block = False
            continue
        if stripped.startswith("//"):
            found[number] = stripped
        elif stripped.startswith("/*"):
            found[number] = stripped
            if "*/" not in stripped[2:]:
                in_block = True
    return found


def comment_blocks(found: dict[int, str]) -> list[list[int]]:
    """Group comment line numbers into runs of consecutive lines."""
    blocks: list[list[int]] = []
    for number in sorted(found):
        if blocks and blocks[-1][-1] == number - 1:
            blocks[-1].append(number)
        else:
            blocks.append([number])
    return blocks


def check_comment_length(
    path: str, found: dict[int, str], targets: set[int] | None
) -> list[Hit]:
    hits: list[Hit] = []
    for block in comment_blocks(found):
        if len(block) <= MAX_COMMENT_LINES:
            continue
        texts = [found[n] for n in block]
        if any(IGNORE_MARKER in t for t in texts):
            continue
        # The audited unsafe sites keep their full record in the source by
        # workspace policy (AGENTS.md), so the ceiling does not apply there.
        if "SAFETY:" in texts[0]:
            continue
        if targets is not None and not any(n in targets for n in block):
            continue
        hits.append(Hit(path, block[0], "comment-length", f"{len(block)} lines"))
    return hits


def check_code(
    path: str, kind: str, lines: list[str], targets: set[int] | None
) -> list[Hit]:
    found = comment_lines(kind, lines)
    hits: list[Hit] = check_comment_length(path, found, targets)
    for number, text in found.items():
        if IGNORE_MARKER in text:
            continue
        if targets is not None and number not in targets:
            continue
        # Code spans go the way they go in markdown: `felis -- cmd` names a
        # command line, and its separator is not a dash in a sentence.
        prose = strip_code_spans(text)
        match = EM_DASH_RE.search(prose)
        if match:
            hits.append(Hit(path, number, "em-dash", text))
        match = COMMENT_HISTORY_RE.search(prose)
        if match:
            hits.append(Hit(path, number, "comment-history", match.group(0)))
    return hits


def check_file(
    path: str, kind: str, lines: list[str], targets: set[int] | None
) -> list[Hit]:
    if kind == "markdown":
        return check_markdown(path, lines, targets)
    return check_code(path, kind, lines, targets)


def git(args: list[str]) -> str:
    return subprocess.run(
        ["git", *args], check=True, capture_output=True, text=True
    ).stdout


def inside_work_tree() -> bool:
    try:
        return git(["rev-parse", "--is-inside-work-tree"]).strip() == "true"
    except (subprocess.CalledProcessError, FileNotFoundError):
        return False


HUNK_RE = re.compile(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@")


def added_lines(diff_args: list[str]) -> dict[str, set[int]]:
    out = git(["diff", "-U0", *diff_args])
    result: dict[str, set[int]] = {}
    path = None
    number = 0
    for line in out.splitlines():
        if line.startswith("+++ "):
            target = line[4:].strip()
            path = None if target == "/dev/null" else target[2:]
            continue
        if line.startswith("@@"):
            match = HUNK_RE.match(line)
            if match:
                number = int(match.group(1))
            continue
        if path is None:
            continue
        if line.startswith("+"):
            result.setdefault(path, set()).add(number)
            number += 1
        elif line.startswith(" "):
            number += 1
    return result


def blob(rev: str, path: str) -> list[str] | None:
    try:
        return git(["show", f"{rev}:{path}"]).splitlines()
    except subprocess.CalledProcessError:
        return None


def read_lines(path: str) -> list[str] | None:
    try:
        with open(path, encoding="utf-8") as handle:
            return handle.read().splitlines()
    except OSError:
        return None


def report(hits: list[Hit]) -> int:
    seen: set[tuple[str, str]] = set()
    for hit in sorted(hits, key=lambda h: (h.path, h.line, h.rule)):
        print(f"{hit.path}:{hit.line}: [{hit.rule}] {hit.text}")
        key = (hit.path, hit.rule)
        if key not in seen:
            seen.add(key)
            print(f"    {ADVICE[hit.rule]}")
    print(f"prose-check: {len(hits)} hit(s)")
    return 1 if hits else 0


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


def self_test() -> int:
    failures = []
    for name, kind, fixture, expected in (
        ("markdown", "markdown", MD_FIXTURE, MD_EXPECTED),
        ("rust", "rust", RS_FIXTURE, RS_EXPECTED),
    ):
        hits = check_file(name, kind, fixture.splitlines(), None)
        got = {(hit.line, hit.rule) for hit in hits}
        for missing in sorted(expected - got):
            failures.append(f"{name}: expected {missing} and got no hit")
        for extra in sorted(got - expected):
            failures.append(f"{name}: unexpected hit {extra}")
        print(f"{name} fixture: {len(got)} hit(s), {len(expected)} expected")
    for failure in failures:
        print(f"FAIL {failure}")
    print("prose-check self-test: " + ("FAILED" if failures else "ok"))
    return 1 if failures else 0


def main(argv: list[str]) -> int:
    if os.environ.get("FELIS_PROSE_CHECK_SKIP") == "1":
        return 0

    parser = argparse.ArgumentParser(description="Check prose norms mechanically.")
    parser.add_argument("--range", dest="rev_range", help="check a git diff range")
    parser.add_argument(
        "--all", action="store_true", help="check every line of the given files"
    )
    parser.add_argument("--self-test", action="store_true", dest="self_test")
    parser.add_argument("files", nargs="*")
    args = parser.parse_args(argv)

    if args.self_test:
        return self_test()

    hits: list[Hit] = []

    if args.all:
        target_files = args.files if args.files else git(["ls-files"]).splitlines()
        for path in target_files:
            kind = classify(path)
            lines = read_lines(path) if kind else None
            if kind and lines is not None:
                hits.extend(check_file(path, kind, lines, None))
        return report(hits)

    # `nix flake check` runs the hooks in a store copy that has no `.git`,
    # the same reason the buf-breaking hook exits early there.
    if not inside_work_tree():
        return 0

    if args.rev_range:
        diff_args = [args.rev_range]
        rev = args.rev_range
        for sep in ("...", ".."):
            if sep in rev:
                rev = rev.split(sep, 1)[1]
                break
        else:
            rev = ""
    else:
        diff_args = ["--cached"]
        rev = ":"
        if args.files:
            diff_args += ["--", *args.files]

    wanted = set(args.files) if args.files else None
    for path, targets in added_lines(diff_args).items():
        kind = classify(path)
        if not kind or (wanted is not None and path not in wanted):
            continue
        if rev == ":":
            lines = blob("", path)
        elif rev:
            lines = blob(rev, path)
        else:
            lines = read_lines(path)
        if lines is None:
            continue
        hits.extend(check_file(path, kind, lines, targets))
    return report(hits)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
