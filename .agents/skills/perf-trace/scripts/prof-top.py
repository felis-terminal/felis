#!/usr/bin/env python3
"""Flat self/total-time table from a samply profile + .syms.json sidecar.

Usage: prof_top.py <profile.json[.gz]> <syms.json> <thread-name-substr> [pid-substr]
Aggregates leaf frames (self) and on-stack presence (total) per symbol.
"""

import bisect, gzip, json, sys
from collections import Counter

prof_path, syms_path, thread_sub = sys.argv[1], sys.argv[2], sys.argv[3]
pid_sub = sys.argv[4] if len(sys.argv) > 4 else None

opener = gzip.open if prof_path.endswith(".gz") else open
with opener(prof_path, "rt") as f:
    prof = json.load(f)
with open(syms_path) as f:
    syms = json.load(f)

strtab = syms["string_table"]
# debug_name -> (sorted rva list, [(rva, size, name)])
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
                return libname, name
    return libname, f"0x{addr:x}" if addr is not None else "?"


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


def frame_info(frame_idx):
    func = ft["func"][frame_idx]
    addr = ft["address"][frame_idx]
    res = fr["resource"][func]
    libidx = rt["lib"][res] if res is not None and res >= 0 else None
    return resolve(libidx, addr if addr is not None and addr >= 0 else None)


self_c, total_c = Counter(), Counter()
nsamples = 0
for stack in t["samples"]["stack"]:
    if stack is None:
        continue
    nsamples += 1
    lib, name = frame_info(st["frame"][stack])
    self_c[(lib, name)] += 1
    seen = set()
    s = stack
    while s is not None:
        key = frame_info(st["frame"][s])
        if key not in seen:
            seen.add(key)
            total_c[key] += 1
        s = st["prefix"][s]

print(f"\n== top self ({nsamples} samples) ==")
for (lib, name), n in self_c.most_common(30):
    print(f"{n:7d} {100 * n / nsamples:5.1f}%  [{lib}] {name}")
print(f"\n== top total ==")
for (lib, name), n in total_c.most_common(40):
    print(f"{n:7d} {100 * n / nsamples:5.1f}%  [{lib}] {name}")
