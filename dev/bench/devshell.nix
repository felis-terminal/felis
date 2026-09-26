# Pinned toolchains for cross-terminal benchmarks so measurements do not depend on host PATH.
_: {
  perSystem =
    {
      config,
      inputs',
      lib,
      pkgs,
      ...
    }:
    let
      inherit (pkgs.stdenv.hostPlatform) isDarwin isLinux;

      # nixpkgs darwin termbench-pro has a broken install_name; DYLD_LIBRARY_PATH avoids rebuilding the C++ closure via overlay.
      tb =
        if !isDarwin then
          pkgs.termbench-pro
        else
          pkgs.runCommand "termbench-pro-wrapped"
            {
              nativeBuildInputs = [ pkgs.makeWrapper ];
            }
            ''
              makeWrapper ${pkgs.termbench-pro}/bin/tb $out/bin/tb \
                --prefix DYLD_LIBRARY_PATH : ${pkgs.termbench-pro}/lib
            '';

      # Patched for fixed-length bench mode because upstream interactive demo blocks on keystrokes.
      doom-fire = pkgs.stdenv.mkDerivation {
        pname = "doom-fire-zig";
        version = "0-unstable-2025-08-20";
        src = pkgs.fetchFromGitHub {
          owner = "const-void";
          repo = "DOOM-fire-zig";
          rev = "eb0631b141b5778eefc6f5767bb45f8974c1be71";
          hash = "sha256-BjU2ODsW+JaBhm9E07mOE1+0zQlXjrGHM4qpfIXu8Yw=";
        };
        patches = [ ../../tools/bench/doom-fire-bench.patch ];
        nativeBuildInputs = [ pkgs.zig_0_14.hook ];
        # `--release=fast` over `--release=safe`: avoids ~6% CPU overhead from per-pixel bounds checks.
        dontSetZigDefaultFlags = true;
        zigBuildFlags = [
          "-Dcpu=baseline"
          "--release=fast"
        ];
        meta.mainProgram = "DOOM-fire";
      };

      # Headless entry point (Main.java) for unattended benchmarking without upstream's interactive Swing UI.
      typometer = pkgs.stdenv.mkDerivation {
        pname = "typometer-headless";
        version = "1.0.1-unstable-2017-09-22";
        src = pkgs.fetchFromGitHub {
          owner = "pavelfatin";
          repo = "typometer";
          rev = "2084acfec845fdb3042f0155adffd88f14117c00";
          hash = "sha256-EQXEentKCEyMWs97xDM5JhQSOhKRFmauKYF4im48dhQ=";
        };
        nativeBuildInputs = [
          pkgs.jdk
          pkgs.makeBinaryWrapper
        ];
        buildPhase = ''
          runHook preBuild
          javac -nowarn -d classes -sourcepath src/main/java \
            -cp ${pkgs.jna}/share/java/jna.jar ${../../tools/bench/typometer}/Main.java
          runHook postBuild
        '';
        installPhase = ''
          runHook preInstall
          mkdir -p $out/share/java
          cp -r classes $out/share/java/typometer
          # `UIElement=true` prevents the JVM from stealing focus and typing into itself during latency measurement.
          makeWrapper ${lib.getExe' pkgs.jdk "java"} $out/bin/typometer \
            --add-flags "-cp $out/share/java/typometer:${pkgs.jna}/share/java/jna.jar" \
            --add-flags "-Djava.awt.headless=false" \
            --add-flags "-Dapple.awt.UIElement=true" \
            --add-flags felis.bench.Main
          runHook postInstall
        '';
        meta.mainProgram = "typometer";
      };

      # The Linux half of the latency suite. Its own crate rather than a
      # workspace member, so a benchmark instrument never enters the felis
      # dependency graph (workspace.md).
      wl-latency = pkgs.rustPlatform.buildRustPackage {
        pname = "wl-latency";
        version = "0.1.0";
        src = ../../tools/bench/wl-latency;
        cargoLock.lockFile = ../../tools/bench/wl-latency/Cargo.lock;
        buildInputs = [ pkgs.wayland ];
        meta.mainProgram = "wl-latency";
      };

      # ghostty-bin on Darwin: nixpkgs source build requires Xcode/Swift 6 unavailable in Nix Darwin.
      terminals =
        with pkgs;
        [
          kitty
          alacritty
          wezterm
        ]
        ++ lib.optional isDarwin ghostty-bin
        ++ lib.optionals isLinux [
          ghostty
          foot
        ];

      paths = {
        FELIS_BENCH_VTEBENCH = "${pkgs.vtebench}/bin/vtebench";
        # nixpkgs installs benchmarks separately; vtebench requires `-b` to find them.
        FELIS_BENCH_VTEBENCH_BENCHMARKS = "${pkgs.vtebench}/share/vtebench/benchmarks";
        FELIS_BENCH_TB = "${tb}/bin/tb";
        FELIS_BENCH_HYPERFINE = "${pkgs.hyperfine}/bin/hyperfine";
        FELIS_BENCH_DOOM_FIRE = "${doom-fire}/bin/DOOM-fire";
        FELIS_BENCH_TYPOMETER = "${typometer}/bin/typometer";
        # Pinned separately from FELIS_BENCH_KITTY so swapping the target kitty binary retains the benchmark driver.
        FELIS_BENCH_KITTEN = "${pkgs.kitty}/bin/kitten";
        FELIS_BENCH_KITTY = "${pkgs.kitty}/bin/kitty";
        FELIS_BENCH_ALACRITTY = "${pkgs.alacritty}/bin/alacritty";
        FELIS_BENCH_WEZTERM = "${pkgs.wezterm}/bin/wezterm";
      }
      // lib.optionalAttrs isDarwin {
        FELIS_BENCH_GHOSTTY = "${pkgs.ghostty-bin}/bin/ghostty";
      }
      // lib.optionalAttrs isLinux {
        FELIS_BENCH_GHOSTTY = "${pkgs.ghostty}/bin/ghostty";
        # Only as a path, not in `packages`: both builds are named `ghostty`,
        # and the release must stay the one on PATH.
        FELIS_BENCH_GHOSTTY_TIP = "${inputs'.ghostty.packages.ghostty}/bin/ghostty";
        FELIS_BENCH_FOOT = "${pkgs.foot}/bin/foot";
        FELIS_BENCH_WL_LATENCY = "${wl-latency}/bin/wl-latency";
      };
    in
    {
      devShells.bench = pkgs.mkShell {
        # Separate shell so default nix develop avoids pulling other terminal emulator closures.
        inputsFrom = [ config.devShells.default ];
        packages = [
          pkgs.vtebench
          tb
          pkgs.hyperfine
          pkgs.jq
          doom-fire
          typometer
          (pkgs.python3.withPackages (ps: [ ps.matplotlib ]))
        ]
        ++ lib.optional isLinux wl-latency
        ++ terminals;
        shellHook = lib.concatStringsSep "\n" (
          [ "export FELIS_BENCH_SHELL=1" ]
          ++ lib.mapAttrsToList (name: value: "export ${name}=${lib.escapeShellArg value}") paths
        );
      };
    };
}
