#!/usr/bin/env bash
# Assemble a relocatable macOS tree that runs on a host without Nix.
#
# Why the CLI moves inside the bundle: `felis` execs `felis-client` and
# both find `felis-daemon` by sibling lookup
# (crates/felis-client-core/src/spawn.rs), so all three have to share one
# directory, and on macOS that directory is `Contents/MacOS`.
# `bin/felis` is therefore a script that execs into the bundle rather
# than a binary of its own; a script, not a symlink, because whether
# `current_exe` resolves a symlink is a std detail while a script leaves
# the sibling directory unambiguous.
#
# Why the dylibs are copied and renamed rather than left alone: a Nix
# build records absolute store paths in LC_LOAD_DYLIB and LC_RPATH, and
# a host without Nix has nothing there. Every store dylib the binaries
# reach is copied into `Contents/Frameworks` and the reference rewritten
# to `@executable_path/../Frameworks/<name>` from an executable or
# `@loader_path/<name>` between dylibs, so the bundle resolves from
# wherever it was dragged.
#
# Why everything is signed again at the end: aarch64-darwin refuses to
# exec an unsigned Mach-O, nixpkgs ad-hoc signs its outputs, and
# `install_name_tool` invalidates that signature. Signing runs
# inside-out because a bundle seal covers the nested code it is computed
# over. `rcodesign` rather than `sigtool`: only the former writes a
# bundle's `_CodeSignature/CodeResources`.
#
# Usage:
#   dist-macos.sh [--out DIR] [--triple TRIPLE] [--bin-dir DIR]
#                 [--share DIR] [--version VER] [--readme PATH]
#                 [--license PATH] [--make-app PATH]
#
# nix/dist.nix passes every flag. Standalone on a `cargo build --release`
# tree, run it with no flags: the binaries come from target/release, the
# version from the workspace manifest, and `share/` is skipped unless
# --share names a prefix that has it. `otool`, `install_name_tool` and
# `rcodesign` come from the PATH there; the Nix build takes them from
# `cctools` and `rcodesign`, so neither path needs Xcode.
set -euo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/.." && pwd)

out=""
triple=""
bin_dir=""
share=""
version=""
readme=""
license=""
make_app=""

while [ $# -gt 0 ]; do
  case "$1" in
  --out) out="$2"; shift 2 ;;
  --triple) triple="$2"; shift 2 ;;
  --bin-dir) bin_dir="$2"; shift 2 ;;
  --share) share="$2"; shift 2 ;;
  --version) version="$2"; shift 2 ;;
  --readme) readme="$2"; shift 2 ;;
  --license) license="$2"; shift 2 ;;
  --make-app) make_app="$2"; shift 2 ;;
  -h | --help)
    sed -n '/^# Usage:/,/Xcode\./p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit 0
    ;;
  *)
    echo "dist-macos: unknown argument: $1" >&2
    exit 2
    ;;
  esac
done

die() {
  echo "dist-macos: $*" >&2
  exit 1
}

# Associative arrays and `mapfile -d` are bash 4 features, and macOS
# still ships 3.2 as /bin/bash. The Nix build runs nixpkgs' bash; a
# standalone run needs one from `nix develop`, brew or elsewhere.
if [ "${BASH_VERSINFO[0]}" -lt 4 ] ||
  { [ "${BASH_VERSINFO[0]}" -eq 4 ] && [ "${BASH_VERSINFO[1]}" -lt 4 ]; }; then
  die "bash 4.4 or newer is required; this is ${BASH_VERSION}"
fi

triple=${triple:-"$(uname -m)-darwin"}
bin_dir=${bin_dir:-"$repo_root/target/release"}
out=${out:-"$repo_root/target/release/felis-$triple"}
readme=${readme:-"$repo_root/README.md"}
license=${license:-"$repo_root/LICENSE"}
# Nix copies each script into the store on its own, so the bundle
# script is a sibling of this one only in the repository.
make_app=${make_app:-"$script_dir/make-macos-app.sh"}
[ -f "$make_app" ] || die "bundle script not found: $make_app"

cli="$bin_dir/felis"
client="$bin_dir/felis-client"
daemon="$bin_dir/felis-daemon"
for bin in "$cli" "$client" "$daemon"; do
  [ -f "$bin" ] || die "binary not found: $bin
  build first, e.g. \`cargo build --workspace --release\`"
done

rm -rf "$out"
mkdir -p "$out/bin" "$out/share"

app="$out/felis.app"
macos="$app/Contents/MacOS"
frameworks="$app/Contents/Frameworks"

version_arg=()
[ -n "$version" ] && version_arg=(--version "$version")
terminfo_arg=()
[ -n "$share" ] && [ -d "$share/terminfo" ] && terminfo_arg=(--terminfo "$share/terminfo")
bash "$make_app" \
  --client "$client" \
  --daemon "$daemon" \
  --cli "$cli" \
  --out "$out" \
  ${version_arg[@]+"${version_arg[@]}"} \
  ${terminfo_arg[@]+"${terminfo_arg[@]}"} >/dev/null
chmod -R u+w "$app"

is_macho() {
  local magic
  magic=$(od -An -tx1 -N4 "$1" | tr -d ' \n')
  case "$magic" in
  cffaedfe | cefaedfe | feedfacf | feedface | cafebabe | bebafeca) return 0 ;;
  *) return 1 ;;
  esac
}

# `otool -l` rather than `otool -L`: -L conflates a dylib's own id with
# its dependencies, and the rpath scrub needs the same parse anyway.
load_commands() {
  local file=$1 want=$2
  otool -l "$file" | awk -v want="$want" '
    $1 == "cmd" { cmd = $2; next }
    cmd == want && ($1 == "name" || $1 == "path") { print $2 }
  '
}

declare -A framework_source=()
pending=()

for exe in "$macos"/*; do
  is_macho "$exe" && pending+=("$exe")
done

while [ ${#pending[@]} -gt 0 ]; do
  file=${pending[0]}
  pending=(${pending[@]+"${pending[@]:1}"})

  case "$file" in
  "$frameworks"/*) prefix="@loader_path" ;;
  *) prefix="@executable_path/../Frameworks" ;;
  esac

  for cmd in LC_LOAD_DYLIB LC_LOAD_WEAK_DYLIB LC_REEXPORT_DYLIB; do
    while read -r dep; do
      [ -n "$dep" ] || continue
      case "$dep" in /nix/store/*) ;; *) continue ;; esac
      base=$(basename "$dep")
      copy="$frameworks/$base"
      if [ -n "${framework_source[$base]:-}" ]; then
        # One basename, two sources: unless they are the same bytes the
        # bundle would silently ship whichever was copied first.
        cmp -s "$dep" "${framework_source[$base]}" ||
          die "two different dylibs would be installed as Frameworks/$base:
  ${framework_source[$base]}
  $dep"
      else
        mkdir -p "$frameworks"
        cp -L "$dep" "$copy"
        chmod 0755 "$copy"
        framework_source[$base]=$dep
        pending+=("$copy")
      fi
      install_name_tool -change "$dep" "$prefix/$base" "$file"
    done < <(load_commands "$file" "$cmd")
  done
done

mapfile -d '' -t machos < <(find "$app" -type f -print0)
for file in "${machos[@]}"; do
  is_macho "$file" || continue
  case "$file" in
  "$frameworks"/*)
    install_name_tool -id "@loader_path/$(basename "$file")" "$file"
    ;;
  esac
  while read -r rpath; do
    [ -n "$rpath" ] || continue
    case "$rpath" in
    /nix/store/*) install_name_tool -delete_rpath "$rpath" "$file" ;;
    esac
  done < <(load_commands "$file" LC_RPATH)
done

# Inside-out: the bundle seal is computed over code that is already
# signed, so the executables and dylibs are signed before it.
if [ -d "$frameworks" ]; then
  for dylib in "$frameworks"/*; do
    rcodesign sign "$dylib" >/dev/null
  done
fi
for exe in "$macos"/*; do
  rcodesign sign "$exe" >/dev/null
done
rcodesign sign "$app" >/dev/null

# The symlink loop and the TERMINFO_DIRS export are the Linux
# launcher's, for the reasons nix/dist-linux.sh gives. The export names
# the bundle's copy rather than share/terminfo because the daemon adds
# that same directory to its sessions, and one path dedupes where two
# would not.
cat >"$out/bin/felis" <<'EOF'
#!/bin/sh
self=$0
while [ -L "$self" ]; do
  link=$(readlink "$self")
  case "$link" in
  /*) self=$link ;;
  *) self=$(dirname "$self")/$link ;;
  esac
done
# A resolved root, so the bundle's binaries see themselves under a
# path without `..`: felis spawns felis-client and felis-daemon by the
# directory of its own executable, and that path is what a process
# list shows.
root=$(cd "$(dirname "$self")/.." && pwd -P)
case ":${TERMINFO_DIRS-}:" in
*":$root/felis.app/Contents/Resources/terminfo:"*) ;;
*) export TERMINFO_DIRS="$root/felis.app/Contents/Resources/terminfo:${TERMINFO_DIRS-}" ;;
esac
exec "$root/felis.app/Contents/MacOS/felis" "$@"
EOF
chmod 0755 "$out/bin/felis"

if [ -n "$share" ]; then
  for tree in terminfo felis bash-completion zsh fish man; do
    [ -d "$share/$tree" ] || continue
    cp -RL "$share/$tree" "$out/share/$tree"
  done
  chmod -R u+w "$out/share"
fi

install -m 0644 "$readme" "$out/README.md"
install -m 0644 "$license" "$out/LICENSE"

# Every file a host reads as data or as text has to be free of store
# paths: Info.plist, the launcher, share/, README, LICENSE, whatever a
# later tree gains. `-a` because a plist or a terminfo entry is binary.
#
# Mach-O files are the one exclusion, and the load-command sweep at the
# end is what covers them instead: a Rust binary carries store strings
# no loader ever reads (vendored crate paths baked into panic
# locations), so a full-content scan of one can only be a false alarm.
mapfile -d '' -t files < <(find "$out" -type f -print0)
leaked=()
for file in "${files[@]}"; do
  is_macho "$file" && continue
  grep -qa /nix/store "$file" && leaked+=("$file")
done
if [ ${#leaked[@]} -gt 0 ]; then
  die "these keep a store path, and the tree must resolve off Nix:
  ${leaked[*]}"
fi

# Everything was copied with symlinks dereferenced, so any link here
# would be one this script did not put there, pointing who knows where.
# BSD readlink has no -f, so the check is the existence of a link.
mapfile -d '' -t links < <(find "$out" -type l -print0)
if [ ${#links[@]} -gt 0 ]; then
  die "the tree carries symlinks, which do not survive a move: ${links[*]}"
fi

# Nothing the loader reads may name a store path: a host without Nix has
# nothing there, and dyld would fail the exec rather than fall back.
for file in "${machos[@]}"; do
  is_macho "$file" || continue
  for cmd in LC_LOAD_DYLIB LC_LOAD_WEAK_DYLIB LC_REEXPORT_DYLIB LC_ID_DYLIB LC_RPATH; do
    while read -r path; do
      case "$path" in
      /nix/store/*) die "$file still names $path in $cmd" ;;
      esac
    done < <(load_commands "$file" "$cmd")
  done
done

echo "$out"
