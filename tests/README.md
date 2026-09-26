# Workspace tests

Guards on invariants that belong to the workspace as a whole rather than to any one crate. They live outside `crates/`
because `crates/*` is the extraction set — each crate there must stay separable, and a check that walks the workspace
root would not survive the split ([workspace.md](../docs/reference/workspace.md)).

| Test                       | Pins                                                         |
| -------------------------- | ------------------------------------------------------------ |
| `doc_source_references.rs` | every bare `crates/.../*.rs` citation under `docs/` resolves |

Markdown links are checked by the repository's `lychee` lint. Dependency bans are checked by `cargo-deny` and by
per-crate purity tests (`crates/felis-protocol/tests/crate_purity.rs`, `crates/felis-grid/tests/no_render_deps.rs`) that
inspect only their own crate's graph.

A test belongs here only if it asserts something about the workspace itself. Tests that exercise one crate against
another — the daemon against the PTY layer, the client against the daemon — belong in the `tests/` directory of the
crate that owns the seam, where the dependency is already declared.
