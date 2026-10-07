#!/usr/bin/env bash
# Capture a samply CPU profile of the felis daemon while it parses a
# benchmark stream, then leave the .json.gz for symbolize.sh.
#
# Usage:
#   samply-profile.sh [benchmark] [seconds]   # default: unicode 16
# Env:
#   FELIS_BIN   path to felis client (default ./target/release/felis)
#   FELIS_DAEMON_BIN  felis-daemon binary (default: FELIS_BIN's sibling)
#   OUT         output profile path (default /tmp/felis_prof.json.gz)
#
# WHY samply launches the daemon itself, and the client only connects:
#   * samply needs kernel.perf_event_paranoid <= 1 (default 2 rejects it).
#     Set it once: `echo 1 | sudo tee /proc/sys/kernel/perf_event_paranoid`.
#   * Even at paranoid=1, `samply record --pid <daemon>` fails with
#     "mmap failed" here: per-cpu ring buffers blow past
#     kernel.perf_event_mlock_kb (516) x ncpus. Raising it needs sudo.
#     samply's *launch* mode uses cheap per-thread events instead.
#   * Launching the client under samply misses the daemon on a Linux
#     desktop with a systemd user manager: autospawn hands it to
#     systemd-run, outside samply's process tree.
#   * The session shell comes from the client's environment, not the
#     daemon's, so SHELL goes on the client.
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../isolated-daemon/scripts/isolated-daemon.sh"

FELIS_BIN="${FELIS_BIN:-./target/release/felis}"
FELIS_DAEMON_BIN="${FELIS_DAEMON_BIN:-$(dirname "$FELIS_BIN")/felis-daemon}"
OUT="${OUT:-/tmp/felis_prof.json.gz}"
BENCH="${1:-unicode}"
SECS="${2:-16}"

felis_dbg_socket felis-prof || exit 1
SOCKDIR="$FELIS_DBG_DIR"
SOCK="$FELIS_DBG_SOCK"
WRAP="$(mktemp)"

CLIENT_LOG="${OUT%.json.gz}.client.log"
STARTED="$SOCKDIR/bench.started"

# Loop the benchmark so the daemon stays busy for the whole window.
{
  echo '#!/usr/bin/env bash'
  echo 'sleep 1.5'
  echo "kitten __benchmark__ --repetitions 20 $BENCH >/dev/null 2>&1 && touch $(printf %q "$STARTED")"
  echo 'for i in $(seq 1 40); do'
  echo "  kitten __benchmark__ --repetitions 60 $BENCH >/dev/null 2>&1"
  echo 'done'
} > "$WRAP"
chmod +x "$WRAP"

ours_alive() {
  [ -n "$(felis_dbg_pids "$SOCK" felis-daemon)" ]
}

stop_ours() {
  felis_dbg_stop "$SOCK" "$FELIS_BIN"
}

# A client that outlives the daemon autospawns a replacement outside samply.
CLIENT=
stop_client() {
  if [ -n "$CLIENT" ] && kill -0 "$CLIENT" 2>/dev/null; then
    kill "$CLIENT"
    wait "$CLIENT" 2>/dev/null
  fi
}

SAMPLY=
cleanup() {
  stop_client
  stop_ours
  if [ -n "$SAMPLY" ] && kill -0 "$SAMPLY" 2>/dev/null; then
    kill "$SAMPLY"
    wait "$SAMPLY" 2>/dev/null
  fi
  rm -rf "$WRAP" "$SOCKDIR"
}
trap cleanup EXIT

rm -f "$OUT"
# An inherited NOTIFY_SOCKET makes `serve` report readiness to a
# manager that is not there, and exit right after its bind when that fails.
RUST_LOG=warn env -u NOTIFY_SOCKET \
  samply record --save-only -n -o "$OUT" -r 1999 -d "$SECS" \
  -- "$FELIS_DAEMON_BIN" serve --socket "$SOCK" >/tmp/samply.log 2>&1 &
SAMPLY=$!

if ! felis_dbg_wait "$SOCK" "$SAMPLY" || ! ours_alive; then
  echo "the daemon is not serving $SOCK; check /tmp/samply.log (paranoid<=1 set?)"
  tail -3 /tmp/samply.log
  exit 1
fi

RUST_LOG=warn SHELL="$WRAP" "$FELIS_BIN" --socket "$SOCK" >"$CLIENT_LOG" 2>&1 &
CLIENT=$!

# samply -d stops sampling after SECS but waits for the daemon to exit
# before writing. Give the bench its window, then stop the daemon so
# samply finalizes.
sleep "$(( SECS + 1 ))"
stop_client
stop_ours
wait "$SAMPLY"
STATUS=$?

# A client that never ran a benchmark leaves an idle daemon's profile.
if [ ! -e "$STARTED" ]; then
  rm -f "$OUT"
  echo "no benchmark completed in the felis window (is kitten on PATH?); see $CLIENT_LOG"
  tail -5 "$CLIENT_LOG"
  exit 1
fi
if [ "$STATUS" -ne 0 ] || ! gzip -t "$OUT" 2>/dev/null; then
  echo "no valid profile (samply exited $STATUS); check /tmp/samply.log (paranoid<=1 set?)"
  tail -3 /tmp/samply.log
  exit 1
fi
echo "profile written: $OUT"
echo "next: scripts/symbolize.sh $OUT felis-pty-parser $FELIS_DAEMON_BIN"
