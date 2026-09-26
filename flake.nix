{
  description = "felis — GPU-accelerated terminal emulator";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    crane.url = "github:ipetkov/crane";
  };

  nixConfig = {
    extra-substituters = [ "https://nix-cache.natsukium.com" ];
    extra-trusted-public-keys = [
      "niks3-1:SoIFTPtiPoCW3/OzUkIBKlLG5znMZfbihlr11XAOles="
    ];
  };

  outputs =
    inputs@{ flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];

      imports = [ flake-parts.flakeModules.partitions ];

      # Shells, checks, and the formatter live in the dev partition, whose
      # inputs extend these with dev/flake.nix. Users never fetch them, while
      # `nix develop`, `nix fmt`, and `nix flake check` keep working at root.
      partitionedAttrs = {
        checks = "dev";
        devShells = "dev";
        formatter = "dev";
      };
      partitions.dev = {
        extraInputsFlake = ./dev;
        # Pin dev tools to the root nixpkgs so there is a single revision
        # in the dev partition. dev/flake.nix keeps its own nixpkgs-dev for
        # standalone `nix flake lock --flake dev`, but it is not fetched via
        # the root partition.
        extraInputs.nixpkgs-dev = inputs.nixpkgs;
        module = {
          imports = [ ./dev/flake-module.nix ];
        };
      };

      perSystem =
        {
          config,
          lib,
          pkgs,
          system,
          ...
        }:
        {
          # Default priority so the dev partition can overlay the toolchains it needs.
          _module.args.pkgs = lib.mkDefault (import inputs.nixpkgs { inherit system; });

          packages = {
            felis = pkgs.callPackage ./nix/package.nix {
              craneLib = inputs.crane.mkLib pkgs;
              # Full revisions, not shortRev: the canonical build identity is
              # `<semver> (<rev40>[-dirty])`, and an abbreviation whose width follows
              # the local object store would not parse back (crates/felis-protocol/src/build_identity.rs).
              # rev exists only for clean trees, dirtyRev for dirty trees; tarball falls back to null ("unknown").
              gitHash = inputs.self.rev or inputs.self.dirtyRev or null;
            };
            # Separate derivation so doc edits avoid rebuilding package binaries.
            # Kept as markdown rather than man pages: roff degrades cross-linked/table-heavy
            # prose and ~40 man7 pages would flood the man namespace.
            docs = pkgs.runCommand "felis-docs" { } ''
              mkdir -p $out/share/doc
              cp -r ${./docs} $out/share/doc/felis
            '';
            default = config.packages.felis;
            # The archive a host without Nix unpacks and runs; nix/dist.nix
            # picks the assembler for the host.
            felis-dist = pkgs.callPackage ./nix/dist.nix {
              inherit (config.packages) felis;
            };
          };
        };

      flake =
        let
          felisModule =
            { pkgs, ... }:
            {
              imports = [ ./nix/hm-module.nix ];
              _module.args.craneLib = inputs.crane.mkLib pkgs;
            };
        in
        {
          overlays.default = final: _prev: {
            felis = final.callPackage ./nix/package.nix {
              craneLib = inputs.crane.mkLib final;
            };
          };

          homeManagerModules = {
            # Uses importing configuration's nixpkgs to avoid pulling a second nixpkgs closure.
            felis = felisModule;
            default = felisModule;
            stylix = ./nix/stylix.nix;
          };
        };
    };
}
