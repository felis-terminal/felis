{
  # Development-only inputs for the dev partition (see partitions.dev in
  # ../flake.nix). Kept out of the root inputs so users and downstream
  # consumers never fetch them.
  #
  # Named nixpkgs-dev so the partition's inputs.nixpkgs still resolves
  # to the root pin: dev shells and checks build against the same
  # nixpkgs revision as the shipped package.
  description = "felis development inputs (see partitions.dev in ../flake.nix)";

  inputs = {
    nixpkgs-dev.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs-dev";
    };
    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "nixpkgs-dev";
    };
    git-hooks = {
      url = "github:cachix/git-hooks.nix";
      inputs.nixpkgs.follows = "nixpkgs-dev";
    };
    # Only the home-manager-module check uses this; the shipped modules are
    # input-free and evaluated by the consumer's own home-manager.
    # master rather than a release-* branch: Home Manager release branches pin
    # to matching nixpkgs releases, whereas nixpkgs here tracks unstable.
    home-manager = {
      url = "github:nix-community/home-manager";
      inputs.nixpkgs.follows = "nixpkgs-dev";
    };
    # The bench field's ghostty tip on Linux (dev/bench/devshell.nix). Not
    # following nixpkgs-dev: upstream pins the Zig its build needs through
    # its own nixpkgs, and a different one can fail to compile the tip.
    ghostty.url = "github:ghostty-org/ghostty";
  };

  # Empty on purpose: the root flake only reads this flake's inputs.
  outputs = { ... }: { };
}
