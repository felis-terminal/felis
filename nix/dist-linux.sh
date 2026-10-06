#!/usr/bin/env bash
# Assemble a relocatable Linux tree that runs on a host without Nix.
#
# Why a bundled loader rather than patchelf: PT_INTERP is an absolute
# path and an unpacked tarball lives wherever the user put it. Each
# `bin/<name>` launcher execs the bundled `ld.so` with `--library-path`,
# so the bundled libraries win at any prefix, and `--argv0` keeps clap's
# usage line and the daemon's process name honest.
#
# Why the loader sits in `bin/`: under a direct loader exec
# `/proc/self/exe` is the loader, so `current_exe().parent()` is the
# loader's directory. Keeping it in `bin/` leaves the launchers as the
# siblings the daemon lookup already walks to
# (crates/felis-client-core/src/spawn.rs, crates/felis-daemon/src/relay.rs).
#
# Why host directories come after `lib/`: the GPU stack (libvulkan.so.1
# and the ICDs it dlopens) is host state, and a bundled copy would load
# the wrong driver.
#
# Why the launcher exports XLOCALEDIR and TERMINFO_DIRS and nothing
# else: the client snapshots its whole environment for every session it
# creates, and the daemon hands that snapshot to PTY children, so a
# launcher export reaches the user's shells. LOCPATH and GCONV_PATH
# would make a host glibc read bundled data and crash; XLOCALEDIR names
# the upstream X11 locale tree, which is what a host program would have
# read anyway, and TERMINFO_DIRS is meant for those shells, whose
# TERM=xterm-felis the host's database lacks. Its trailing empty element
# keeps the host's default directories searched.
#
# Why the launcher resolves its own symlinks: users link bin/felis into
# a directory on PATH, and the tree is found from the link's target. A
# loop over plain `readlink` rather than `readlink -f`, which older macOS
# lacks, so the two platforms' launchers share one form.
#
# Usage:
#   dist-linux.sh [--out DIR] [--triple TRIPLE] [--bin-dir DIR]
#                 [--share DIR] [--loader PATH] [--lib-from PATH]...
#                 [--x11-locale DIR] [--readme PATH] [--license PATH]
#
# --lib-from takes a store path (whose `lib/*.so*` is copied), a plain
# directory of shared objects, or a single file. nix/dist.nix passes
# every flag: it classifies the package's closure into what is bundled
# and what the host supplies, and a path in neither list fails the build
# there rather than here.
#
# Standalone on a `cargo build --release` tree, run it with no flags
# from anywhere: the binaries come from target/release, the loader from
# the CLI's own PT_INTERP, the bundled libraries from what `ldd`
# resolves for the three binaries, the compose tables from
# /usr/share/X11/locale, and the package's share/ trees are skipped
# unless --share names a prefix that has them. The dlopen'd windowing
# and GPU libraries are not bundled that way, so a standalone tree
# leans on the build host for them.
set -euo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/.." && pwd)

out=""
triple=""
bin_dir=""
share=""
loader=""
x11_locale=""
readme=""
license=""
lib_from=()

while [ $# -gt 0 ]; do
  case "$1" in
  --out) out="$2"; shift 2 ;;
  --triple) triple="$2"; shift 2 ;;
  --bin-dir) bin_dir="$2"; shift 2 ;;
  --share) share="$2"; shift 2 ;;
  --loader) loader="$2"; shift 2 ;;
  --lib-from) lib_from+=("$2"); shift 2 ;;
  --x11-locale) x11_locale="$2"; shift 2 ;;
  --readme) readme="$2"; shift 2 ;;
  --license) license="$2"; shift 2 ;;
  -h | --help)
    sed -n '/^# Usage:/,/them\./p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit 0
    ;;
  *)
    echo "dist-linux: unknown argument: $1" >&2
    exit 2
    ;;
  esac
done

die() {
  echo "dist-linux: $*" >&2
  exit 1
}

triple=${triple:-"$(uname -m)-linux"}
bin_dir=${bin_dir:-"$repo_root/target/release"}
out=${out:-"$repo_root/target/release/felis-$triple"}
readme=${readme:-"$repo_root/README.md"}
license=${license:-"$repo_root/LICENSE"}
x11_locale=${x11_locale:-/usr/share/X11/locale}

cli="$bin_dir/felis"
daemon="$bin_dir/felis-daemon"
# The Nix package wraps the client in a shell script and keeps the ELF
# beside it; a cargo tree has the ELF under the plain name.
client="$bin_dir/.felis-client-wrapped"
[ -f "$client" ] || client="$bin_dir/felis-client"

for bin in "$cli" "$client" "$daemon"; do
  [ -f "$bin" ] || die "binary not found: $bin
  build first, e.g. \`cargo build --workspace --release\`"
done

if [ -z "$loader" ]; then
  loader=$(readelf -l "$cli" |
    sed -n 's/.*Requesting program interpreter: \(.*\)\]/\1/p')
  [ -n "$loader" ] || die "$cli has no PT_INTERP; pass --loader"
fi

if [ ${#lib_from[@]} -eq 0 ]; then
  mapfile -t lib_from < <(ldd "$cli" "$client" "$daemon" |
    awk '$3 ~ /^\// { print $3 }' | sort -u)
fi

rm -rf "$out"
mkdir -p "$out/bin" "$out/libexec" "$out/lib" "$out/share"

install -m 0755 "$cli" "$out/libexec/felis"
install -m 0755 "$client" "$out/libexec/felis-client"
install -m 0755 "$daemon" "$out/libexec/felis-daemon"
install -m 0755 -T "$loader" "$out/bin/.ld.so"

arch=${triple%%-*}
launcher_template=$(
  cat <<'EOF'
#!/bin/sh
self=$0
while [ -L "$self" ]; do
  link=$(readlink "$self")
  case "$link" in
  /*) self=$link ;;
  *) self=$(dirname "$self")/$link ;;
  esac
done
here=$(cd "$(dirname "$self")" && pwd -P)
root=$(dirname "$here")
export XLOCALEDIR="${XLOCALEDIR:-$root/share/X11/locale}"
case ":${TERMINFO_DIRS-}:" in
*":$root/share/terminfo:"*) ;;
*) export TERMINFO_DIRS="$root/share/terminfo:${TERMINFO_DIRS-}" ;;
esac
exec "$here/.ld.so" --argv0 "$0" \
  --library-path "$root/lib:${LD_LIBRARY_PATH:+$LD_LIBRARY_PATH:}/usr/lib/@ARCH@-linux-gnu:/lib/@ARCH@-linux-gnu:/usr/lib64:/lib64:/usr/lib:/lib" \
  "$root/libexec/@NAME@" "$@"
EOF
)

for name in felis felis-client felis-daemon; do
  printf '%s\n' "$launcher_template" |
    sed -e "s|@ARCH@|$arch|g" -e "s|@NAME@|$name|g" >"$out/bin/$name"
  chmod 0755 "$out/bin/$name"
done

# Dereference on copy so the tree carries no link out of itself, and
# refuse two different files under one name rather than letting the
# later source win silently.
install_lib() {
  local src=$1 base dest
  base=$(basename "$src")
  dest="$out/lib/$base"
  if [ -e "$dest" ]; then
    cmp -s "$src" "$dest" ||
      die "two different files would be installed as lib/$base; last was $src"
    return
  fi
  cp -L "$src" "$dest"
  chmod 0755 "$dest"
}

for src in "${lib_from[@]}"; do
  if [ -f "$src" ]; then
    install_lib "$src"
    continue
  fi
  dir="$src"
  [ -d "$dir/lib" ] && dir="$dir/lib"
  [ -d "$dir" ] || die "library source not found: $src"
  mapfile -d '' -t files < <(
    find "$dir" -maxdepth 1 \( -type f -o -type l \) -name '*.so*' -print0
  )
  for file in "${files[@]}"; do
    install_lib "$file"
  done
done

[ -d "$x11_locale" ] || die "X11 locale tree not found: $x11_locale"
mkdir -p "$out/share/X11"
cp -RL "$x11_locale" "$out/share/X11/locale"
chmod -R u+w "$out/share/X11/locale"
# %S is libX11's own token for the locale directory, expanded by
# libxkbcommon too, so a rewritten `include` resolves under XLOCALEDIR
# wherever the tree is unpacked.
mapfile -t absolute < <(grep -rl '"/nix/store/' "$out/share/X11/locale" || true)
if [ ${#absolute[@]} -gt 0 ]; then
  sed -i 's|"/nix/store/[^"]*/share/X11/locale/|"%S/|g' "${absolute[@]}"
fi

if [ -n "$share" ]; then
  for tree in terminfo felis applications bash-completion zsh fish man; do
    [ -d "$share/$tree" ] || continue
    cp -RL "$share/$tree" "$out/share/$tree"
  done
  chmod -R u+w "$out/share"
fi

install -m 0644 "$readme" "$out/README.md"
install -m 0644 "$license" "$out/LICENSE"

if grep -rl /nix/store "$out/share" >&2; then
  die "the files above keep a store path; share/ must resolve off Nix"
fi
for name in felis felis-client felis-daemon; do
  if grep -l /nix/store "$out/bin/$name" >&2; then
    die "bin/$name keeps a store path"
  fi
done

mapfile -d '' -t links < <(find "$out" -type l -print0)
for link in ${links[@]+"${links[@]}"}; do
  target=$(readlink -f "$link" || true)
  case "$target" in
  "$out"/*) ;;
  *) die "$link points outside the tree ($target)" ;;
  esac
done

# The bundle is closed when every NEEDED library of every bundled ELF is
# itself in lib/. The GPU and windowing libraries the client dlopens are
# not NEEDED entries, so they are absent from this closure by design.
mapfile -d '' -t elves < <(find "$out/lib" "$out/libexec" -type f -print0)
for elf in "${elves[@]}"; do
  [ "$(head -c 4 "$elf" | od -An -c | tr -d ' ')" = '177ELF' ] || continue
  mapfile -t needed < <(
    readelf -d "$elf" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p'
  )
  for lib in ${needed[@]+"${needed[@]}"}; do
    [ -f "$out/lib/$lib" ] ||
      die "$elf needs $lib, which is not in lib/"
  done
done

echo "$out"
