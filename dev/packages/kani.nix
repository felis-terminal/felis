# kani-compiler links rustc_private and must be built with kani's exact pinned nightly.
# Avoids `cargo kani setup` (network downloads forbidden in Nix sandbox) by using nixpkgs cbmc
# and precompiled sysroot/library from the release tarball.

# Revisit if: nixpkgs ships a `kani` package (drop this file); the sysroot becomes buildable from source in one
# derivation (drop the tarball borrow and the tag pin, track main); proofs need to run on aarch64-linux or darwin;
# or CBMC version drift in nixpkgs breaks goto-binary compatibility with the pinned Kani.
{
  lib,
  stdenv,
  fetchFromGitHub,
  fetchzip,
  rust-bin,
  makeRustPlatform,
  rsync,
  makeWrapper,
  autoPatchelfHook,
  glibc,
  cbmc,
  kissat,
}:

let
  version = "0.67.0";

  # Pinned to kani release's rust-toolchain.toml: precompiled sysroot is MIR-locked to this exact nightly.
  rustToolchainDate = "2025-11-21";

  src = fetchFromGitHub {
    owner = "model-checking";
    repo = "kani";
    tag = "kani-${version}";
    fetchSubmodules = true;
    hash = "sha256-Advfh0BWvvEbnwWvTpHzu/7MI9P0/dhzvtX9r2qnXeI=";
  };

  kaniRustToolchain = rust-bin.nightly.${rustToolchainDate}.default.override {
    extensions = [
      "rustc-dev"
      "rust-src"
      "llvm-tools"
      "rustfmt"
    ];
  };

  rustPlatform = makeRustPlatform {
    cargo = kaniRustToolchain;
    rustc = kaniRustToolchain;
  };

  # Precompiled sysroot/library from release tarball. kani-compiler is excluded to link against Nix nightly rustc_private.
  kaniHome = stdenv.mkDerivation {
    pname = "kani-home";
    inherit version;

    src = fetchzip {
      url = "https://github.com/model-checking/kani/releases/download/kani-${version}/kani-${version}-x86_64-unknown-linux-gnu.tar.gz";
      hash = "sha256-I+GKPEWYXPZimCN79IB9dKiY8+NhP4Y8JjAS7R00XMs=";
    };

    buildInputs = [ stdenv.cc.cc.lib ];

    # patchelf DT_NEEDED scan misses glibc for some bundled binaries.
    runtimeDependencies = [ glibc ];

    nativeBuildInputs = [ autoPatchelfHook ];

    installPhase = ''
      runHook preInstall
      mkdir -p $out
      ${rsync}/bin/rsync -av $src/ $out --exclude kani-compiler
      runHook postInstall
    '';

    dontConfigure = true;
    dontBuild = true;
  };
in
# Only the x86_64-linux release tarball is wired up; upstream does publish
# aarch64-linux + darwin tarballs as of 0.67.0 — widen once tested.
assert lib.assertMsg (
  stdenv.hostPlatform.system == "x86_64-linux"
) "dev/packages/kani.nix: only x86_64-linux is wired up (got ${stdenv.hostPlatform.system})";

rustPlatform.buildRustPackage {
  pname = "kani";
  inherit version src;

  # fetchCargoVendor over cargoLock.lockFile: handles git dependencies (Charon submodule).
  cargoDeps = rustPlatform.fetchCargoVendor {
    inherit src;
    hash = "sha256-vH4eslc5wm7YVNwXWGEtlWyZwxXIxXwgDVli681ENGY=";
  };

  nativeBuildInputs = [ makeWrapper ];

  # Satisfies kani's rustup layout expectations without installing rustup (".." selects the Nix toolchain).
  env = {
    RUSTUP_HOME = "${kaniRustToolchain}";
    RUSTUP_TOOLCHAIN = "..";
  };

  doCheck = false;

  postInstall = ''
    install -d "$out/lib/kani-${version}"

    # --chmod makes store-sourced read-only files writable to install kani-compiler into the bundle.
    ${rsync}/bin/rsync -av ${kaniHome}/ "$out/lib/kani-${version}" \
      --perms --chmod=D+rwx,F+rw

    # kani resolves kani-compiler relative to KANI_HOME/kani-<version>/bin rather than PATH.
    install -d "$out/lib/kani-${version}/bin"
    cp $out/bin/* "$out/lib/kani-${version}/bin/"

    ln -s ${kaniRustToolchain} "$out/lib/kani-${version}/toolchain"
  '';

  # Wrapped rather than patched: KANI_HOME is read from environment and CBMC tools are resolved via PATH.
  postFixup = ''
    for entry in kani cargo-kani; do
      if [ -e "$out/bin/$entry" ]; then
        wrapProgram "$out/bin/$entry" \
          --set KANI_HOME "$out/lib" \
          --prefix PATH : "${
            lib.makeBinPath [
              cbmc
              kissat
            ]
          }"
      fi
    done
  '';

  passthru.toolchain = kaniRustToolchain;

  meta = {
    description = "Kani Rust model checker (bit-precise verification via CBMC)";
    homepage = "https://github.com/model-checking/kani";
    license = with lib.licenses; [
      asl20
      mit
    ];
    platforms = [ "x86_64-linux" ];
    mainProgram = "cargo-kani";
  };
}
