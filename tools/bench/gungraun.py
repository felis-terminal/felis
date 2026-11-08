#!/usr/bin/env python3
"""Instruction-count regression gate (bench.yml, `just bench-gate-ir`).

Benches the base revision in a throwaway worktree, saves it as a
gungraun baseline, then benches the current checkout against it with a
callgrind limit on instructions (Ir). Each crate in GATES carries its
own `gungraun_gate` bench. gungraun applies the limit; this script owns
what gungraun cannot know: skipping a crate whose gate the base
predates, a fresh output home per run, the per-bench report, and the PR
label that accepts a regression.

Exit codes: 0 clean, skipped, or accepted by label; 1 regression, a
failed bench run, or a base bench missing from the head run; 2 nothing
was compared.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

GATES = ("felis-client-core", "felis-shaping")
BENCH = "gungraun_gate"
BASELINE = "base"
DEFAULT_THRESHOLD_PCT = 2.0
ACCEPT_LABEL = "perf-regression-accepted"
# gungraun-runner's exit status when only limits were exceeded; any
# other non-zero status is a broken run. `cargo bench` folds both into
# 101, so the bench executable runs directly.
REGRESSION_RC = 3


def label_accepts(labels_json: str) -> bool:
    """True when the event payload's `pull_request.labels` carries ACCEPT_LABEL."""
    if not labels_json.strip():
        return False
    labels = json.loads(labels_json)
    return any(label.get("name") == ACCEPT_LABEL for label in labels or [])


def bench_source(package: str) -> str:
    return f"crates/{package}/benches/{BENCH}.rs"


def crate_home(home: Path, package: str) -> Path:
    """The `--home` one crate's bench runs with.

    gungraun names no package in the paths under it when the bench
    executable runs outside `cargo bench`, so a shared home would leave
    the summaries without a crate to attribute a regression to.
    """
    return home / package


def collect(home: Path) -> list[dict]:
    """One row per benchmark summary that was compared against BASELINE."""
    rows = []
    for package in GATES:
        rows += collect_crate(crate_home(home, package), package)
    return rows


def collect_crate(home: Path, package: str) -> list[dict]:
    rows = []
    for path in sorted(home.glob("**/summary.json")):
        summary = json.loads(path.read_text())
        for profile in summary["profiles"]:
            if profile["tool"] != "Callgrind":
                continue
            total = profile["data"]["total"]
            ir = total["metrics"]["Ir"]["values"]
            rows.append(
                {
                    # gungraun's own identity: a baseline is stored per
                    # module path, so a workload moved to another group
                    # keeps its name but loses its base count.
                    "key": f"{package}::{summary['module_path']}::{summary.get('id')}",
                    "package": package,
                    "bench": f"{summary['function_name']}/{summary['id']}",
                    "new": ir["new"],
                    "old": ir.get("old"),
                    "regressions": total.get("regressions") or [],
                }
            )
    return rows


def render(rows: list[dict]) -> str:
    lines = [f"{'bench':<52} {'base Ir':>12} {'head Ir':>12} {'change':>9}"]
    for row in rows:
        old, new = row["old"], row["new"]
        change = f"{(new - old) / old * 100:+.2f}%" if old else "new"
        mark = "  REGRESSED" if row["regressions"] else ""
        old_text = str(old) if old is not None else "-"
        lines.append(f"{row['bench']:<52} {old_text:>12} {new:>12} {change:>9}{mark}")
    return "\n".join(lines)


def verdict(
    rows: list[dict],
    base_ids: set[str],
    bench_rcs: dict[str, int],
    accepted: bool,
    threshold: float,
) -> int:
    for package, rc in bench_rcs.items():
        if rc not in (0, REGRESSION_RC):
            print(
                f"bench-gate-ir: the {package} bench run failed (exit {rc})",
                file=sys.stderr,
            )
            return 1
    head = {row["key"]: row for row in rows}
    missing = sorted(base_ids - head.keys())
    uncompared = sorted(
        head[key]["bench"] for key in base_ids & head.keys() if head[key]["old"] is None
    )
    if missing or uncompared:
        lost = ", ".join([*missing, *uncompared])
        print(
            f"bench-gate-ir: base bench(es) not compared in the head run: {lost}."
            f" The {ACCEPT_LABEL} label accepts a cost, not lost coverage.",
            file=sys.stderr,
        )
        return 1
    regressed = [row for row in rows if row["regressions"]]
    for package, rc in bench_rcs.items():
        if rc == REGRESSION_RC and not any(
            row["package"] == package for row in regressed
        ):
            print(
                f"bench-gate-ir: gungraun reported a {package} regression no"
                " summary records",
                file=sys.stderr,
            )
            return 1
    if regressed:
        names = ", ".join(row["bench"] for row in regressed)
        if accepted:
            print(
                f"bench-gate-ir: {len(regressed)} bench(es) past +{threshold}% Ir"
                f" ({names}), accepted by the {ACCEPT_LABEL} label"
            )
            return 0
        print(
            f"bench-gate-ir: {len(regressed)} bench(es) past +{threshold}% Ir: {names}."
            f" If the cost is intended, add the {ACCEPT_LABEL} label and state why"
            " in a PR comment.",
            file=sys.stderr,
        )
        return 1
    if not any(row["old"] is not None for row in rows):
        print("bench-gate-ir: no benchmark was compared to the base", file=sys.stderr)
        return 2
    print(
        f"bench-gate-ir: all {len(rows)} bench(es) within +{threshold}% Ir of the base"
    )
    return 0


def git(repo: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["git", "-C", str(repo), *args], check=check, capture_output=True, text=True
    )


def workspace_members(cwd: Path) -> list[str]:
    meta = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version=1"],
        cwd=cwd,
        check=True,
        stdout=subprocess.PIPE,
        text=True,
    ).stdout
    return [pkg["name"] for pkg in json.loads(meta)["packages"]]


def build_benches(cwd: Path, target_dir: Path, packages: list[str]) -> dict[str, Path]:
    env = {**os.environ, "CARGO_TARGET_DIR": str(target_dir)}
    # cargo identifies a workspace unit without its checkout path, so an
    # artifact another checkout built later than these sources were
    # written passes as fresh, and the "head" would bench that build.
    clean = ["cargo", "clean", "--profile", "bench"]
    for member in workspace_members(cwd):
        clean += ["-p", member]
    subprocess.run(clean, cwd=cwd, env=env, check=True)
    cmd = ["cargo", "bench", "--bench", BENCH, "--no-run"]
    for package in packages:
        cmd += ["-p", package]
    print(f"::: {cwd}: {' '.join(cmd)}", flush=True)
    out = subprocess.run(
        [*cmd, "--message-format=json"],
        cwd=cwd,
        env=env,
        check=True,
        stdout=subprocess.PIPE,
        text=True,
    ).stdout
    exes = {}
    for line in out.splitlines():
        try:
            msg = json.loads(line)
        except ValueError:
            continue
        if (
            msg.get("reason") == "compiler-artifact"
            and msg["target"]["name"] == BENCH
            and msg.get("executable")
        ):
            # A package id names its package only when that differs
            # from the directory, so the manifest's directory is read.
            exes[Path(msg["manifest_path"]).parent.name] = Path(msg["executable"])
    missing = [package for package in packages if package not in exes]
    if missing:
        raise RuntimeError(
            f"cargo built no {BENCH} executable for {', '.join(missing)} in {cwd}"
        )
    return exes


def run_benches(
    cwd: Path, target_dir: Path, home: Path, packages: list[str], *gungraun_args: str
) -> dict[str, int]:
    exes = build_benches(cwd, target_dir, packages)
    rcs = {}
    for package in packages:
        cmd = [
            str(exes[package]),
            "--bench",
            f"--home={crate_home(home, package)}",
            *gungraun_args,
        ]
        print(f"::: {cwd}: {' '.join(cmd)}", flush=True)
        rcs[package] = subprocess.run(cmd, cwd=cwd / "crates" / package).returncode
    return rcs


def cmd_gate(args: argparse.Namespace, repo: Path) -> int:
    base = git(repo, "rev-parse", "--verify", f"{args.base}^{{commit}}").stdout.strip()
    base_packages = []
    for package in GATES:
        source = bench_source(package)
        if git(repo, "cat-file", "-e", f"{base}:{source}", check=False).returncode:
            print(
                f"bench-gate-ir: base {base[:12]} has no {source}; nothing to"
                f" compare {package} against, so its gate is skipped"
            )
        else:
            base_packages.append(package)
    if not base_packages:
        return 0

    # A fresh home per run: the target directory outlives the job on
    # the CI runner, and a baseline left there by another PR would be
    # compared as if it were this base.
    scratch = Path(tempfile.mkdtemp(prefix="felis-bench-gate-ir."))
    target_dir = Path(os.environ.get("CARGO_TARGET_DIR") or repo / "target")
    home = scratch / "home"
    worktree = scratch / "base"
    base_ids: set[str] = set()
    try:
        git(repo, "worktree", "add", "--detach", str(worktree), base)
        try:
            base_rcs = run_benches(
                worktree,
                target_dir,
                home,
                base_packages,
                f"--save-baseline={BASELINE}",
                "--save-summary=json",
            )
            base_ids = {row["key"] for row in collect(home)}
            # A head run that never reaches a bench must not leave the
            # base's summary to be read as the head's.
            for path in home.glob("**/summary.json"):
                path.unlink()
        finally:
            git(repo, "worktree", "remove", "--force", str(worktree), check=False)
        failed = {package: rc for package, rc in base_rcs.items() if rc != 0}
        if failed:
            print(
                f"bench-gate-ir: benching the base failed ({failed})", file=sys.stderr
            )
            return 1

        head_rcs = run_benches(
            repo,
            target_dir,
            home,
            list(GATES),
            f"--baseline={BASELINE}",
            f"--callgrind-limits=ir={args.threshold}%",
            "--save-summary=json",
        )
        rows = collect(home)
        print()
        print(render(rows))
        print()
        return verdict(
            rows, base_ids, head_rcs, label_accepts(args.labels), args.threshold
        )
    finally:
        shutil.rmtree(scratch, ignore_errors=True)
        git(repo, "worktree", "prune", check=False)


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(prog="gungraun.py", description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    gate = sub.add_parser("gate", help="compare the checkout with a base revision")
    gate.add_argument("--base", required=True, metavar="REV")
    gate.add_argument(
        "--threshold",
        type=float,
        default=DEFAULT_THRESHOLD_PCT,
        metavar="PCT",
        help=f"Ir increase in percent that fails a bench (default {DEFAULT_THRESHOLD_PCT})",
    )
    gate.add_argument(
        "--labels",
        default=os.environ.get("PR_LABELS", ""),
        metavar="JSON",
        help="the event payload's pull_request.labels (env PR_LABELS)",
    )
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    repo = Path(__file__).resolve().parent.parent.parent
    if args.command == "gate":
        return cmd_gate(args, repo)
    raise AssertionError(f"unhandled command {args.command}")


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
