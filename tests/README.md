# Workspace tests

Guards on invariants that belong to the workspace as a whole rather than to any one crate. They live outside `crates/`
because `crates/*` is the extraction set — each crate there must stay separable, and a check that walks the workspace
root would not survive the split ([workspace.md](../docs/reference/workspace.md)).

| Test                       | Pins                                                                                            |
| -------------------------- | ----------------------------------------------------------------------------------------------- |
| `doc_source_references.rs` | every bare `crates/.../*.rs` citation under `docs/` resolves                                    |
| `protocol_ledger.rs`       | the minor ledger in `docs/reference/ipc.md` covers felis-protocol's metadata, one row per minor |

Markdown links are checked by the repository's `lychee` lint. Dependency bans are checked by `just deny`.

A test belongs here only if it asserts something about the workspace itself. Tests that exercise one crate against
another — the daemon against the PTY layer, the client against the daemon — belong in the `tests/` directory of the
crate that owns the seam, where the dependency is already declared.
