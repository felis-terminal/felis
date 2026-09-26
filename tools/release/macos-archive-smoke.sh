#!/bin/sh
# Run the unpacked felis-aarch64-darwin tree on the macOS runner. Takes
# the tree's root and a directory to put the run's socket under.
#
# The runner has no Aqua session and cannot open a window, so the client
# is exercised by running it directly with --version: `felis version`
# reports a client it could not launch as `unavailable` and still exits
# 0, so only the direct run catches a dylib dyld cannot resolve. That
# the client is also the tag's own build is checked by the release
# workflow, which reads `felis version --format json` next to its
# identity gate. The window itself rests on the maintainer's use of the
# build (docs/explanation/testing.md "Maintainer use, for
# `aarch64-darwin`").
#
# Not run through `nix develop`: the dev shell exports
# DYLD_FALLBACK_LIBRARY_PATH, which could resolve a dylib the bundle
# failed to carry and turn the one check that proves the relocation into
# a pass.
set -eu

tree=${1:?usage: macos-archive-smoke.sh <unpacked tree> [socket parent]}
# Resolved, because the daemon's command line is compared against it
# and the launcher resolves its own root the same way.
tree=$(cd "$tree" && pwd -P)
app="$tree/felis.app"

# A fresh directory per run, removed at exit: the runner is persistent
# and the workspace path is stable, so a daemon left behind by a failed
# run would be adopted by the next one and pass the check below as if it
# were the daemon this run spawned.
# A short name: a unix socket path is capped near 104 bytes on macOS,
# and the daemon's own `run/` sits below this.
run_dir=$(mktemp -d "${2:-${TMPDIR:-/tmp}}/felis-arc.XXXXXX")
# The daemon creates the socket's parent itself, at the 0700 it then
# insists on (crates/felis-transport/src/unix.rs); a directory this
# script created would carry the inherited umask and be refused.
sock="$run_dir/run/daemon.sock"

cleanup() {
  "$tree/bin/felis" --socket "$sock" daemon stop --force >/dev/null 2>&1 || true
  rm -rf "$run_dir"
}
trap cleanup EXIT

echo "== the bundled client resolves its libraries and runs"
"$app/Contents/MacOS/felis-client" --version

echo "== the launcher reaches the CLI inside the bundle"
"$tree/bin/felis" --version

echo "== the archive spawns its own daemon"
"$tree/bin/felis" --socket "$sock" sessions spawn -- true
# No /proc on macOS: the daemon is found by the command line it was
# spawned with, which carries the absolute path the client chose.
# `|| true`: no match must reach the case below, not kill the script
# under set -e with nothing said about why.
command_line=$(ps -ax -o command= | grep -F -- "serve --socket $sock" | grep -v grep | head -n1 || true)
case "$command_line" in
"$app/Contents/MacOS/felis-daemon"*) ;;
*)
  echo "the daemon runs ${command_line:-nothing}, not the extracted bundle's" >&2
  exit 1
  ;;
esac

echo "== a launcher reached through links finds its tree, and a session resolves its TERM"
mkdir -p "$run_dir/links"
ln -s "$tree/bin/felis" "$run_dir/links/felis-abs"
ln -s felis-abs "$run_dir/links/felis"
"$run_dir/links/felis" --version
# /usr/bin/tput: the system ncurses is the reader whose format limits
# the entry is compiled for.
"$run_dir/links/felis" --socket "$sock" sessions spawn -- sh -c \
  '/usr/bin/tput colors >"$1.tmp" 2>&1; echo "status=$?" >>"$1.tmp"; mv "$1.tmp" "$1"' \
  sh "$run_dir/colors.out"
i=0
while [ ! -s "$run_dir/colors.out" ] && [ "$i" -lt 100 ]; do
  i=$((i + 1))
  sleep 0.2
done
if [ "$(cat "$run_dir/colors.out" 2>/dev/null)" != "$(printf '256\nstatus=0')" ]; then
  echo "tput colors under TERM=xterm-felis said: $(cat "$run_dir/colors.out" 2>/dev/null)" >&2
  exit 1
fi

# --force: a plain stop refuses while an exited session sits in its reap grace.
"$tree/bin/felis" --socket "$sock" daemon stop --force
