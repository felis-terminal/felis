#!/usr/bin/env python3
"""Collapsed stacks (inferno/FlameGraph format) from a samply profile
plus its .syms.json sidecar.

Usage: prof_flame.py <profile.json[.gz]> <syms.json> <thread-name-substr> [pid-substr]

Pipe the output to `inferno-flamegraph` (nixpkgs `inferno`) for an
SVG. Exists because samply's own flamegraph view lives in the Firefox
Profiler web UI — fine interactively (`samply load`), useless when the
deliverable is a file. Same address resolution as prof-top.py: macOS
addr2line can't read Mach-O, so symbols come from the
--unstable-presymbolicate sidecar.
"""

import bisect
import gzip
import json
import sys
from collections import Counter

prof_path, syms_path, thread_sub = sys.argv[1], sys.argv[2], sys.argv[3]
pid_sub = sys.argv[4] if len(sys.argv) > 4 else None

opener = gzip.open if prof_path.endswith(".gz") else open
with opener(prof_path, "rt") as f:
    prof = json.load(f)
with open(syms_path) as f:
    syms = json.load(f)

strtab = syms["string_table"]
libsym = {}
for ent in syms["data"]:
    tbl = sorted(
        (e["rva"], e["size"], strtab[e["symbol"]]) for e in ent["symbol_table"]
    )
    libsym[ent["debug_name"]] = ([t[0] for t in tbl], tbl)

libs = prof["libs"]


def resolve(libidx, addr):
    libname = libs[libidx]["debugName"] if libidx is not None and libidx >= 0 else "?"
    if libname in libsym and addr is not None:
        rvas, tbl = libsym[libname]
        i = bisect.bisect_right(rvas, addr) - 1
        if i >= 0:
            rva, size, name = tbl[i]
            if size == 0 or addr < rva + size:
                return name
    # Foreign libs (libsystem etc.) have no sidecar entry; the lib name
    # is more readable in a flamegraph than a raw address.
    return f"[{libname}]" if addr is not None else "?"


threads = [
    t
    for t in prof["threads"]
    if thread_sub in t["name"] and (pid_sub is None or pid_sub in str(t["pid"]))
]
threads.sort(key=lambda t: -t["samples"]["length"])
t = threads[0]
print(
    f"thread: {t['name']} pid={t['pid']} samples={t['samples']['length']}",
    file=sys.stderr,
)

ft, st, fr = t["frameTable"], t["stackTable"], t["funcTable"]
rt = t["resourceTable"]


def frame_name(frame_idx):
    func = ft["func"][frame_idx]
    addr = ft["address"][frame_idx]
    res = fr["resource"][func]
    libidx = rt["lib"][res] if res is not None and res >= 0 else None
    return resolve(libidx, addr if addr is not None and addr >= 0 else None)


# stack index -> root-first frame list, memoized via the prefix chain.
stack_names = {}


def names_for(stack):
    if stack is None:
        return []
    got = stack_names.get(stack)
    if got is None:
        got = names_for(st["prefix"][stack]) + [frame_name(st["frame"][stack])]
        stack_names[stack] = got
    return got


sys.setrecursionlimit(100_000)
counts = Counter()
for stack in t["samples"]["stack"]:
    if stack is None:
        continue
    counts[";".join(names_for(stack))] += 1

for line, n in counts.items():
    print(f"{line} {n}")
