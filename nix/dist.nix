# The archive a host without Nix unpacks and runs: the package's own
# binaries, the libraries they need, and the data trees that would
# otherwise resolve to a store path. Linux relocates a closure around a
# bundled loader and macOS rewrites Mach-O load commands and signs
# again, so each host gets its own assembler and its own branch below.
#
# On Linux the closure is classified here rather than in the assembler,
# because only Nix knows what the package pulled in. A path in neither
# list fails the build, so a new runtime dependency is a red build
# instead of a library that silently ships or silently goes missing.
{
  lib,
  stdenv,
  runCommand,
  closureInfo,
  binutils,
  gnutar,
  gzip,
  libx11,
  cctools,
  rcodesign,
  felis,
}:

let
  triple = stdenv.hostPlatform.system;
  closure = closureInfo { rootPaths = [ felis ]; };

  # Reproducible: a local `nix build .#felis-dist` yields the bytes the
  # release attaches, so the archive can be checked against it.
  archive = ''
    mkdir -p $out
    tar --sort=name --mtime=@1 --owner=0 --group=0 --numeric-owner \
      -C "$stage" -cf - felis-${triple} |
      gzip -n > $out/felis-${triple}.tar.gz
  '';

  meta = {
    description = "Relocatable ${triple} archive of felis for hosts without Nix";
    inherit (felis.meta) license platforms;
  };

  darwin =
    runCommand "felis-dist-${triple}-${felis.version}"
      {
        inherit meta;
        nativeBuildInputs = [
          cctools
          rcodesign
          gnutar
          gzip
        ];
      }
      ''
        stage=$NIX_BUILD_TOP/stage
        bash ${./dist-macos.sh} \
          --out "$stage/felis-${triple}" \
          --triple ${triple} \
          --bin-dir ${felis}/bin \
          --share ${felis}/share \
          --version ${felis.version} \
          --readme ${../README.md} \
          --license ${../LICENSE} \
          --make-app ${./make-macos-app.sh}

        ${archive}
      '';

  linux =
    runCommand "felis-dist-${triple}-${felis.version}"
      {
        inherit meta;
        nativeBuildInputs = [
          binutils
          gnutar
          gzip
        ];
      }
      ''
        classify() {
          case "$1" in
          glibc-* | libffi-* | libx11-* | libxcb-* | libxau-* | libxdmcp-* \
            | libxcursor-* | libxrender-* | libxfixes-* | libxi-* | libxext-* \
            | libxrandr-* | wayland-*)
            echo bundled
            ;;
          # Only the runtime the felis binaries resolve: the rest of gcc's
          # lib output is the sanitizer and libstdc++ set, twenty
          # megabytes nothing in the archive loads.
          gcc-*-lib | gcc-*-libgcc)
            echo "bundled lib/libgcc_s.so.1"
            ;;
          # libxkbregistry lists RMLVO layouts for configuration UIs and is
          # the one file here that needs the excluded XML stack.
          libxkbcommon-*)
            echo "bundled lib/libxkbcommon.so.0 lib/libxkbcommon-x11.so.0"
            ;;
          vulkan-loader-* | libglvnd-* | fontconfig-* | freetype-* | expat-* \
            | libxml2-* | brotli-* | bzip2-* | libpng-* | zlib-* \
            | dejavu-fonts-minimal-* | libidn2-* | libunistring-* \
            | xgcc-*-libgcc | xkeyboard-config-* | bash-* | felis-*)
            echo host
            ;;
          *)
            echo unclassified
            ;;
          esac
        }

        libArgs=()
        unclassified=()
        while read -r path; do
          name=''${path#/nix/store/}
          name=''${name#*-}
          read -r class files <<<"$(classify "$name")"
          case "$class" in
          bundled)
            if [ -z "$files" ]; then
              libArgs+=(--lib-from "$path")
            else
              for file in $files; do
                libArgs+=(--lib-from "$path/$file")
              done
            fi
            ;;
          host) ;;
          *) unclassified+=("$name") ;;
          esac
        done < ${closure}/store-paths

        if [ ''${#unclassified[@]} -gt 0 ]; then
          echo "felis-dist: closure paths in neither list: ''${unclassified[*]}" >&2
          echo "felis-dist: classify each in nix/dist.nix as bundled or host-supplied" >&2
          exit 1
        fi

        stage=$NIX_BUILD_TOP/stage
        bash ${./dist-linux.sh} \
          --out "$stage/felis-${triple}" \
          --triple ${triple} \
          --bin-dir ${felis}/bin \
          --share ${felis}/share \
          --x11-locale ${libx11}/share/X11/locale \
          --readme ${../README.md} \
          --license ${../LICENSE} \
          "''${libArgs[@]}"

        ${archive}
      '';
in
if stdenv.hostPlatform.isDarwin then darwin else linux
