# Repository tools

The `justfile` is the public entry point for routine work. Call these tools directly only when iterating on the tool
itself or when a recipe mentions the direct form.

## Layout

- `tools/proto/compat.py` is the IPC wire-compatibility gate behind `just proto-compat` and the `proto-compat` CI job:
  `buf breaking` against the base the run selects, plus the pre-release acknowledgment path.
  `tools/proto/compat_test.py` (`just proto-compat-test`, `compat.py --self-test`) is its exit-code suite on a throwaway
  repo.
- `tools/release/verify.py` is the release identity gate behind `just release-check <tag>` and `release.yml`'s
  `verify-tag` job: the tag is annotated, points at the revision being published, agrees with the workspace version
  every crate inherits, and (for a final tag) has its CHANGELOG section, on a clean tree. `--identity` compares the
  built binary's `felis version --format json` with the tag, and `tools/release/verify_test.py`
  (`just release-check-test`, `verify.py --self-test`) is its exit-code suite on a throwaway repo.
- `tools/skill_check.py` validates required YAML frontmatter fields in every `SKILL.md`, behind `just skill-check` and
  its pre-commit hook.
- `tools/prose_check.py` is the mechanical half of the felis prose norms, behind `just prose-check` and the
  `prose-check` pre-commit hook: dashes, history narration, and filler phrases on the added lines of a diff.
  `--self-test` is its fixture suite.
- `tools/unicode/gen_tables.py` generates the Unicode property tables unicode-width does not expose
  (`crates/felis-grid/src/uax29/tables.rs`, `crates/felis-shaping/src/presentation/tables.rs`) from the UCD the dev
  shell pins, behind `just unicode-tables` and its pre-commit hook. A Unicode bump changes that pin and unicode-width
  together; the cross-checks in the crates' tests catch a skew.
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
