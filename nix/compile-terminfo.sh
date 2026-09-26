#!/usr/bin/env bash
# Compile share/terminfo/felis.terminfo, then assert the two properties
# the entry's macOS readers need. Apple ships ncurses 6.0, which predates
# the 32-bit terminfo format (magic 0o1036) and refuses any entry over
# 4096 bytes — either violation makes /usr/bin/tput, vim and less inside
# felis report "unknown terminal" while Nix-built programs work fine, so
# a broken entry would ship unnoticed. The source file carries the caps
# that keep it under both limits.
#
# Usage: compile-terminfo.sh <felis.terminfo> <output-share-terminfo-dir>
set -euo pipefail

src=$1
out=$2

mkdir -p "$out"
tic -x -o "$out" "$src"

# tic files the entry under a subdirectory named for the terminal name's
# first character, but WHICH spelling that takes depends on the build —
# GNU ncurses 6 writes the literal letter (x/), Apple's bundled tic the
# hex code (78/) — so locate the entry instead of assuming one layout.
entry=$(find "$out" -type f -name xterm-felis | head -n1)
magic=$(od -An -tx1 -N2 "$entry" | tr -d ' ')
if [ "$magic" != "1a01" ]; then
	echo "terminfo: $entry is not the 16-bit format (magic $magic)" >&2
	exit 1
fi

size=$(wc -c <"$entry")
if [ "$size" -ge 4096 ]; then
	echo "terminfo: $entry is $size bytes, over the 4096-byte reader limit" >&2
	exit 1
fi
