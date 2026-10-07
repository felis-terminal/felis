# felis: collaborator notes

felis is a GPU-accelerated terminal emulator (Rust + wgpu + winit) with a daemon/client split, full Kitty protocol
coverage, and **no in-terminal multiplexer**. State lives in the daemon; closing the window does not kill the shell. The
Cargo workspace under `crates/` has landed the full original milestone plan; open work is tracked in Forgejo issues
(what shipped is recorded by `git log` and the design docs, never by a progress tracker).

## Reading order before editing anything

The design docs are the source of truth for every feature decision. Before proposing or writing code, read the relevant
docs first. `docs/README.md` is the doc map: it defines the Diátaxis quadrants and when reference and explanation split.

| When you are about to…                       | Read                                                                                                                                                                                                            |
| -------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Touch any feature scope                      | `docs/explanation/design.md` (the values), `docs/explanation/principles.md`, `docs/explanation/non-goals.md`                                                                                                    |
| Add or change a requirement                  | `docs/reference/spec.md` (every REQ-XXX is sourced)                                                                                                                                                             |
| Understand what a feature does _now_         | the reference page (`docs/reference/…`) — wire formats, config keys, status tables                                                                                                                              |
| Argue _why_ something was chosen or rejected | the owning explanation doc under `docs/explanation/` — each carries the rationale, rejected alternatives, and "Revisit if …" triggers inline                                                                    |
| Add a protocol behavior                      | `docs/reference/protocols/<name>.md` + `docs/explanation/protocols/<name>.md`; the verdict is `docs/reference/protocols/support-matrix.md`, and `docs/explanation/protocols/landscape.md` carries the rationale |
| Plan implementation work                     | Forgejo issues and `docs/explanation/implementation.md`                                                                                                                                                         |
| Write a new doc                              | `docs/README.md` first to find which quadrant it belongs to                                                                                                                                                     |

The principles' **Test:** clauses (`docs/explanation/principles.md`) are the rejection criteria: quote them when arguing
scope.

For any implementation task, the `implement-feature` skill is the front door: it encodes this reading order plus crate
placement and the verification gates, and routes to the specialized skills by task shape (scope, tests, docs, config,
IPC, escape sequences).

## Project shape

Virtual Cargo workspace of `felis-*` crates under `crates/`; the crate map and the one-way dependency direction are
`docs/reference/workspace.md` ("Crate map", "Dependency direction"). The seams are extraction points (a future repo
split must not need re-shuffling), so a test that reaches above its own crate root does not belong in a crate: the
workspace-wide source-reference guard lives in the `tests/` member; Markdown links and dependency bans use repository
tooling instead.

The two front-door binaries: `felis` (felis-cli) is the light, GPU-free entry point. Headless verbs run in-process;
window launches exec the GUI client (`felis-client`); `docs/reference/cli.md` owns which verb is which. The
autospawn-the-daemon policy lives in `felis-client-core` (`connect_or_spawn_daemon`), daemon-free, so both binaries
share it without dragging the backend into the client extraction set.

Hard rule: `felis-protocol` must not depend on `tokio` or anything OS-specific: it is the cross-language reuse surface.

## Dev environment

`CONTRIBUTING.md` "Development environment" is the contract for the toolchain, the pre-commit harness and `just check`;
no doc may add `rust-toolchain.toml`, `cargo install …`, `.editorconfig`, or ad-hoc tool-version pins. The
`implement-feature` skill carries the agent-facing details (which shells exist, what the dev shell puts on `PATH`, the
Windows caveats).

## Workspace policy

`docs/reference/workspace.md` governs the layout (the crate-boundary decision record lives in
`docs/explanation/architecture/overview.md`). Lint, edition, MSRV and the `unsafe_code` policy are defined by
`Cargo.toml` and `clippy.toml`, never by a doc: name the file, not the value. The audited `unsafe` relaxations are the
`#[allow(unsafe_code)]` sites (grep for them), each with a `// SAFETY:` comment; those comments are the record; consult
them before touching `unsafe`. What CI runs is `docs/reference/testing.md` "CI shape"; the dev shell provides the same
checks but does not auto-run them on commit.

## Commit conventions

`CONTRIBUTING.md` "Commit conventions" is the contract. The part agents get wrong: the scope is an area, never a change
type.

## Recording design decisions & editing docs

A design decision is recorded inline in the owning explanation doc, and most decisions earn no record at all: a why that
fails doc-cascade's "Default to no record" gate lives in the commit body. History belongs to `git log` (plus a
`CHANGELOG.md` entry for a user-affecting change), never to the docs. For lint/toolchain/dev-env decisions the record is
the Nix/Cargo config itself (`flake.nix`, `dev/`, `Cargo.toml`, `clippy.toml`) and its comments.

A change to one doc usually cascades into several across quadrants and the non-doc mirrors. The full procedure (quadrant
choice, the twin rule, decision-recording requirements, and the grep sweep to run before declaring done) is the
`doc-cascade` skill; use it for any change under `docs/`.

## Things to _not_ do

- **Don't add scripting.** Principle 1 forbids any embedded evaluator. The extension surface is IPC. Action variants may
  carry typed data (e.g. `SendString { text }`), never expressions or callbacks.
- **Don't add tabs/splits/panes.** Principle 1. The WM owns layout.
- **Don't add Sixel.** Permanent reject: Kitty does not implement it either, and Kitty graphics covers the same ground.
- **Don't widen scope to please an unstated user.** Principle 1: add only what earns its place. We want users who
  already know they don't want feature X, not users who might want feature X some day.
- **Don't introduce heuristics on shell content** (URL detection, content-type sniffing, smart quoting). Principle 4.

## Workflow expectations

`CONTRIBUTING.md` "Workflow" is the contract. The rules an agent must hold without opening it:

- `main` is protected; direct pushes (force-pushes included) are disabled. All changes go through feature branches and
  pull requests; a feature branch may be rebased, squashed, and pushed with `--force-with-lease` freely.
- The repo uses **Forgejo Actions** for CI, not GitHub Actions. Be careful when touching workflow files.
- Agent worktrees go under `.claude/worktrees/` inside the repo (it is git-excluded); a worktree outside the repo makes
  every cargo/git call prompt for permission.

## Skills

Project skills live in `.agents/skills/`; Claude Code reads the same tree through the `.claude/skills` symlink, so don't
maintain a list here. When a task yields reusable procedural knowledge (a workflow, environment gotchas, a tool recipe
worth repeating), capture it as a new skill (SKILL.md per the agentskills.io spec, plus any helper scripts) instead of
expanding this file. Keep `AGENTS.md` thin: this file holds the rules and the map; the skills hold the procedures.

Two maintenance rules: `skills/felis` is the **product-shipped** skill (symlinked into `.agents/skills/`); any CLI/IPC
surface change must cascade into it, or users receive a stale skill. And skills go stale like docs do: when a workflow
they encode changes (a justfile recipe, a benchmark harness, a doc path), update the skill in the same change (commit
scope: `skills:`).
