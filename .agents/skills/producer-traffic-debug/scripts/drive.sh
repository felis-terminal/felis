#!/usr/bin/env bash
# Drive felis unattended to observe REAL producer traffic (yazi,
# presenterm, timg, hand-rolled Kitty escapes) on Linux or macOS,
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
. "$(dirname "${BASH_SOURCE[0]}")/../../isolated-daemon/scripts/isolated-daemon.sh"

FELIS_BIN="${FELIS_BIN:-./target/release/felis}"
FELIS_DAEMON_BIN="${FELIS_DAEMON_BIN:-./target/release/felis-daemon}"
RUST_LOG="${RUST_LOG:-info,felis=debug,felis_daemon=debug}"
LOG="${LOG:-/tmp/felis.log}"
CMD="${1:?usage: drive.sh '<command>' [hold_seconds]}"
HOLD="${2:-6}"

# Reusing a socket connects to a stale daemon: no fresh logs.
felis_dbg_socket felis-dbg || exit 1
SOCKDIR="$FELIS_DBG_DIR"
SOCK="$FELIS_DBG_SOCK"
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
cleanup() {
  if [ -n "$FP" ] && kill "$FP" 2>/dev/null; then
    wait "$FP" 2>/dev/null
  fi
  felis_dbg_stop "$SOCK"
  rm -rf "$WRAP" "$SOCKDIR"
}
trap cleanup EXIT

: > "$LOG"
# Started here rather than auto-spawned: an auto-spawned daemon's console
# is /dev/null or the journal, so its lines would never reach $LOG.
# An inherited NOTIFY_SOCKET makes `serve` report readiness to a
# manager that is not there, and exit right after its bind when that fails.
RUST_LOG="$RUST_LOG" SHELL="$WRAP" env -u NOTIFY_SOCKET "$FELIS_DAEMON_BIN" serve --socket "$SOCK" >>"$LOG" 2>&1 &
if ! felis_dbg_wait "$SOCK" $!; then
  echo "the daemon is not serving $SOCK; see $LOG"
  exit 1
fi
RUST_LOG="$RUST_LOG" SHELL="$WRAP" "$FELIS_BIN" --socket "$SOCK" >>"$LOG" 2>&1 &
FP=$!
sleep "$(( HOLD + 3 ))"

echo "log: $LOG  (grep the signal lines, not a screenshot)"
echo "--- graphics event flow / image arrival / placeholder pass ---"
grep -aE 'dispatch_apc_bodies|image header|placeholder pass' "$LOG" | tail -20
