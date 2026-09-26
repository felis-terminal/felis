#!/usr/bin/env bash
# Capture a samply CPU profile of felis on macOS, driving one of three
# workloads inside the profiled window. Differences from the Linux
# samply-profile.sh: no perf_event_paranoid gate, socket in a dedicated
# $TMPDIR subdir (Listener::bind refuses any parent that is not a 0700
# directory you own, /tmp and the $TMPDIR root included), ps-based cleanup
# (no /proc), and --unstable-presymbolicate for a .syms.json sidecar
# (addr2line cannot read Mach-O) — read the pair with prof-top.py or
# prof-flame.py.
#
# Usage:
#   samply-macos.sh kitten [benchmark] [secs]    # default: csi 16
#   samply-macos.sh flood <payload-file> [secs]  # arbitrary bytes via cat
#   samply-macos.sh tb [secs]                    # termbench-pro
#
# Env: FELIS_BIN (./target/release/felis), OUT (/tmp/felis_prof.json.gz),
#      SIZE_MB (tb payload size, 32)
#
# `flood` exists because termbench-pro cannot run a single category and
# kitten's benchmarks take no custom payload — generate the exact byte
# shape under investigation (per-cell SGR, cursor-move storms) into a
# file and point this at it. `tb` covers all five categories at once;
# in the flamegraph the SGR categories still separate from the
# print/scroll ones by their CSI-dispatch frames.
set -u

FELIS_BIN="${FELIS_BIN:-./target/release/felis}"
OUT="${OUT:-/tmp/felis_prof.json.gz}"
MODE="${1:?usage: samply-macos.sh kitten|flood|tb ...}"
shift

case "$MODE" in
kitten)
  BENCH="${1:-csi}"
  SECS="${2:-16}"
  BODY=$(printf '%s\n' \
    "kitten __benchmark__ --repetitions 20 $BENCH >/dev/null 2>&1" \
    'for i in $(seq 1 40); do' \
    "  kitten __benchmark__ --repetitions 60 $BENCH >/dev/null 2>&1" \
    'done')
  ;;
flood)
  PAYLOAD="${1:?usage: samply-macos.sh flood <payload-file> [secs]}"
  SECS="${2:-16}"
  [ -r "$PAYLOAD" ] || { echo "payload not readable: $PAYLOAD" >&2; exit 1; }
  BODY=$(printf '%s\n' \
    'for i in $(seq 1 200); do' \
    "  cat '$PAYLOAD'" \
    'done')
  ;;
tb)
  SECS="${1:-16}"
  SIZE_MB="${SIZE_MB:-32}"
  # Same `tb` the comparison suite uses: the `.#bench` shell exports a
  # wrapped one (the nixpkgs darwin build's install_name is broken; see
  # dev/bench/devshell.nix), so profiling and comparing never end
  # up measuring two different binaries.
  TB_BIN="${TB_BIN:-${FELIS_BENCH_TB:-$(command -v tb || true)}}"
  [ -x "$TB_BIN" ] || {
    echo "termbench-pro (tb) not found: '$TB_BIN' (nix develop .#bench)" >&2
    exit 1
  }
  BODY=$(printf '%s\n' \
    'for i in $(seq 1 40); do' \
    "  '$TB_BIN' --size $SIZE_MB" \
    'done')
  ;;
*)
  echo "unknown workload: $MODE (kitten|flood|tb)" >&2
  exit 1
  ;;
esac

SOCKDIR="$(mktemp -d "${TMPDIR:-/tmp}/felis-prof.XXXXXX")"
SOCK="$SOCKDIR/felis_prof_$$.sock"
WRAP="$(mktemp)"

{
  echo '#!/usr/bin/env bash'
  echo 'sleep 1.5'
  printf '%s\n' "$BODY"
} > "$WRAP"
chmod +x "$WRAP"

# Every process is matched on the FULL socket path: `pkill -x felis-client`
# / `pkill -x felis` end every felis window and CLI on the machine, and a
# basename match would reach the user's real daemon (hence the per-run
# socket name above, not `daemon.sock`).
kill_ours() {
  "$FELIS_BIN" --socket "$SOCK" daemon stop --force >/dev/null 2>&1
  for p in $(pgrep -x felis-daemon) $(pgrep -x felis-client); do
    /bin/ps -o command= -p "$p" 2>/dev/null | tr ' ' '\n' | grep -qxF -- "$SOCK" && kill "$p"
  done
}
cleanup() {
  kill_ours
  rm -rf "$WRAP" "$SOCKDIR"
}
trap cleanup EXIT

rm -f "$OUT" "${OUT%.json.gz}.syms.json"
RUST_LOG=warn SHELL="$WRAP" \
  samply record --save-only -n --unstable-presymbolicate -o "$OUT" \
  -r 1999 -d "$SECS" \
  -- "$FELIS_BIN" --socket "$SOCK" >/tmp/samply.log 2>&1 &

sleep "$((SECS + 3))"
kill_ours
sleep 3

if [ -f "$OUT" ]; then
  echo "profile written: $OUT"
  ls -la "${OUT%.json.gz}"* 2>/dev/null
else
  echo "no profile written; check /tmp/samply.log"
  tail -5 /tmp/samply.log
fi
