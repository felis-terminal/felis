#!/usr/bin/env bash
# Check an unpacked felis-aarch64-darwin tree: what a host needs is
# there, nothing a host reads as text names a store path, and no load
# command sends dyld to /nix. Takes the tree's root.
#
# The derivation asserts the same things before it tars; this is the
# check against the artifact a user would actually download. `otool`
# comes from nixpkgs `cctools`, since the runner has no Xcode Command
# Line Tools.
set -euo pipefail

# `mapfile` below is bash 4, and macOS still ships 3.2 as /bin/bash, so
# a run outside the workflow needs a bash from Nix, brew or elsewhere.
if [ "${BASH_VERSINFO[0]}" -lt 4 ]; then
  echo "bash 4 or newer is required; this is ${BASH_VERSION}" >&2
  exit 1
fi

tree=${1:?usage: macos-archive-manifest.sh <unpacked tree>}
app="$tree/felis.app"

for path in felis.app/Contents/MacOS/felis \
  felis.app/Contents/MacOS/felis-client \
  felis.app/Contents/MacOS/felis-daemon \
  felis.app/Contents/Info.plist bin/felis \
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
for dir in share/terminfo felis.app/Contents/Resources/terminfo; do
  if [ -z "$(find "$tree/$dir" -type f -name xterm-felis 2>/dev/null)" ]; then
    echo "the archive is missing the xterm-felis terminfo entry under $dir" >&2
    exit 1
  fi
done

# The bundle launches felis-client, not the CLI that sits beside it.
if ! grep -q '<string>felis-client</string>' "$app/Contents/Info.plist"; then
  echo "CFBundleExecutable does not name felis-client" >&2
  exit 1
fi

is_macho() {
  local magic
  magic=$(od -An -tx1 -N4 "$1" | tr -d ' \n')
  case "$magic" in
  cffaedfe | cefaedfe | feedfacf | feedface | cafebabe | bebafeca) return 0 ;;
  *) return 1 ;;
  esac
}

# Every file a host reads as data or as text has to be free of store
# paths: Info.plist, the launcher, share/, README, LICENSE. `-a`
# because a plist or a terminfo entry is binary.
#
# Mach-O files are the one exclusion, and the load-command sweep below
# is what covers them instead: a Rust binary carries store strings no
# loader ever reads (vendored crate paths baked into panic locations),
# so a full-content scan of one can only be a false alarm.
mapfile -d '' -t files < <(find "$tree" -type f -print0)
leaked=()
for file in "${files[@]}"; do
  is_macho "$file" && continue
  grep -qa /nix/store "$file" && leaked+=("$file")
done
if [ ${#leaked[@]} -gt 0 ]; then
  echo "these keep a store path, and the archive must resolve off Nix:" >&2
  printf '  %s\n' "${leaked[@]}" >&2
  exit 1
fi

# A host without Nix has nothing under /nix/store, and dyld fails the
# exec rather than falling back, so a surviving reference is fatal at
# first run. Mach-O keeps three kinds: what a file loads, what it calls
# itself, and where it searches.
found=""
for file in "$app"/Contents/MacOS/* "$app"/Contents/Frameworks/*; do
  [ -f "$file" ] || continue
  hits=$(otool -l "$file" | awk '
    $1 == "cmd" { cmd = $2; next }
    cmd ~ /^LC_(LOAD_DYLIB|LOAD_WEAK_DYLIB|REEXPORT_DYLIB|ID_DYLIB|RPATH)$/ &&
      ($1 == "name" || $1 == "path") && $2 ~ /^\/nix\/store\// { print cmd, $2 }
  ')
  if [ -n "$hits" ]; then
    echo "$file still names a store path:" >&2
    echo "$hits" >&2
    found=yes
  fi
done
[ -z "$found" ] || exit 1
