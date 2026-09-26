#!/usr/bin/env bash
# Check an unpacked felis-x86_64-linux tree: what a host needs is there,
# nothing a host reads as text names a store path, and the bundle is
# closed. Takes the tree's root.
#
# The derivation asserts the same things before it tars; this is the
# check against the artifact a user would actually download.
set -euo pipefail

tree=${1:?usage: linux-archive-manifest.sh <unpacked tree>}

for path in bin/felis bin/felis-client bin/felis-daemon bin/.ld.so \
  libexec/felis libexec/felis-client libexec/felis-daemon \
  lib/libc.so.6 share/X11/locale/compose.dir \
  share/applications/felis.desktop \
  share/bash-completion/completions/felis.bash \
  share/zsh/site-functions/_felis \
  share/fish/vendor_completions.d/felis.fish \
  share/man/man1/felis.1.gz README.md LICENSE; do
  if [ ! -e "$tree/$path" ]; then
    echo "the archive is missing $path" >&2
    exit 1
  fi
done

# tic files the entry under a directory named for the first character
# of the terminal name, as the letter (x/) or its hex code (78/)
# depending on the ncurses build, so the entry is located, not assumed.
if [ -z "$(find "$tree/share/terminfo" -type f -name xterm-felis 2>/dev/null)" ]; then
  echo "the archive is missing the xterm-felis terminfo entry under share/terminfo" >&2
  exit 1
fi

# The ELF files keep store strings that are inert under --library-path
# (RUNPATH, and the loader's own compiled-in cache and locale paths), so
# what has to be clean is what is read as text: the launchers and the
# data trees.
if grep -l /nix/store "$tree"/bin/felis "$tree"/bin/felis-client "$tree"/bin/felis-daemon; then
  echo "a launcher keeps a store path" >&2
  exit 1
fi
if grep -rl /nix/store "$tree/share"; then
  echo "the files above keep a store path" >&2
  exit 1
fi

# A closed bundle: what the ELF files NEED is in lib/. The GPU and
# windowing libraries are dlopen'd, never NEEDED, and come from the host
# on purpose.
mapfile -d '' -t elves < <(find "$tree/lib" "$tree/libexec" -type f -print0)
for elf in "${elves[@]}"; do
  mapfile -t needed < <(readelf -d "$elf" 2>/dev/null | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p')
  for lib in ${needed[@]+"${needed[@]}"}; do
    if [ ! -f "$tree/lib/$lib" ]; then
      echo "$elf needs $lib, which is not in lib/" >&2
      exit 1
    fi
  done
done
