# The bench binary hands its run to this runner and aborts unless the two
# versions match exactly, and nixpkgs does not package it. `version` must
# equal the `gungraun` entry in ../../Cargo.lock; flake-module.nix asserts it.
{
  lib,
  rustPlatform,
  fetchCrate,
}:

rustPlatform.buildRustPackage (finalAttrs: {
  pname = "gungraun-runner";
  version = "0.20.0";

  src = fetchCrate {
    inherit (finalAttrs) pname version;
    hash = "sha256-XZHnq0t5XI/lSAdtP0UYxcaaV/YVZOmT/E7YQWX7jOU=";
  };

  cargoHash = "sha256-nYOFsVAuLw13XObUO4mS4edlv5N/BSHRYPPS1N7y0Os=";

  # Its tests drive valgrind and fixture benchmarks the sandbox cannot run.
  doCheck = false;

  meta = {
    description = "Runner binary for gungraun (Valgrind-based benchmarks)";
    homepage = "https://github.com/gungraun/gungraun";
    license = with lib.licenses; [
      asl20
      mit
    ];
    mainProgram = "gungraun-runner";
    platforms = lib.platforms.linux;
  };
})
