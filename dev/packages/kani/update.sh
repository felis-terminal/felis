file=dev/packages/kani/default.nix
[ -f "$file" ] || {
  echo "run from the repository root" >&2
  exit 1
}

tag=$(curl -fsSL https://api.github.com/repos/model-checking/kani/releases/latest | jq -r .tag_name)
version=${tag#kani-}
current=$(sed -n 's/^  version = "\(.*\)";$/\1/p' "$file")
if [ "$version" = "$current" ]; then
  echo "kani is already at $current"
  exit 0
fi

date=$(curl -fsSL "https://raw.githubusercontent.com/model-checking/kani/$tag/rust-toolchain.toml" |
  sed -n 's/^channel = "nightly-\(.*\)"$/\1/p')
[ -n "$date" ] || {
  echo "no nightly channel in $tag's rust-toolchain.toml" >&2
  exit 1
}
sed -i -e "s/^  version = \".*\";$/  version = \"$version\";/" \
  -e "s/^  rustToolchainDate = \".*\";$/  rustToolchainDate = \"$date\";/" "$file"

if ! nix eval --raw .#kani.toolchain.drvPath >/dev/null 2>&1; then
  echo "nightly-$date is missing from the locked rust-overlay; updating it"
  nix flake update rust-overlay --flake ./dev
fi

# nix-update cannot evaluate a flake-parts partition, so each hash is computed here.
set_hash() {
  sed -i "/$1 {/,/};/ s|hash = \".*\";|hash = \"$2\";|" "$file"
}

set_hash fetchFromGitHub "$(nix-prefetch-git --quiet --fetch-submodules \
  --url https://github.com/model-checking/kani --rev "refs/tags/$tag" | jq -r .hash)"

set_hash fetchzip "$(nix store prefetch-file --unpack --json \
  "https://github.com/model-checking/kani/releases/download/$tag/$tag-x86_64-unknown-linux-gnu.tar.gz" |
  jq -r .hash)"

set_hash fetchCargoVendor "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
cargo_hash=$(nix build --no-link .#kani.cargoDeps 2>&1 | sed -n 's/^ *got: *\(sha256-.*\)$/\1/p' || true)
[ -n "$cargo_hash" ] || {
  echo "could not determine the cargoDeps hash" >&2
  exit 1
}
set_hash fetchCargoVendor "$cargo_hash"

echo "kani $current -> $version (nightly-$date); verify with: just kani"
