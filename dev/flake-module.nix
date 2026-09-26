# Development partition (partitions.dev in ../flake.nix): toolchains,
# shells, checks, formatters, hooks. Evaluated only for dev outputs, so
# dev-only inputs never burden users of the root flake.
{ inputs, ... }:
{
  imports = [
    inputs.treefmt-nix.flakeModule
    inputs.git-hooks.flakeModule
    ./bench/devshell.nix
  ];

  perSystem =
    {
      config,
      pkgs,
      system,
      ...
    }:
    let
      mkNightlyToolchain =
        targets:
        pkgs.rust-bin.selectLatestNightlyWith (
          t:
          t.default.override {
            extensions = [
              "rust-src"
              "rust-analyzer"
              "llvm-tools-preview"
            ];
            inherit targets;
          }
        );
      # Includes wasm32-unknown-unknown so CI guards felis-client-core's portable core against feature-split bitrot.
      rustToolchain = mkNightlyToolchain [ "wasm32-unknown-unknown" ];

      # Kept out of default toolchain so mingw cc never shadows the native compiler (which breaks libFuzzer builds).
      windowsRustToolchain = mkNightlyToolchain [
        "x86_64-pc-windows-gnu"
        "x86_64-pc-windows-msvc"
      ];

      # rust-overlay keys stable releases by full x.y.z, while Cargo.toml's rust-version may omit the patch version.
      msrvVersion = (builtins.fromTOML (builtins.readFile ../Cargo.toml)).workspace.package.rust-version;
      msrvFull =
        if builtins.match "[0-9]+\\.[0-9]+" msrvVersion != null then "${msrvVersion}.0" else msrvVersion;
      msrvToolchain = pkgs.rust-bin.stable.${msrvFull}.default;

      runtimeLibs =
        with pkgs;
        [
          fontconfig
          freetype
        ]
        ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
          vulkan-loader
          libxkbcommon
          wayland
          libGL
          libx11
          libxcursor
          libxi
          libxrandr
          libxcb
        ];

      esctest = pkgs.callPackage ./packages/esctest.nix { };

      # The Nix profile aggregates terminfo automatically, but dev shells do not;
      # without this, programs in dev-daemon sessions abort with "unknown terminal type".
      felisTerminfo =
        pkgs.runCommand "felis-terminfo"
          {
            nativeBuildInputs = [ pkgs.ncurses ];
          }
          ''
            bash ${../nix/compile-terminfo.sh} \
              ${../share/terminfo/felis.terminfo} $out/share/terminfo
          '';

      # Noto Sans Mono is excluded so Family::Monospace resolves to Monaspace in shaping tests.
      # Pinned font set prevents tests from skipping silently on machines lacking ligature or emoji fonts.
      testFonts = pkgs.runCommand "felis-test-fonts" { } ''
        mkdir -p $out/share/fonts
        cp -r ${pkgs.monaspace}/share/fonts/. $out/share/fonts/
        cp ${pkgs.noto-fonts}/share/fonts/noto/NotoSansSymbols.ttf $out/share/fonts/
        cp ${pkgs.noto-fonts}/share/fonts/noto/NotoSansSymbols2-Regular.otf $out/share/fonts/
        cp ${pkgs.noto-fonts-color-emoji}/share/fonts/noto/NotoColorEmoji.ttf $out/share/fonts/
      '';

      kani = pkgs.callPackage ./packages/kani.nix { };

      devTools =
        with pkgs;
        [
          pkg-config
          just
          prek
          cargo-deny
          cargo-machete
          lychee
          cargo-nextest
          cargo-insta
          cargo-fuzz
          cargo-mutants
          cargo-watch
          samply
          cargo-flamegraph
          # Conformance harnesses required so felis-pty integration tests do not skip silently.
          esctest
          vttest
          # localhost-SSH tests skip when sshd is missing; sshd usually lives outside default PATH (/usr/sbin).
          openssh
          # Completion tests drive generated fish/zsh scripts and skip when interpreters are missing.
          fish
          zsh
          python3
          yq-go
          buf
          protoc-gen-prost
          config.treefmt.build.wrapper
        ]
        ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
          # Fast linker for the per-mutant relinks (and dev builds generally).
          mold
        ];
    in
    {
      # Overrides the root default: dev shells and checks need the nightly
      # and stable toolchains from rust-overlay.
      _module.args.pkgs = import inputs.nixpkgs {
        inherit system;
        overlays = [ inputs.rust-overlay.overlays.default ];
      };

      checks = {
        # MSRV gate as a derivation rather than a CI cargo job: run under cargo
        # it grew a second toolchain's worth of artifacts in the Forgejo runner's
        # shared CARGO_TARGET_DIR while reusing none of the nightly build job's,
        # since a stable rustc means a different unit hash. Here it carries its
        # own store-cached dependency build and leaves that directory alone.
        msrv =
          let
            craneLibMsrv = (inputs.crane.mkLib pkgs).overrideToolchain msrvToolchain;
            # --all-features over the shipped feature set: the gate has to reach
            # every cfg-gated path, not only the ones the package turns on.
            cargoArgs = "--locked --workspace --all-targets --all-features";
            args = config.packages.felis.commonArgs // {
              pname = "felis-msrv";
              cargoExtraArgs = cargoArgs;
            };
          in
          craneLibMsrv.mkCargoDerivation (
            builtins.removeAttrs args [ "cargoExtraArgs" ]
            // {
              # check over build: the question is whether the language and std
              # surface the code uses exists in the floor release, not codegen.
              cargoArtifacts = craneLibMsrv.buildDepsOnly (
                args // { cargoBuildCommand = "cargoWithProfile check"; }
              );
              buildPhaseCargoCommand = "cargoWithProfile check ${cargoArgs}";
            }
          );

        # Evaluates module options during `nix flake check`.
        # `package` is stubbed so this check evaluates the module rather than
        # building felis (gated by packages.felis; the cargo work here is msrv's).
        # `settings` and `notifications` are populated to exercise TOML generation and systemd/launchd units.
        # stylix stays out to avoid carrying a Stylix flake input for an optional target.
        home-manager-module =
          (inputs.home-manager.lib.homeManagerConfiguration {
            inherit pkgs;
            modules = [
              inputs.self.homeManagerModules.felis
              {
                # Home Manager's manual generation dominates check build time while testing nothing in felis.
                manual.manpages.enable = false;
                home = {
                  username = "felis";
                  homeDirectory = if pkgs.stdenv.hostPlatform.isDarwin then "/Users/felis" else "/home/felis";
                  stateVersion = "24.11";
                };
                programs.felis = {
                  enable = true;
                  package = pkgs.writeShellScriptBin "felis" "";
                  settings = {
                    font = {
                      family = "Monaspace Neon";
                      size = 14.0;
                    };
                    theme.background = "#0d0d12";
                  };
                  notifications.enable = true;
                };
              }
            ];
          }).activationPackage;
      };

      treefmt = {
        projectRootFile = "flake.nix";
        # Excluded so rustfmt does not desync committed prost output from buf codegen.
        settings.global.excludes = [
          "crates/felis-protocol/src/generated/**"
        ];
        programs.nixfmt.enable = true;
        programs.rustfmt = {
          enable = true;
          package = rustToolchain;
          edition = "2024";
        };
        settings.formatter.rustfmt.options = [
          "--config"
          "newline_style=Unix"
        ];
        programs.taplo.enable = true;
        programs.buf.enable = true;
        # ruff-format over black: faster and avoids introducing a separate toolchain for tools/*.py.
        programs.ruff-format.enable = true;
        settings.formatter.oxfmt = {
          command = "${pkgs.oxfmt}/bin/oxfmt";
          options = [
            "--config"
            (builtins.toFile "oxfmtrc.json" (
              builtins.toJSON {
                printWidth = 120;
                proseWrap = "always";
              }
            ))
            "--write"
          ];
          includes = [ "*.md" ];
        };
      };

      pre-commit.settings = {
        package = pkgs.prek;
        hooks = {
          treefmt.enable = true;
          check-merge-conflicts.enable = true;
          buf-lint = {
            enable = true;
            name = "buf lint";
            description = "lint the IPC wire schema (felis.proto)";
            entry = toString (
              pkgs.writeShellScript "felis-buf-lint" ''
                cd crates/felis-protocol
                exec ${pkgs.buf}/bin/buf lint
              ''
            );
            files = "\\.proto$";
            pass_filenames = false;
          };
          # Exits 0 when outside a git repo because `nix flake check` evaluates in a store copy without `.git`.
          buf-breaking = {
            enable = true;
            name = "buf breaking";
            description = "warn on a wire-incompatible felis.proto change vs HEAD (the CI gate is `just proto-compat`; see proto/BREAKING.md for an intended break)";
            entry = toString (
              pkgs.writeShellScript "felis-buf-breaking" ''
                ${pkgs.git}/bin/git rev-parse --is-inside-work-tree >/dev/null 2>&1 || exit 0
                exec ${pkgs.buf}/bin/buf breaking crates/felis-protocol --against '.git#ref=HEAD'
              ''
            );
            files = "\\.proto$";
            pass_filenames = false;
          };
          # Regenerating in a hook rather than a CI job: git-hooks.nix runs every
          # hook under `nix flake check` against a committed scratch repo, so one
          # definition gates both the commit and CI, and a stale committed copy
          # shows up as a hook-modified file. No network is involved: buf.yaml has
          # no BSR dependencies and buf.gen.yaml drives a local plugin.
          buf-generate = {
            enable = true;
            name = "buf generate";
            description = "regenerate the prost wire types; fails when the committed copy is stale (`just proto`)";
            entry = toString (
              pkgs.writeShellScript "felis-buf-generate" ''
                export PATH=${pkgs.lib.makeBinPath [ pkgs.protoc-gen-prost ]}:$PATH
                cd crates/felis-protocol
                exec ${pkgs.buf}/bin/buf generate
              ''
            );
            # buf.gen.yaml joins the schema: a plugin or option change drifts the output too.
            files = "(\\.proto|buf\\.gen\\.yaml)$";
            pass_filenames = false;
          };
          # Added lines of the staged diff, not a whole-tree lint: the
          # committed tree carries hits the norms postdate, and gating the
          # whole tree would force a cleanup commit before anyone could
          # commit anything. Not a Rust lint either: the subject is prose,
          # in Markdown as much as in comments, and these are the findings
          # the review panel kept reporting by hand.
          prose-check = {
            enable = true;
            name = "prose check";
            description = "mechanical felis prose norms on added lines (`just prose-check`)";
            entry = toString (
              pkgs.writeShellScript "felis-prose-check" ''
                exec ${pkgs.python3}/bin/python3 tools/prose_check.py "$@"
              ''
            );
            files = "^(docs/.*\\.md|skills/.*\\.md|\\.agents/skills/.*\\.md|\\.claude/skills/.*\\.md|[^/]*\\.md|crates/.*\\.rs|proto/.*\\.proto|\\.forgejo/.*\\.ya?ml)$";
            pass_filenames = true;
          };
          lychee = {
            enable = true;
            files = "^docs/.*\\.md$";
            pass_filenames = false;
            extraPackages = [ pkgs.cacert ];
            settings.flags = "--offline --no-progress 'docs/**/*.md'";
          };
          skill-check = {
            enable = true;
            name = "skill frontmatter check";
            description = "validate SKILL.md YAML frontmatter and required metadata";
            entry = toString (
              pkgs.writeShellScript "felis-skill-check" ''
                export PATH=${pkgs.lib.makeBinPath [ pkgs.yq-go ]}:$PATH
                exec ${pkgs.python3}/bin/python3 tools/skill_check.py "$@"
              ''
            );
            files = "^(\\.agents/skills|skills)/.*/SKILL\\.md$";
            pass_filenames = true;
          };
          prose-check-self-test = {
            enable = true;
            name = "prose check self-test";
            description = "exercise every prose rule against its fixture when the checker changes";
            entry = toString (
              pkgs.writeShellScript "felis-prose-check-self-test" ''
                exec ${pkgs.python3}/bin/python3 tools/prose_check.py --self-test
              ''
            );
            files = "^tools/prose_check\\.py$";
            pass_filenames = false;
          };
        };
      };

      devShells = {
        default = pkgs.mkShell {
          packages = [
            rustToolchain
          ]
          ++ devTools
          ++ runtimeLibs;
          shellHook = ''
            ${config.pre-commit.installationScript}
            export LD_LIBRARY_PATH=${pkgs.lib.makeLibraryPath runtimeLibs}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}
            export PATH=./target/debug:$PATH
            export TERMINFO_DIRS=${felisTerminfo}/share/terminfo''${TERMINFO_DIRS:+:$TERMINFO_DIRS}
            # Shaping tests skip when unset so builds outside Nix stay green.
            export FELIS_TEST_FONT_DIR=${testFonts}/share/fonts
            ${pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isDarwin ''
              # Bundled LLVM tools link @rpath/libLLVM.dylib which fails to resolve during cargo release strip.
              export DYLD_FALLBACK_LIBRARY_PATH=${rustToolchain}/lib''${DYLD_FALLBACK_LIBRARY_PATH:+:$DYLD_FALLBACK_LIBRARY_PATH}
            ''}
            ${pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
              # mold over BFD: every mutant pays a relink, so shorter link time
              # multiplies across the sweep. RUSTFLAGS here, not
              # .cargo/config.toml, so builds outside this shell keep the
              # default linker.
              export RUSTFLAGS="''${RUSTFLAGS:+$RUSTFLAGS }-C link-arg=-fuse-ld=mold"
            ''}
          '';
        };

        # Kept separate from default shell so mingw cc does not shadow the native C toolchain.
        windows = pkgs.mkShell {
          packages = [
            windowsRustToolchain
            pkgs.just
            pkgs.cargo-xwin
            pkgs.pkgsCross.mingwW64.stdenv.cc
          ];
          shellHook = ''
            export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-cc
          '';
        };

        # Verifies build against MSRV to catch accidental use of newer language or std features.
        msrv = pkgs.mkShell {
          packages = [
            msrvToolchain
            pkgs.pkg-config
            pkgs.cargo-nextest
          ]
          ++ runtimeLibs;
          shellHook = ''
            export LD_LIBRARY_PATH=${pkgs.lib.makeLibraryPath runtimeLibs}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}
          '';
        };

        # Pins tools for publishing to binary cache and the release page; niks3-action post-build hook missed pre-built smoke artifacts.
        publish = pkgs.mkShell {
          packages = [
            pkgs.niks3
            pkgs.curl
            pkgs.jq
            pkgs.python3
          ];
        };
      }
      // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
        # Xvfb over Wayland: avoids compositor setup (seats/renderers); winit falls back to X11 when DISPLAY is set.
        # VK_DRIVER_FILES pins Lavapipe explicitly even if the host runner has a GPU driver.
        smoke = pkgs.mkShell {
          packages = [ pkgs.xvfb-run ];
          # LD_LIBRARY_PATH for the unwrapped cargo binaries a PR smoke drives;
          # the package's wrapper prefixes its own.
          shellHook = ''
            export LD_LIBRARY_PATH=${pkgs.lib.makeLibraryPath runtimeLibs}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}
            export VK_DRIVER_FILES=${pkgs.mesa}/share/vulkan/icd.d/lvp_icd.${pkgs.stdenv.hostPlatform.parsed.cpu.name}.json
          '';
        };
      }
      // pkgs.lib.optionalAttrs (system == "x86_64-linux") {
        kani = pkgs.mkShell {
          packages = [
            kani
            kani.toolchain
            pkgs.pkg-config
          ];
        };
      };
    };
}
