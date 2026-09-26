#!/usr/bin/env bash
# Turn a samply profile into a flat, symbolicated self-time table for
# felis's own code. samply leaves frames as raw `0x...` addresses (it
# symbolicates client-side in the browser); we resolve them offline with
# addr2line against the non-stripped release binary.
#
# Usage:
#   symbolize.sh <profile.json.gz> [thread-name-substr] [daemon-binary]
# Defaults:
#   thread-name-substr = tokio-rt-worker   (the daemon's fan-out threads)
#   daemon-binary      = ./target/release/felis-daemon
#
# Tips:
#   * The parse (parser + grid mutation) runs on the per-session
#     `felis-pty-parser` thread, NOT the tokio workers; the
#     `felis-pty-reader` thread only does read(2) and buffer swaps. For a
#     parse breakdown, pass a parser-thread substring; the
#     tokio-rt-worker default profiles the effect-replay / diff-compose /
#     fan-out side instead.
#   * The hottest *leaf* is often libc; walk up to the first felis frame
#     before trusting a libc number. nm's nearest-exported-symbol name
#     for a libc offset (e.g. "sem_trywait") is unreliable; prefer the
#     caller.
set -u

PROF="${1:?usage: symbolize.sh <profile.json.gz> [thread-substr] [binary]}"
THREAD="${2:-tokio-rt-worker}"
BIN="${3:-./target/release/felis-daemon}"

JSON="$(mktemp)"
gunzip -c "$PROF" > "$JSON"

echo "=== threads with samples ==="
jq -r '.threads[] | select(.samples.length>0)
       | "\(.samples.length)\t\(.name)\tpid=\(.pid)"' "$JSON" \
  | sort -rn | head -12

echo
echo "=== top self functions on '$THREAD' (felis-daemon code only) ==="
# leaf frame -> func -> resource -> lib name; keep felis-daemon frames,
# tally by address, resolve with addr2line.
jq -r --arg th "$THREAD" '
  [.libs[].name] as $libs
  | .threads[] | select(.name | contains($th)) | . as $t
  | $t.samples.stack[] | select(. != null)
  | $t.frameTable.func[$t.stackTable.frame[.]] as $fn
  | select($libs[$t.resourceTable.lib[$t.funcTable.resource[$fn]]] == "felis-daemon")
  | $t.stringArray[$t.funcTable.name[$fn]]
' "$JSON" \
  | sort | uniq -c | sort -rn | head -25 \
  | while read -r n addr; do
      sym="$(addr2line -f -e "$BIN" -C "$addr" 2>/dev/null | head -1)"
      printf '%7s  %-10s  %s\n' "$n" "$addr" "${sym:-?}"
    done

rm -f "$JSON"
echo
echo "(merge duplicate symbol rows by hand: one function spans several"
echo " addresses. To disambiguate a libc leaf, re-run jq selecting that"
echo " address and walk .stackTable.prefix up to the first felis frame.)"
