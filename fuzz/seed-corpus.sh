#!/usr/bin/env bash
# Prime `fuzz/corpus/<target>/` with the curated seeds in
# `fuzz/seeds/<target>/` for every cargo-fuzz target.
#
# Idempotent: copying the same seed twice is harmless (cargo-fuzz
# dedupes by content). Run before `cargo fuzz run <target>` if
# the working corpus is empty or stale; nightly CI re-runs this
# before each long-run pass.
#
# See `fuzz/seeds/README.md` for the seed-naming convention and
# why curated seeds live separately from the libFuzzer corpus.

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
seeds_root="$script_dir/seeds"
corpus_root="$script_dir/corpus"

if [[ ! -d "$seeds_root" ]]; then
  echo "no seeds directory at $seeds_root" >&2
  exit 1
fi

shopt -s nullglob
copied=0
for target_seeds in "$seeds_root"/*/; do
  target="$(basename "$target_seeds")"
  [[ "$target" == "README.md" ]] && continue
  target_corpus="$corpus_root/$target"
  mkdir -p "$target_corpus"
  for seed in "$target_seeds"*.bin; do
    cp -f "$seed" "$target_corpus/"
    copied=$((copied + 1))
  done
done

echo "primed $copied seed(s) into $corpus_root/"
