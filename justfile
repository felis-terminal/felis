# just over mise/cargo-make: does not manage toolchains, avoiding collisions with Nix.
set windows-shell := ["powershell.exe", "-NoLogo", "-Command"]

# Wayland clipboard backend is Linux-only; omitted on macOS/Windows.
clipboard := if os() == "linux" { "--features felis-client/wayland-clipboard" } else { "" }

# Show the recipe list (default target).
default:
    @just --list

# ── Build ───────────────────────────────────────────────────────────

# Debug build of the whole workspace (Wayland clipboard on Linux).
[group('build')]
build:
    cargo build --workspace {{ clipboard }}

# Optimised release build of the whole workspace (Wayland clipboard on Linux).
[group('build')]
build-release:
    cargo build --workspace --release {{ clipboard }}

# Reproducible package build via Nix (mirrors darwin.yml). Linux/macOS.
[group('build')]
nix-build:
    nix build .#felis --print-build-logs

# Bundle a standalone macOS felis.app from a release build. macOS only.
[group('build')]
[macos]
macos-app: build-release
    nix/make-macos-app.sh

# ── Windows cross-compile (Linux/macOS) ─────────────────────────────
# Clippy over check: only pass that lints cfg(windows) code on non-Windows hosts.

# Cross-lint the Windows build, test targets included (windows.yml runs this).
[group('windows')]
check-windows:
    nix develop .#windows -c cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu -- -D warnings

# Link the Windows binaries via mingw-w64 (`-gnu` target).
[group('windows')]
build-windows-gnu:
    nix develop .#windows -c cargo build -p felis-client -p felis-daemon --target x86_64-pc-windows-gnu

# Build the shippable stand-alone .exe via cargo-xwin (`-msvc` target).
[group('windows')]
build-windows-msvc:
    nix develop .#windows -c cargo xwin build -p felis-client -p felis-daemon --target x86_64-pc-windows-msvc

# ── Test ────────────────────────────────────────────────────────────

# Run the workspace test suite (pr.yml's build job runs this).
[group('test')]
test:
    cargo nextest run --workspace --all-features

# Drive the frontend smoke on this display (uses cat so marker reaches screen via tty echo).
[group('test')]
smoke marker="felis-smoke-local": (_smoke marker "")

# Drive the frontend smoke headless, exactly as pr.yml's `linux-smoke` does.
[group('test')]
smoke-headless marker="felis-smoke-local": (_smoke marker "nix develop .#smoke -c xvfb-run -a")

_smoke marker wrapper:
    #!/usr/bin/env bash
    set -euo pipefail
    nix build .#felis --print-build-logs
    # A per-run 0700 directory: a socket parent must be one, and a fixed name would hand the run to
    # a daemon an earlier build left there. Smoke mode detaches once it has verified the marker, so
    # the trap stops the daemon it spawned and takes the directory with it, on failure as well.
    sockdir="$(mktemp -d "${TMPDIR:-/tmp}/felis-smoke.XXXXXX")"
    trap './result/bin/felis --socket "$sockdir/daemon.sock" daemon stop --force >/dev/null 2>&1 || true; rm -rf "$sockdir"' EXIT
    FELIS_SMOKE_MARKER={{ marker }} {{ wrapper }} ./result/bin/felis-client --socket "$sockdir/daemon.sock" -- cat

# Run the renderer tests that need a GPU adapter on software Vulkan (lavapipe), as pr.yml's `linux-smoke` does.
[group('test')]
test-gpu:
    VK_DRIVER_FILES="$(nix develop .#smoke -c printenv VK_DRIVER_FILES)" cargo nextest run -p felis-render-wgpu --all-features --run-ignored only

# Verify the code still compiles against the declared MSRV (mirrors checks.msrv).
# Dev shell over `nix build .#checks.<system>.msrv`: same command, but incremental
# against the local target dir instead of a cold sandbox build.
[group('test')]
test-msrv:
    nix develop .#msrv -c cargo check --workspace --all-targets --all-features

# ── Lint / format ───────────────────────────────────────────────────

# Format all sources in place via treefmt (Rust + Nix + TOML + Markdown).
[group('lint')]
fmt:
    treefmt

# Check formatting without writing via treefmt (mirrors the treefmt check under `nix flake check`).
[group('lint')]
fmt-check:
    treefmt --fail-on-change

# Regenerate the published JSON schemas from the serde types.
[group('lint')]
schema:
    UPDATE_SCHEMA=1 cargo test -p felis-client-core --features schema config_schema
    UPDATE_SCHEMA=1 cargo test -p felis-cli --features schema cli_schema
    UPDATE_SCHEMA=1 cargo test -p felis-grid --features schema json_v1::schema

# Regenerate the golden `felis bridge` conversations from a live bridge,
# and the golden felis-json v1 frames.
[group('lint')]
golden:
    UPDATE_GOLDEN=1 cargo nextest run -p felis-cli --test cli_bridge golden
    UPDATE_GOLDEN=1 cargo nextest run -p felis-grid --features json --test json_v1

# Regenerate IPC wire types (crates/felis-protocol/src/generated) via buf.
# The buf-generate pre-commit hook runs this and fails on a stale committed copy.
[group('lint')]
proto:
    cd crates/felis-protocol && buf lint
    cd crates/felis-protocol && buf generate

# Wire-compatibility gate against base (defaults to merge-base with origin/main).
[group('lint')]
proto-compat base="":
    python3 tools/proto/compat.py {{ base }}

# Self-test the wire-compatibility gate on a throwaway repo.
[group('lint')]
proto-compat-test:
    python3 tools/proto/compat.py --self-test

# Mechanical prose norms on the added lines of a range (the pre-commit hook
# runs the same script over the staged diff).
[group('lint')]
prose-check range="origin/main...HEAD":
    python3 tools/prose_check.py --range {{ range }}

# Exercise every prose rule against its fixture.
[group('lint')]
prose-check-selftest:
    python3 tools/prose_check.py --self-test

# Mechanical prose norms across all files.
[group('lint')]
prose-check-all:
    python3 tools/prose_check.py --all

# Validate SKILL.md YAML frontmatter and required metadata fields.
[group('lint')]
skill-check:
    python3 tools/skill_check.py

# Clippy with warnings denied (pr.yml's build job runs this).
[group('lint')]
lint:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# Lint felis-client-core's portable core on wasm32 (pr.yml's build job runs this).
# Clippy over check: -D warnings also catches the dead cfg leftovers a
# feature split leaves behind, which `cargo check` accepts silently.
[group('lint')]
check-portable:
    cargo clippy -p felis-client-core --no-default-features --target wasm32-unknown-unknown -- -D warnings

# License / ban / advisory audit (pr.yml's build job runs this).
[group('lint')]
deny:
    cargo deny check

# Source-based unused-dependency scan.
[group('lint')]
machete:
    cargo machete

# Check local Markdown links without depending on network availability.
[group('lint')]
docs-links:
    lychee --offline --no-progress 'docs/**/*.md'

# Hermetic self-tests of every Python tool under tools/ (pr.yml's build job runs this).
[group('lint')]
selftest: prose-check-selftest proto-compat-test release-check-test release-mirror-test bench-selftest bench-vs-selftest

# Run full check suite: fmt-check + lint + portable core + prose + skills + test + docs links + deny + proto-compat + tool self-tests.
[group('lint')]
check: fmt-check lint check-portable prose-check skill-check test docs-links deny proto-compat selftest

# ── Release ────────────────────────────────────────────────────────

# Assert a release tag's identity before pushing it (mirrors release.yml's verify-tag).
[group('release')]
release-check tag:
    python3 tools/release/verify.py {{ tag }}

# Self-test the release identity gate on a throwaway repo.
[group('release')]
release-check-test:
    python3 tools/release/verify.py --self-test

# Self-test the release page publisher against a local fake forge.
[group('release')]
release-publish-test:
    python3 tools/release/publish.py --self-test

# Self-test the GitHub release mirror against a local fake Forgejo and GitHub.
[group('release')]
release-mirror-test:
    python3 tools/release/mirror.py --self-test

# ── Bench (mirrors bench.yml; tools/bench/criterion.py is the entry point) ──
# Runs through orchestrator rather than bare cargo bench so flags match CI baselines.

# Run the headline end-to-end throughput benchmark.
[group('bench')]
bench *args:
    python3 tools/bench/criterion.py run end_to_end_throughput {{ args }}

# Run every Criterion bench target in the workspace (auto-discovered).
[group('bench')]
bench-all *args:
    python3 tools/bench/criterion.py run {{ args }}

# Markdown summary of the Criterion results already on disk.
[group('bench')]
bench-report:
    python3 tools/bench/criterion.py report

# Fail any past-threshold regression vs a saved baseline (docs/reference/testing.md "Running benchmarks").
[group('bench')]
bench-gate:
    python3 tools/bench/criterion.py gate

# Self-tests for the bench orchestrator (hermetic, no cargo).
[group('bench')]
bench-selftest:
    python3 tools/bench/criterion_test.py

# ── Cross-terminal comparison (not a CI gate) ───────────────────────
# Note: latency suite drives active window; leave keyboard untouched while running.

# Run the cross-terminal suites and render the charts (~1-1.5 h).
[group('bench')]
bench-vs *suites:
    nix develop .#bench -c python3 tools/bench/crossterm.py run {{ suites }}

# Re-render report.md and its charts from an existing results root (no measuring).
[group('bench')]
bench-vs-report dir:
    nix develop .#bench -c python3 tools/bench/crossterm.py report {{ dir }}

# Self-tests for the cross-terminal orchestrator (hermetic, no windows).
[group('bench')]
bench-vs-selftest:
    python3 tools/bench/crossterm_test.py

# Fetch ghostty's daily tip pre-release for field benchmarking.
[group('bench')]
bench-vs-fetch-tip *args:
    python3 tools/bench/fetch_ghostty_tip.py {{ args }}

# Preflight test each terminal's pinned launch arguments and grid size.
[group('bench')]
bench-vs-check-field *args:
    nix develop .#bench -c python3 tools/bench/check_field.py {{ args }}

# ── Fuzz (mirrors fuzz.yml; Linux/macOS, needs the flake nightly) ───

# Enumerate the cargo-fuzz targets.
[group('fuzz')]
fuzz-list:
    cd fuzz && cargo fuzz list

# Smoke every target (-runs=10000) — mirrors fuzz.yml per-PR job.
[group('fuzz')]
fuzz-smoke: _fuzz-seed
    #!/usr/bin/env bash
    set -euo pipefail
    cd fuzz
    for target in $(cargo fuzz list); do
        echo "::: fuzzing $target"
        cargo fuzz run "$target" -- -runs=10000
    done

# Long-run + cmin every target — mirrors fuzz.yml nightly (10 min/target).
[group('fuzz')]
fuzz-long secs='600': _fuzz-seed && fuzz-cmin
    #!/usr/bin/env bash
    set -euo pipefail
    cd fuzz
    for target in $(cargo fuzz list); do
        echo "::: long-run $target ({{ secs }}s)"
        cargo fuzz run "$target" -- -max_total_time={{ secs }}
    done

# Minify every target's corpus without re-running.
[group('fuzz')]
fuzz-cmin:
    #!/usr/bin/env bash
    set -euo pipefail
    cd fuzz
    for target in $(cargo fuzz list); do
        echo "::: cmin $target"
        cargo fuzz cmin "$target"
    done

# Run one fuzz target; extra libFuzzer args go after `--`.
[group('fuzz')]
fuzz target *args: _fuzz-seed
    cd fuzz && cargo fuzz run {{ target }} {{ args }}

_fuzz-seed:
    ./fuzz/seed-corpus.sh

# ── Kani model checking (x86_64-linux only; mirrors kani.yml) ───────

# Model-check the vt + protocol + grid + client-core kernel proofs (docs/reference/testing.md).
# --no-default-features keeps felis-client-core's `native` feature (tokio, felis-transport) out of the
# goto model; the other three packages have no default features to lose.
[group('kani')]
kani *args:
    nix develop .#kani -c cargo kani -p felis-vt -p felis-protocol -p felis-grid -p felis-client-core --no-default-features {{ args }}

# ── Mutation testing (audits whether the suite bites) ───────────────

mutants_jobs := env('CARGO_MUTANTS_JOBS', '4')

# Mutation-test grid/vt/protocol (`-F` narrows; PR gate: mutants-pr; full sweep: mutants-shard).
[group('mutants')]
mutants *args:
    CARGO_MUTANTS_JOBS={{ mutants_jobs }} cargo mutants {{ args }}

# Mutation-test only the mutants touched by the branch diff (fast pre-push gate).
[group('mutants')]
mutants-pr base="origin/main":
    #!/usr/bin/env bash
    set -euo pipefail
    diff=$(mktemp)
    trap 'rm -f "$diff"' EXIT
    git diff {{ base }}...HEAD > "$diff"
    CARGO_MUTANTS_JOBS={{ mutants_jobs }} cargo mutants --in-diff "$diff"

# Mutation-test one shard of the full sweep; run every shard for a full result.
[group('mutants')]
mutants-shard shard="0/8":
    CARGO_MUTANTS_JOBS={{ mutants_jobs }} cargo mutants --shard {{ shard }}

# ── Insta snapshots ─────────────────────────────────────────────────

# Re-run the whole suite under cargo-insta to stage `.snap.new` files.
[group('snapshots')]
snapshot-test:
    cargo insta test --test-runner nextest --disable-nextest-doctest \
        --workspace --all-features

# Interactive review of the staged snapshots (TUI).
[group('snapshots')]
snapshot-review:
    cargo insta review

# Accept all staged snapshots without review.
[group('snapshots')]
snapshot-accept:
    cargo insta accept
