#!/bin/sh
# Prove the relocated Linux archive on a foreign host.
#
# Runs inside `ubuntu:24.04` with the extracted tree read-only at
# /opt/felis and the smoke's own helpers at /opt/smoke, so anything the
# archive still resolved from a store path would fail here. The
# .forgejo/workflows/release.yml `linux-package` job is one caller; by
# hand it is
#
#   docker run --rm -v <tree>:/opt/felis:ro -v <ctx>:/opt/smoke:ro \
#     -e FELIS_SMOKE_MARKER=felis-smoke -e FELIS_SMOKE_TIMEOUT_MS=60000 \
#     ubuntu:24.04 sh /opt/smoke/smoke.sh
#
# where <ctx> holds this script as smoke.sh and the
# libexec/xkbcommon/xkbcli-compile-compose helper out of the package's
# own libxkbcommon.
set -eu

export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq --no-install-recommends \
  locales xvfb xauth fonts-dejavu-core fontconfig-config libvulkan1 \
  mesa-vulkan-drivers libwayland-client0 libxkbcommon-x11-0
locale-gen en_US.UTF-8 ja_JP.UTF-8

felis=/opt/felis/bin/felis

# The daemon is reparented and detached, so it is found by its command
# line rather than by a job id. /proc alone, to keep the image bare.
daemon_pid() {
  for proc in /proc/[0-9]*; do
    if tr '\0' ' ' <"$proc/cmdline" 2>/dev/null | grep -q -- "serve --socket $1"; then
      echo "${proc#/proc/}"
      return 0
    fi
  done
  return 1
}

await() {
  i=0
  while [ "$i" -lt 100 ]; do
    if "$@" >/dev/null 2>&1; then
      return 0
    fi
    i=$((i + 1))
    sleep 0.2
  done
  echo "smoke: timed out waiting for: $*" >&2
  return 1
}

# The daemon runs under the bundled loader, and the loader is what execs,
# so its own executable is .ld.so and the daemon it runs names libexec.
assert_bundled_daemon() {
  pid=$(daemon_pid "$1")
  exe=$(readlink "/proc/$pid/exe")
  [ "$exe" = /opt/felis/bin/.ld.so ] ||
    { echo "smoke: the daemon runs $exe, not the bundled loader" >&2; exit 1; }
  tr '\0' ' ' <"/proc/$pid/cmdline" | grep -q /opt/felis/libexec/felis-daemon ||
    { echo "smoke: the daemon's command line does not name the archive" >&2; exit 1; }
}

# A PTY child inherits the client's whole environment snapshot, so a
# launcher export reaches the user's shell. Only XLOCALEDIR and
# TERMINFO_DIRS may, and the latter once: the CLI's launcher execs the
# client's, which must not prepend the tree a second time.
assert_child_env() {
  for name in LOCPATH GCONV_PATH LD_LIBRARY_PATH XKB_CONFIG_ROOT; do
    if grep -q "^$name=" "$1"; then
      echo "smoke: a PTY child inherited $name" >&2
      exit 1
    fi
  done
  grep -qx "XLOCALEDIR=/opt/felis/share/X11/locale" "$1" ||
    { echo "smoke: a PTY child did not inherit the bundled XLOCALEDIR" >&2; exit 1; }
  grep -qx "TERMINFO_DIRS=/opt/felis/share/terminfo:" "$1" ||
    { echo "smoke: a PTY child's TERMINFO_DIRS is not the bundled tree plus the default" >&2; exit 1; }
}

echo "== a: the CLI runs"
"$felis" --version

echo "== b: the archive spawns its own daemon"
sock=/tmp/felis-b/daemon.sock
"$felis" --socket "$sock" sessions spawn -- true
assert_bundled_daemon "$sock"
"$felis" --socket "$sock" sessions spawn -- sh -c 'env > /tmp/child.env'
await test -s /tmp/child.env
assert_child_env /tmp/child.env
# --force: a plain stop refuses while an exited session sits in its reap grace.
"$felis" --socket "$sock" daemon stop --force

echo "== b1: the relay starts a cold daemon that outlives it"
export PATH="/opt/felis/bin:$PATH"
relay_sock=/tmp/felis-b1/daemon.sock
felis-daemon relay --socket "$relay_sock" </dev/null
await "$felis" --socket "$relay_sock" sessions list
assert_bundled_daemon "$relay_sock"
"$felis" --socket "$relay_sock" daemon stop --force

echo "== b2: the bundled glibc resolves a locale from the host archive"
LANG=en_US.UTF-8 /opt/felis/bin/.ld.so --library-path /opt/felis/lib \
  /usr/bin/locale charmap >/tmp/charmap.out 2>/tmp/charmap.err
[ "$(cat /tmp/charmap.out)" = "UTF-8" ] ||
  { echo "smoke: locale charmap said $(cat /tmp/charmap.out)" >&2; exit 1; }
[ ! -s /tmp/charmap.err ] ||
  { echo "smoke: locale warned: $(cat /tmp/charmap.err)" >&2; exit 1; }

echo "== b3: compose tables resolve off Nix"
XLOCALEDIR=/opt/felis/share/X11/locale LANG=ja_JP.UTF-8 \
  /opt/felis/bin/.ld.so --library-path /opt/felis/lib \
  /opt/smoke/xkbcli-compile-compose --locale ja_JP.UTF-8 >/tmp/compose.out
[ -s /tmp/compose.out ] ||
  { echo "smoke: the ja_JP.UTF-8 compose table came out empty" >&2; exit 1; }

echo "== b4: a launcher reached through links finds its tree, and a session resolves its TERM"
mkdir -p /tmp/links
ln -s /opt/felis/bin/felis /tmp/links/felis-abs
ln -s felis-abs /tmp/links/felis
link_sock=/tmp/felis-b4/daemon.sock
/tmp/links/felis --version
/tmp/links/felis --socket "$link_sock" sessions spawn -- \
  sh -c 'tput colors >/tmp/colors.tmp 2>&1; echo "status=$?" >>/tmp/colors.tmp; mv /tmp/colors.tmp /tmp/colors.out'
await test -s /tmp/colors.out
[ "$(cat /tmp/colors.out)" = "$(printf '256\nstatus=0')" ] ||
  { echo "smoke: tput colors under TERM=xterm-felis said: $(cat /tmp/colors.out)" >&2; exit 1; }
assert_bundled_daemon "$link_sock"
# Exit status ignored: the container's other checks (no GPU, no
# clipboard) are not this row's business.
/tmp/links/felis --socket "$link_sock" doctor >/tmp/doctor.out || true
grep -q '^ok  *terminfo ' /tmp/doctor.out ||
  { echo "smoke: felis doctor did not find the bundled terminfo:" >&2; cat /tmp/doctor.out >&2; exit 1; }
/tmp/links/felis --socket "$link_sock" daemon stop --force

echo "== c: the client opens a window and renders through the host's Vulkan"
gui_sock=/tmp/felis-c/daemon.sock
LANG=en_US.UTF-8 FELIS_SMOKE_MARKER="$FELIS_SMOKE_MARKER" \
  FELIS_SMOKE_TIMEOUT_MS="$FELIS_SMOKE_TIMEOUT_MS" \
  xvfb-run -a /opt/felis/bin/felis-client --socket "$gui_sock" \
  -- sh -c 'env > /tmp/gui-child.env; cat'
assert_child_env /tmp/gui-child.env
"$felis" --socket "$gui_sock" daemon stop --force
