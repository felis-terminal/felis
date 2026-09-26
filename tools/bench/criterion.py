#!/usr/bin/env python3
"""felis benchmark orchestrator.

Single entry point for every Criterion procedure so the local recipes
(`just bench*`), the per-PR regression gate, and the nightly snapshot
all share one set of flags — divergent hand-rolled `cargo bench`
invocations is how baseline comparisons stop being like-for-like.

Python over shell: the inputs and outputs here are JSON (cargo
metadata, Criterion's estimates.json / benchmark.json), and the bash
predecessor (`bench-gate.sh`) already needed jq + awk + globstar for a
fraction of this; stdlib-only Python keeps it one pinned runtime
(`python3` in the dev shell, flake.nix) with testable functions
(`tools/bench/criterion_test.py`).

Commands
  list    discover Criterion bench targets via `cargo metadata`
  run     run benches with consistent flags; optionally emit a report
  report  summarize target/criterion/**/new/estimates.json
  gate    fail on regressions vs a saved baseline (CI gate)

The gate preserves the bench-gate.sh contract: env vars THRESHOLD /
CRITERION_ROOT / ALLOWLIST and exit codes 0 (clean), 1 (regression),
2 (nothing to compare — `--baseline` was probably not passed).
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time
from pathlib import Path

# Criterion writes under cargo's target directory, which CI parks outside
# the workspace (.forgejo/actions/cargo-env), so a literal "target" would
# name a directory the benches never wrote to and the gate would read an
# empty run as nothing to check.
DEFAULT_ROOT = Path(os.environ.get("CARGO_TARGET_DIR") or "target") / "criterion"
# 15% default: free-CI runner noise adds ~5% jitter to small benches,
# so the floor sits where a real regression dominates the noise.
# Tighten via THRESHOLD when the runner stabilises (bench.yml env).
DEFAULT_THRESHOLD = 0.15


# ── discovery ────────────────────────────────────────────────────────


def bench_targets(metadata: dict) -> list[dict]:
    """Extract Criterion bench targets from `cargo metadata` output.

    Returns [{package, name, required_features}] sorted by (package,
    name). Discovery over a hand-maintained list: adding a bench to a
    crate's Cargo.toml must not require touching this script or the
    workflow (the fuzz workflow auto-discovers targets for the same
    reason).
    """
    targets = []
    for pkg in metadata["packages"]:
        for tgt in pkg["targets"]:
            if "bench" not in tgt["kind"]:
                continue
            targets.append(
                {
                    "package": pkg["name"],
                    "name": tgt["name"],
                    "required_features": tgt.get("required-features", []),
                }
            )
    return sorted(targets, key=lambda t: (t["package"], t["name"]))


def load_metadata(manifest_dir: Path) -> dict:
    out = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        cwd=manifest_dir,
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    return json.loads(out)


# ── result collection ────────────────────────────────────────────────


def collect_results(root: Path, since: float | None = None) -> list[dict]:
    """Read every `new/estimates.json` under `root` into flat records.

    `since` filters by mtime so a `run` invocation reports only the
    benches it refreshed — `target/criterion` accumulates results from
    every bench ever run locally, and reporting stale neighbors as if
    this run produced them would make two snapshots incomparable.
    """
    results = []
    for estimates_path in sorted(root.glob("**/new/estimates.json")):
        if since is not None and estimates_path.stat().st_mtime < since:
            continue
        estimates = json.loads(estimates_path.read_text())
        bench_dir = estimates_path.parent
        bench_id = str(bench_dir.parent.relative_to(root))
        throughput = None
        benchmark_path = bench_dir / "benchmark.json"
        if benchmark_path.exists():
            benchmark = json.loads(benchmark_path.read_text())
            bench_id = benchmark.get("full_id", bench_id)
            throughput = benchmark.get("throughput")
        mean_ns = estimates["mean"]["point_estimate"]
        results.append(
            {
                "id": bench_id,
                "mean_ns": mean_ns,
                "std_err_ns": estimates["mean"]["standard_error"],
                "throughput": throughput_rate(throughput, mean_ns),
            }
        )
    return sorted(results, key=lambda r: r["id"])


def throughput_rate(throughput: dict | None, mean_ns: float) -> dict | None:
    """Convert Criterion's per-iteration throughput into a rate.

    Criterion stores the *workload size* per iteration ({"Bytes": n} or
    {"Elements": n}); the human-meaningful number is the rate, which is
    what Criterion's own console output prints (MiB/s, Melem/s).
    """
    if not throughput or mean_ns <= 0:
        return None
    if "Bytes" in throughput:
        mib_s = throughput["Bytes"] / (1 << 20) / (mean_ns / 1e9)
        return {"unit": "MiB/s", "rate": mib_s}
    if "Elements" in throughput:
        melem_s = throughput["Elements"] / 1e6 / (mean_ns / 1e9)
        return {"unit": "Melem/s", "rate": melem_s}
    return None


def format_ns(ns: float) -> str:
    for limit, divisor, unit in ((1e3, 1, "ns"), (1e6, 1e3, "µs"), (1e9, 1e6, "ms")):
        if ns < limit:
            return f"{ns / divisor:.2f} {unit}"
    return f"{ns / 1e9:.2f} s"


def render_markdown(results: list[dict]) -> str:
    lines = [
        "| bench | mean | throughput |",
        "| --- | ---: | ---: |",
    ]
    for r in results:
        tp = (
            f"{r['throughput']['rate']:.1f} {r['throughput']['unit']}"
            if r["throughput"]
            else ""
        )
        lines.append(f"| `{r['id']}` | {format_ns(r['mean_ns'])} | {tp} |")
    return "\n".join(lines) + "\n"


# ── gate ─────────────────────────────────────────────────────────────


def gate(root: Path, threshold: float, allowlist: list[str]) -> int:
    """Walk `**/change/estimates.json`; fail past-threshold regressions.

    A point estimate of -0.05 means 5% faster than the baseline; +0.15
    means 15% slower. Gates on the point estimate, not the CI upper
    bound: gating on the upper bound double-counts noise that the
    threshold margin already absorbs.
    """
    if not root.is_dir():
        print(f"bench-gate: {root} does not exist; nothing to check", file=sys.stderr)
        return 2
    files = sorted(root.glob("**/change/estimates.json"))
    if not files:
        print(
            f"bench-gate: no change/estimates.json under {root}"
            " — run benches with --baseline first",
            file=sys.stderr,
        )
        return 2

    regressions = 0
    total = 0
    for f in files:
        bench = str(f.parent.parent.relative_to(root))
        if bench in allowlist:
            print(f"bench-gate: skip {bench} (allowlisted)")
            continue
        total += 1
        estimates = json.loads(f.read_text())
        point = estimates["mean"]["point_estimate"]
        upper = estimates["mean"]["confidence_interval"]["upper_bound"]
        if point > threshold:
            print(
                f"REGRESSED  {bench:<60}  mean={point * 100:+.1f}%"
                f" (CI upper {upper * 100:+.1f}%)"
            )
            regressions += 1

    if regressions:
        print(
            f"\nbench-gate: {regressions}/{total} benches regressed past"
            f" threshold ({threshold * 100:.0f}%)",
            file=sys.stderr,
        )
        return 1
    print(
        f"bench-gate: all {total} bench(es) within {threshold * 100:.0f}% of baseline"
    )
    return 0


# ── run ──────────────────────────────────────────────────────────────


def cargo_bench_cmd(target: dict, args: argparse.Namespace) -> list[str]:
    cmd = ["cargo", "bench", "-p", target["package"], "--bench", target["name"]]
    if target["required_features"]:
        cmd += ["--features", ",".join(target["required_features"])]
    cmd.append("--")
    if args.quick:
        # Matches the CI gate's budget: keeps each bench group in
        # single-digit seconds at the cost of wider confidence
        # intervals — acceptable because the gate threshold (15%)
        # is far above the extra variance.
        cmd += ["--warm-up-time", "1", "--measurement-time", "3"]
    if args.save_baseline:
        cmd += ["--save-baseline", args.save_baseline]
    if args.baseline:
        cmd += ["--baseline", args.baseline]
    if args.filter:
        # Criterion's harness takes a single positional FILTER (a regex)
        # and rejects a second one as an unexpected argument, so repeated
        # `--filter` values join into one alternation.
        cmd.append("|".join(args.filter))
    return cmd


def cmd_run(args: argparse.Namespace, repo_root: Path) -> int:
    targets = bench_targets(load_metadata(repo_root))
    if args.targets:
        by_name = {t["name"]: t for t in targets}
        unknown = [n for n in args.targets if n not in by_name]
        if unknown:
            known = ", ".join(sorted(by_name))
            print(
                f"bench: unknown target(s) {unknown}; known: {known}", file=sys.stderr
            )
            return 2
        targets = [by_name[n] for n in args.targets]

    started = time.time()
    failed = []
    for target in targets:
        cmd = cargo_bench_cmd(target, args)
        print(f"::: {target['package']} / {target['name']}", flush=True)
        # One cargo invocation per target, not one combined call: a
        # single hung or panicking bench (e.g. a font probe on a bare
        # runner) must not take the rest of the suite's numbers with it.
        proc = subprocess.run(cmd, cwd=repo_root)
        if proc.returncode != 0:
            failed.append(f"{target['package']}/{target['name']}")

    results = collect_results(repo_root / args.root, since=started)
    if args.json_out:
        Path(args.json_out).write_text(json.dumps(results, indent=2) + "\n")
    if args.markdown_out:
        # Append, not truncate: $GITHUB_STEP_SUMMARY is shared by every
        # step in the job.
        with open(args.markdown_out, "a") as f:
            f.write(render_markdown(results))
    if not args.json_out and not args.markdown_out:
        print()
        print(render_markdown(results), end="")

    if failed:
        print(
            f"\nbench: {len(failed)} target(s) failed: {', '.join(failed)}",
            file=sys.stderr,
        )
        return 1
    return 0


# ── CLI ──────────────────────────────────────────────────────────────


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(prog="bench.py", description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    sub.add_parser("list", help="print discovered bench targets")

    run = sub.add_parser("run", help="run Criterion benches")
    run.add_argument("targets", nargs="*", help="bench target names (default: all)")
    run.add_argument(
        "--quick", action="store_true", help="CI-budget warm-up/measurement times"
    )
    run.add_argument("--save-baseline", metavar="NAME")
    run.add_argument("--baseline", metavar="NAME")
    run.add_argument(
        "--filter",
        action="append",
        default=[],
        metavar="EXPR",
        help="Criterion id filter, repeatable (positional after --)",
    )
    run.add_argument("--json-out", metavar="PATH", help="write results JSON")
    run.add_argument("--markdown-out", metavar="PATH", help="append a markdown table")
    run.add_argument("--root", default=str(DEFAULT_ROOT), help=argparse.SUPPRESS)

    report = sub.add_parser("report", help="summarize existing Criterion results")
    report.add_argument("--root", default=str(DEFAULT_ROOT))
    report.add_argument("--format", choices=("markdown", "json"), default="markdown")

    gate_p = sub.add_parser("gate", help="fail on regressions vs a baseline")
    gate_p.add_argument(
        "--threshold",
        type=float,
        default=float(os.environ.get("THRESHOLD", DEFAULT_THRESHOLD)),
        help="fractional regression that fails the gate (env THRESHOLD)",
    )
    gate_p.add_argument(
        "--root",
        default=os.environ.get("CRITERION_ROOT", str(DEFAULT_ROOT)),
        help="criterion output root (env CRITERION_ROOT)",
    )
    gate_p.add_argument(
        "--allow",
        action="append",
        default=None,
        metavar="BENCH_ID",
        help="bench id to skip, repeatable (env ALLOWLIST, newline-separated)",
    )

    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    repo_root = Path(__file__).resolve().parent.parent

    if args.command == "list":
        for t in bench_targets(load_metadata(repo_root)):
            feats = (
                f" (--features {','.join(t['required_features'])})"
                if t["required_features"]
                else ""
            )
            print(f"{t['package']}  {t['name']}{feats}")
        return 0

    if args.command == "run":
        return cmd_run(args, repo_root)

    if args.command == "report":
        results = collect_results(Path(args.root))
        if args.format == "json":
            print(json.dumps(results, indent=2))
        else:
            print(render_markdown(results), end="")
        return 0

    if args.command == "gate":
        allowlist = args.allow
        if allowlist is None:
            allowlist = [l for l in os.environ.get("ALLOWLIST", "").splitlines() if l]
        return gate(Path(args.root), args.threshold, allowlist)

    raise AssertionError(f"unhandled command {args.command}")


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
