#!/usr/bin/env python3
from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

EXPRESSION = (
    'type == "!!map" and (.name | type == "!!str") and '
    '(.description | type == "!!str") and (.name | length > 0) and '
    "(.description | length > 0)"
)


def discover_files(arguments: list[str]) -> list[Path]:
    if arguments:
        return [Path(argument) for argument in arguments]
    files: list[Path] = []
    for root in (Path(".agents/skills"), Path("skills")):
        if root.is_dir():
            files.extend(root.rglob("SKILL.md"))
    return files


def frontmatter(path: Path) -> str | None:
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeError):
        return None
    if not lines or lines[0] != "---":
        return None
    try:
        end = lines.index("---", 1)
    except ValueError:
        return None
    return "\n".join(lines[1:end]) + "\n"


def valid_yaml(yq: str, document: str) -> bool:
    try:
        result = subprocess.run(
            [yq, "-p=yaml", "-e", EXPRESSION],
            check=False,
            input=document,
            text=True,
            stdout=subprocess.DEVNULL,
        )
    except OSError:
        return False
    return result.returncode == 0


def main(argv: list[str]) -> int:
    yq = os.environ.get("YQ", "yq")
    failed = False
    for path in discover_files(argv):
        if not path.is_file():
            continue
        document = frontmatter(path)
        if document is None:
            print(f"{path}: missing YAML frontmatter delimiters", file=sys.stderr)
            failed = True
            continue
        if not valid_yaml(yq, document):
            print(f"{path}: invalid skill frontmatter", file=sys.stderr)
            failed = True
    return int(failed)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
