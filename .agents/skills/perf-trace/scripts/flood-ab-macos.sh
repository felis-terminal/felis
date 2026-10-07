#!/usr/bin/env bash
# Time a payload flood inside a fresh felis window, raw vs wrapped in
# DECSET 2026 (synchronized output). The delta isolates the
# render-live pipeline (frame pulls interleaving the parse loop) from
# the pure parse path: kitten __benchmark__ always sets 2026, termbench
# and vtebench never do, so this is the only harness that measures the
# SAME bytes both ways.
#
# Usage: flood-ab-macos.sh <payload-file> [reps]
set -u
. "$(dirname "${BASH_SOURCE[0]}")/../../isolated-daemon/scripts/isolated-daemon.sh"

FELIS_BIN="${FELIS_BIN:-./target/release/felis}"
PAYLOAD="${1:?usage: flood-ab-macos.sh <payload-file> [reps]}"
REPS="${2:-3}"
RESULTS="${RESULTS:-$(mktemp -d "${TMPDIR:-/tmp}/flood-ab.XXXXXX")}"
mkdir -p "$RESULTS"

[ -r "$PAYLOAD" ] || { echo "payload not readable: $PAYLOAD" >&2; exit 1; }
[ -x "$FELIS_BIN" ] || { echo "felis not built: $FELIS_BIN" >&2; exit 1; }

felis_dbg_socket felis-ab || exit 1
SOCKDIR="$FELIS_DBG_DIR"
SOCK="$FELIS_DBG_SOCK"
WRAP="$(mktemp)"

{
  echo '#!/usr/bin/env bash'
  echo 'sleep 1.5'
  echo "for i in \$(seq 1 $REPS); do"
  echo "  { /usr/bin/time -p cat '$PAYLOAD' ; } 2>> '$RESULTS/raw.time'"
  echo 'done'
  echo "printf '\\033[?2026h'"
  echo "for i in \$(seq 1 $REPS); do"
  echo "  { /usr/bin/time -p cat '$PAYLOAD' ; } 2>> '$RESULTS/sync.time'"
  echo 'done'
  echo "printf '\\033[?2026l'"
  echo "touch '$RESULTS/done'"
  echo 'sleep 1'
} > "$WRAP"
chmod +x "$WRAP"

cleanup() {
  [ -n "${FP:-}" ] && kill "$FP" 2>/dev/null
  felis_dbg_stop "$SOCK"
  rm -rf "$WRAP" "$SOCKDIR"
}
trap cleanup EXIT

echo "results dir: $RESULTS"
RUST_LOG=warn SHELL="$WRAP" "$FELIS_BIN" --socket "$SOCK" >/dev/null 2>&1 &
FP=$!
t=0
until [ -e "$RESULTS/done" ]; do
  sleep 1
  t=$((t + 1))
  if [ "$t" -ge 300 ]; then echo "TIMEOUT" >&2; exit 1; fi
done

# wc -c, not stat: PATH may hold GNU stat (where -f means filesystem
# status and silently returns garbage instead of failing over).
BYTES="$(wc -c < "$PAYLOAD" | tr -d ' ')"
report() { # $1 = label, $2 = file
  [ -e "$2" ] || return 0
  awk -v label="$1" -v bytes="$BYTES" \
    '/^real/ { s += $2; q += $2 * $2; n++ }
     END { m = s / n
           printf "  %-24s %6.2fs avg +-%4.2fs  (%.0f MB/s, n=%d)\n",
                  label, m, sqrt(q / n - m * m), bytes / m / 1048576, n }' "$2"
}
echo
report "raw (render live)" "$RESULTS/raw.time"
report "DECSET 2026 (no render)" "$RESULTS/sync.time"
