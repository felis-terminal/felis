#!/usr/bin/env bash
# A private felis daemon on a per-run socket, and a teardown that cannot
# reach the user's own daemon, windows, or CLI streams.
#
# Run it:
#   isolated-daemon.sh start         # prints SOCK=… LOG=…, daemon keeps running
#   isolated-daemon.sh stop <sock>   # clients first, then the daemon, then the dir
# Or source it from bash for the felis_dbg_* functions.
# Env (start):
#   FELIS_DAEMON_BIN  daemon binary (default ./target/release/felis-daemon)
#   RUST_LOG          (default info,felis_daemon=debug)

# Under /tmp, not $TMPDIR: an agent sandbox's TMPDIR can push the socket
# path past SUN_LEN (104 bytes on macOS), and bind only reports that in the
# daemon log. mktemp -d gives the 0700 parent bind insists on.
felis_dbg_socket() { # [prefix] -> FELIS_DBG_DIR, FELIS_DBG_SOCK
  FELIS_DBG_DIR="$(mktemp -d "/tmp/${1:-felis-dbg}.XXXXXX")" || return 1
  FELIS_DBG_SOCK="$FELIS_DBG_DIR/d.sock"
}

# One whole argv element equal to the full socket path. A basename or
# substring match also hits the user's real daemon (its argv carries a
# `.sock` too), and `pkill -f <sock>` matches the calling shell's own argv.
felis_dbg_pids() { # <sock> <process name>...
  local sock="$1" name p
  shift
  for name in "$@"; do
    for p in $(pgrep -x "$name"); do
      if [ -r "/proc/$p/cmdline" ]; then
        tr '\0' '\n' <"/proc/$p/cmdline"
      else
        /bin/ps -o command= -p "$p" | tr ' ' '\n'
      fi 2>/dev/null | grep -qxF -- "$sock" && echo "$p"
    done
  done
}

# A client that dials before the bind autospawns a second daemon outside
# the caller's control (and outside samply's process tree).
felis_dbg_wait() { # <sock> <pid of the process that binds it>
  local _
  for _ in $(seq 100); do
    [ -S "$1" ] && break
    kill -0 "$2" 2>/dev/null || break
    sleep 0.05
  done
  sleep 0.2
  [ -S "$1" ] && kill -0 "$2" 2>/dev/null
}

# Clients go first: one that outlives its daemon reconnects, and the
# reconnect autospawns a replacement on the same socket.
felis_dbg_stop() { # <sock> [felis front-door binary, for a clean `daemon stop`]
  local p
  for p in $(felis_dbg_pids "$1" felis-client); do
    kill "$p" 2>/dev/null
  done
  sleep 0.3
  [ -n "${2:-}" ] && [ -x "$2" ] && "$2" --socket "$1" daemon stop --force >/dev/null 2>&1
  for p in $(felis_dbg_pids "$1" felis-daemon); do
    kill "$p" 2>/dev/null
  done
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  set -u
  case "${1:-}" in
  start)
    felis_dbg_socket || exit 1
    LOG="$FELIS_DBG_DIR/daemon.log"
    # An inherited NOTIFY_SOCKET makes `serve` report readiness to a
    # manager that is not there, and exit right after its bind when that fails.
    RUST_LOG="${RUST_LOG:-info,felis_daemon=debug}" env -u NOTIFY_SOCKET \
      "${FELIS_DAEMON_BIN:-./target/release/felis-daemon}" serve --socket "$FELIS_DBG_SOCK" >"$LOG" 2>&1 &
    if ! felis_dbg_wait "$FELIS_DBG_SOCK" $!; then
      echo "the daemon is not serving $FELIS_DBG_SOCK; see $LOG" >&2
      tail -5 "$LOG" >&2
      exit 1
    fi
    echo "SOCK=$FELIS_DBG_SOCK"
    echo "LOG=$LOG"
    ;;
  stop)
    sock="${2:?usage: isolated-daemon.sh stop <sock>}"
    felis_dbg_stop "$sock" "${FELIS_BIN:-./target/release/felis}"
    case "$(dirname "$sock")" in
    /tmp/felis-dbg.*) rm -rf "$(dirname "$sock")" ;;
    esac
    ;;
  *)
    echo "usage: isolated-daemon.sh start | stop <sock>" >&2
    exit 2
    ;;
  esac
fi
