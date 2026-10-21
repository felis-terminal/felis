# Repository tools

The `justfile` is the public entry point for routine work. Call these tools directly only when iterating on the tool
itself or when a recipe mentions the direct form.

## Layout

Each tool's tests sit beside it as `<tool>_test.py`; `just test-tools` runs them all under pytest.

- `tools/proto/compat.py` is the IPC wire-compatibility gate behind `just proto-compat` and the `proto-compat` CI job:
  `buf breaking` against the base the run selects, plus the pre-release acknowledgment path.
  `tools/proto/compat_test.py` is its exit-code suite on a throwaway repo.
- `tools/proto/schema_meta.py` generates `crates/felis-protocol/src/schema_meta.rs` (the arm table and each addition's
  minor) from `felis.proto`'s options, behind `just proto` and the `schema-meta` pre-commit hook, and checks the
  schema's shape rules on the way.
- `tools/release/verify.py` is the release identity gate behind `just release-check <tag>` and `release.yml`'s
  `verify-tag` job: the tag is annotated, points at the revision being published, agrees with the workspace version the
  binary crates inherit, and (for a final tag) has its CHANGELOG section, on a clean tree. `--identity` compares the
  built binary's `felis version --format json` with the tag, and `tools/release/verify_test.py` is its exit-code suite
  on a throwaway repo.
- `tools/release/crates.py` is the crate half of the release: `check` (in `release.yml`'s `verify-tag` and
  `just release-check`) refuses a tree whose library versions are not ready to publish, and `publish-plan` lists, for
  `release-mirror.yml`'s `crates` job, the crates whose version is not on crates.io. `package-tests`
  (`just package-tests`, in `release.yml`'s build job) builds each crate's tests from its extracted package.
  `tools/release/crates_test.py` tests it on fixtures and a file-backed index.
- `tools/dep_bans.py` checks each crate's production dependency closure against the "MUST NOT depend on" column of
  [workspace.md](../docs/reference/workspace.md) "Crate map", behind `just deny` and the CI job that runs it;
  `tools/dep_bans_test.py` is its fixture suite.
- `tools/skill_check.py` validates required YAML frontmatter fields in every `SKILL.md`, behind `just skill-check` and
  its pre-commit hook.
- `tools/prose_check.py` is the mechanical half of the felis prose norms, behind `just prose-check` and the
  `prose-check` pre-commit hook: dashes, history narration, and filler phrases on the added lines of a diff.
  `tools/prose_check_test.py` is its fixture suite.
- `tools/unicode/gen_tables.py` generates the Unicode property tables unicode-width does not expose
  (`crates/felis-grid/src/uax29/tables.rs`, `crates/felis-shaping/src/presentation/tables.rs`) from the UCD the dev
  shell pins, behind `just unicode-tables` and its pre-commit hook. A Unicode bump changes that pin and unicode-width
  together; the cross-checks in the crates' tests catch a skew.
- `tools/shell_integration/ssh_test.py` tests the shipped `ssh` function (`share/felis/shell-integration/ssh.*`) against
  stub `ssh`, `infocmp`, and `tic` executables.
- `tools/bench/criterion.py` drives Criterion runs for `just bench*` and the performance CI gate.
- `tools/bench/crossterm.py` orchestrates cross-terminal measurements: it resolves the field, records provenance, runs
  suites, and writes the report.
- `tools/bench/suites.py` launches terminal windows and writes raw harness artifacts.
- `tools/bench/loaders.py` parses those raw artifacts into report data.
- `tools/bench/field.py` owns the pinned terminal launch condition.
- `tools/bench/report.py` renders Markdown and PNG output from loaded suites.
- `tools/bench/envinfo.py` records machine, display, power, and tool provenance.
- `tools/bench/check_field.py` verifies the pinned field before a long cross-terminal run.
- `tools/bench/payloads.py` generates deterministic cat-suite payloads.
- `tools/bench/fetch_ghostty_tip.py` fetches an explicit ghostty-tip binary for comparison runs.
- `dev/bench/devshell.nix`, `tools/bench/doom-fire-bench.patch`, and `tools/bench/typometer/Main.java` are inputs to the
  `.#bench` shell.

Other helper locations are intentionally separate: `fuzz/seed-corpus.sh` belongs to the cargo-fuzz workspace, and
packaging helpers belong under `nix/`.
