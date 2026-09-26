#!/usr/bin/env bash
# Drive felis unattended to observe REAL producer traffic (yazi,
# presenterm, timg, hand-rolled Kitty escapes) on the Linux/Wayland box,
# capturing both the client's and a foreground daemon's logs.
#
# Usage:
#   drive.sh '<command to run in the felis window>' [hold_seconds]
# Examples:
#   drive.sh 'yazi /some/dir' 6
#   drive.sh 'timg -p kitty x.png'
#   drive.sh "printf '\e_Ga=T,f=100,...\e\\'"
# Env:
#   FELIS_BIN  felis client binary (default ./target/release/felis)
#   FELIS_DAEMON_BIN  daemon binary (default ./target/release/felis-daemon)
#   RUST_LOG   (default info,felis=debug,felis_daemon=debug)
#   LOG        log file (default /tmp/felis.log)
set -u

FELIS_BIN="${FELIS_BIN:-./target/release/felis}"
FELIS_DAEMON_BIN="${FELIS_DAEMON_BIN:-./target/release/felis-daemon}"
RUST_LOG="${RUST_LOG:-info,felis=debug,felis_daemon=debug}"
LOG="${LOG:-/tmp/felis.log}"
CMD="${1:?usage: drive.sh '<command>' [hold_seconds]}"
HOLD="${2:-6}"

# Unique socket so the window attaches to the daemon this script starts.
# Reusing one connects to a stale daemon — no fresh logs. A dedicated
# directory: bind requires a 0700 parent this uid owns, so a socket
# straight under /tmp is refused.
SOCKDIR="$(mktemp -d "${TMPDIR:-/tmp}/felis-dbg.XXXXXX")"
SOCK="$SOCKDIR/felis_dbg_$$.sock"
WRAP="$(mktemp)"

# The wrapper IS the PTY shell: let the window map, run the producer,
# hold the frame, then exit so felis closes.
{
  echo '#!/usr/bin/env bash'
  echo 'sleep 1'
  echo "$CMD"
  echo "sleep $HOLD"
} > "$WRAP"
chmod +x "$WRAP"

FP=
DP=
cleanup() {
  # The client goes first: one that outlives its daemon reconnects, and
  # the reconnect autospawns a replacement on the same socket.
  if [ -n "$FP" ] && kill "$FP" 2>/dev/null; then
    wait "$FP" 2>/dev/null
  fi
  [ -n "$DP" ] && kill "$DP" 2>/dev/null
  # Kill the daemon by pid filtered on the FULL socket path. Never
  # `pkill -f <socket>` — it matches this script's own argv (exit 144) —
  # and never a basename match, which also matches the user's real daemon.
  for p in $(pgrep -x felis-daemon); do
    tr '\0' '\n' < "/proc/$p/cmdline" 2>/dev/null | grep -qxF -- "$SOCK" && kill "$p"
  done
  rm -rf "$WRAP" "$SOCKDIR"
}
trap cleanup EXIT

: > "$LOG"
# Started here rather than auto-spawned: an auto-spawned daemon's console
# is /dev/null or the journal, so its lines would never reach $LOG.
# An inherited NOTIFY_SOCKET makes `serve` report readiness to a
# manager that is not there, and exit right after its bind when that fails.
RUST_LOG="$RUST_LOG" SHELL="$WRAP" env -u NOTIFY_SOCKET "$FELIS_DAEMON_BIN" serve --socket "$SOCK" >>"$LOG" 2>&1 &
DP=$!
# A client that dials a missing socket autospawns a second daemon.
for _ in $(seq 100); do
  [ -S "$SOCK" ] && break
  kill -0 "$DP" 2>/dev/null || break
  sleep 0.05
done
sleep 0.2
if [ ! -S "$SOCK" ] || ! kill -0 "$DP" 2>/dev/null; then
  echo "the daemon is not serving $SOCK; see $LOG"
  exit 1
fi
RUST_LOG="$RUST_LOG" SHELL="$WRAP" "$FELIS_BIN" --socket "$SOCK" >>"$LOG" 2>&1 &
FP=$!
sleep "$(( HOLD + 3 ))"

echo "log: $LOG  (grep the signal lines, not a screenshot)"
echo "--- graphics event flow / image arrival / placeholder pass ---"
grep -aE 'dispatch_apc_bodies|image header|placeholder pass' "$LOG" | tail -20
