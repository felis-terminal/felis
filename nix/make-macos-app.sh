#!/usr/bin/env bash
# Assemble a macOS `felis.app` bundle from two already-built binaries.
#
# Why a bundle at all: felis-client is a winit/wgpu GUI. macOS only
# grants a bare Mach-O the Dock icon, foreground activation, HiDPI
# backing store and window focus a terminal needs once it is launched
# *as an .app*; a loose binary run from Finder misbehaves. The flake
# owns the build (flake.nix is the single source of truth for the
# toolchain) — this script is the one bundle-layout definition that both
# `nix build` (nix/package.nix) and a plain `cargo build --release`
# drive, so the two paths can never disagree.
#
# Why both binaries land in Contents/MacOS side by side: the client
# auto-spawns its daemon via `current_exe().parent().join("felis-daemon")`
# (crates/felis-client-core/src/spawn.rs::spawn_daemon_child) so it runs
# the *matching* build instead of whatever a stray PATH offers. The
# bundle must preserve that adjacency or the client falls back to PATH.
#
# Why --cli renames the client: the CLI execs felis-client and both look
# for felis-daemon beside themselves, so a tree that ships all three
# needs one directory holding all three, and on macOS that is
# Contents/MacOS. The client cannot stay `felis` there, so with --cli it
# installs as `felis-client` and CFBundleExecutable follows it.
# CFBundleName and CFBundleDisplayName do not, so Finder and the Dock
# are unchanged.
#
# Why copy, not symlink: an .app is meant to be relocatable (dragged to
# /Applications, handed to another machine). Symlinks into a build dir
# or the Nix store would dangle the moment the bundle moves.
#
# Usage:
#   make-macos-app.sh [--client PATH] [--daemon PATH] [--cli PATH]
#                             [--version VER] [--out DIR]
#                             [--name NAME] [--identifier ID]
#
# Defaults target a standalone `cargo build --release`: it reads the
# binaries from target/release and the version from the client crate's
# Cargo.toml, emitting target/release/felis.app. nix/package.nix passes
# every flag explicitly.
set -euo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/.." && pwd)

name="felis"
identifier="com.natsukium.felis"
client=""
daemon=""
cli=""
version=""
out=""

while [ $# -gt 0 ]; do
  case "$1" in
  --client) client="$2"; shift 2 ;;
  --daemon) daemon="$2"; shift 2 ;;
  --cli) cli="$2"; shift 2 ;;
  --version) version="$2"; shift 2 ;;
  --out) out="$2"; shift 2 ;;
  --name) name="$2"; shift 2 ;;
  --identifier) identifier="$2"; shift 2 ;;
  -h | --help)
    # Echo the usage block above, trimming the leading "# ".
    sed -n '/^# Usage:/,/explicitly\./p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit 0
    ;;
  *)
    echo "make-macos-app: unknown argument: $1" >&2
    exit 2
    ;;
  esac
done

# Standalone defaults: a release build in the workspace target dir. The
# GUI binary is `felis-client` (the bare `felis` command is the headless
# front-door, felis-cli — not what the .app bundles).
client=${client:-"$repo_root/target/release/felis-client"}
daemon=${daemon:-"$repo_root/target/release/felis-daemon"}
out=${out:-"$repo_root/target/release"}
if [ -z "$version" ]; then
  # `[workspace.package] version`, the one number every crate inherits.
  # Kept in lockstep with nix/package.nix, which reads the same field.
  version=$(sed -nE '/^\[workspace\.package\]/,/^\[/ s/^version = "([^"]+)".*/\1/p' \
    "$repo_root/Cargo.toml" | head -n1)
fi

executable="$name"
if [ -n "$cli" ]; then
  executable="felis-client"
fi

for bin in "$client" "$daemon" ${cli:+"$cli"}; do
  if [ ! -f "$bin" ]; then
    echo "make-macos-app: binary not found: $bin" >&2
    echo "  build first, e.g. \`cargo build --workspace --release\`" >&2
    exit 1
  fi
done

app="$out/$name.app"
macos="$app/Contents/MacOS"
resources="$app/Contents/Resources"

# Start clean so a rebuild never leaves a stale binary behind.
rm -rf "$app"
mkdir -p "$macos" "$resources"

install -m 0755 "$client" "$macos/$executable"
install -m 0755 "$daemon" "$macos/felis-daemon"
if [ -n "$cli" ]; then
  install -m 0755 "$cli" "$macos/felis"
fi

# NSHighResolutionCapable is mandatory: without it AppKit hands winit a
# 1x backing store on Retina and every glyph renders blurry. The two
# version keys differ in role — CFBundleShortVersionString is the
# human-facing marketing version, CFBundleVersion the build counter —
# but a single source has nothing to distinguish, so both carry the
# crate version.
cat >"$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>
  <string>en</string>
  <key>CFBundleExecutable</key>
  <string>$executable</string>
  <key>CFBundleIdentifier</key>
  <string>$identifier</string>
  <key>CFBundleName</key>
  <string>$name</string>
  <key>CFBundleDisplayName</key>
  <string>$name</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleShortVersionString</key>
  <string>$version</string>
  <key>CFBundleVersion</key>
  <string>$version</string>
  <key>NSHighResolutionCapable</key>
  <true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key>
  <true/>
</dict>
</plist>
EOF

echo "$app"
