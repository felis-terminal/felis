---
name: doc-cascade
description:
  Edit the felis docs tree without leaving it inconsistent — pick the right Diátaxis quadrant, keep reference facts and
  explanation rationale in their lanes, record decisions inline (no ADR layer), and run the grep sweep before declaring
  done. Use for any change under docs/ (new page, requirement, decision record, protocol status flip, terminology
  change), or as the final step of a code change that touches documented behavior. One doc change normally cascades into
  several files across quadrants and the non-doc mirrors; this skill is the checklist that finds the others.
allowed-tools: Read Grep Edit Write
---

# Editing the felis docs

`docs/README.md` is the governing statement: quadrant definitions and when reference and explanation split;
`CONTRIBUTING.md` owns the commit rules, and §3 below owns the decision-record gate. This skill is the operational
checklist on top of them. Sentence-level norms are the `doc-prose` skill; use both when drafting a page.

## 1. The Diátaxis Quadrant Test

Route by reader activity per `docs/README.md`. The felis-specific exclusions are what the definitions there do not say:

- **Tutorials (`docs/tutorials/`)**: no theory, no alternative branches, no reference tables.
- **How-to guides (`docs/how-to/`)**: no foundational concept lessons, no comprehensive schema tables.
- **Reference (`docs/reference/`)**: no design rationale, no apologies.
- **Explanation (`docs/explanation/`)**: no retelling of reference facts, no status inventories.

A page that mixes quadrants gets split, not grown.

## 2. Boundary Invariants (The Five Traps)

Every doc failure in felis has come from crossing these five borders:

1. **Trap 1: Narrative Retelling (Explanation → Reference)**. When reference already carries a fact operationally (a
   table row, a knob list, a wire field), explanation keeps only the decision, its rejected alternatives, and its
   revisit trigger: never a prose re-tell of the same facts. An explanation section that argues what a reference table
   already states forces every future change to edit two places.
2. **Trap 2: Status Matrices in Explanation**. Checklists of feature status (such as ✅/🟡/⚪/❌ tables) belong in
   Reference (`docs/reference/protocols/support-matrix.md`). Explanation argues how and why a boundary is drawn, never
   the per-feature inventory. When an explanation page grows a feature matrix, compress it to the governing boundary
   decision and cite the reference table.
3. **Trap 3: The 1:1 Twin Myth**. Most config keys are self-evident: do not invent an explanation doc or dump config
   rationale into another page to fulfill a twin expectation. Explanation pages follow concerns and boundaries, not code
   or reference layout.
4. **Trap 4: Scope Rejections Outside `non-goals.md`**. Rejections of _scope_ go to `docs/explanation/non-goals.md`, not
   the owning explanation doc. Point to `non-goals.md` rather than writing isolated "What felis does not do" sections.
5. **Trap 5: Investigation Notes as Pages**. A survey that drove decisions (peer terminal catalogs, format scoring) does
   not get a doc of its own: compress its adopt/reject conclusions into the owning page, open a Forgejo issue for
   anything unshipped with its blocker, and delete the field notes. Catalogs rot silently once the decision exists.

## 3. Decision Recording ("Default to no record")

Every change carries a why, and the commit body is where that why lives. An explanation page records a decision only
when both hold: a competent contributor would plausibly re-propose the rejected alternative, and the reference facts
alone do not show why it fails. Field shapes, names, and any behavior a test pins fail that gate.

A _Revisit if_ trigger earns its line only when it names something observable from outside (a producer that appears, a
platform that changes), never "if this turns out to be wrong". Test: if the section argues each field of a struct or
each row of a table in turn, it is a commit body in the wrong file; keep the one decision that covers them.

## 4. History is not doc content

Every page describes the current design as if it had always been this way: no `previously`, `renamed from`, `no longer`,
`the old one`, no schema-version bump stamps, and no two-ways-where-one-is-the-old-way. The past has exactly two homes:
`git log`, and, for a user-affecting surface change (CLI verb, config key, keybinding, wire, default, platform), a
`CHANGELOG.md` entry. What does stay in an explanation page, argued in the present tense: rejected alternatives,
_Revisit if_ triggers, VT terms of art ("legacy xterm encoding"), third-party version facts, and recorded
post-publication policy.

## 5. Requirements

Every requirement is a numbered `REQ-XXX` in `docs/reference/spec.md`, each citing the doc that commits to it. Adding or
changing a requirement means updating spec.md **and** the committing doc in the same change.

## 6. The sweep (before declaring done)

```sh
grep -ri '<changed term>' docs/
```

Run it for every renamed concept, flipped status, moved file, and changed number. Run `just prose-check` in the same
pass: it catches the mechanical `doc-prose` rules (dashes, history narration, filler) on the lines you added. Then check
the non-doc mirrors:

- `skills/felis/SKILL.md` (shipped skill) for CLI/IPC facts;
- `.agents/skills/*/SKILL.md` for paths and commands you moved;
- `README.md` at the repo root, for the CLI surface it shows and the `--format` contract;
- `nix/hm-module.nix` docstrings and `nix/stylix.nix` for config sections and key names;
- `crates/felis-client-core/felis-config.schema.json` (regenerate with `just schema`, never hand-edit);
- `CHANGELOG.md`: not a rename target but an append surface. If the change you are documenting is user-affecting, the
  entry must exist (see §4, "History is not doc content").

## 7. One owner, outside `docs/` too

A fact has exactly one home, and the repo-root files are not it. `docs/` owns facts and decisions; `AGENTS.md`,
`CONTRIBUTING.md`, `README.md` and every `SKILL.md` cite the owning page instead of restating it, and a skill states
only procedure (commands, order of operations, environment gotchas). When a root file must name a value the config owns
(MSRV, edition, a lint), it names the file, not the value. Test: if a change to one number would edit prose in two
files, one of them should have been a citation. Cite by heading, never by section number; numbers drift. One exception
is `AGENTS.md`: it is the only file every agent session loads, so a rule an agent must hold before it opens anything
(protected `main`, Forgejo Actions, the worktree location) may stand there as a one-line restatement. The other is
`README.md`: it is the product front page, so it may summarize a topic in a sentence or two before linking its owner,
but it never carries a fact alone.

## 8. Page mechanics

- Every page carries Starlight frontmatter (`title:`, optionally `sidebar: order:`): the tree is published via the
  felis-docs repo. No `description:` key.
- A reference page opens with its subject, never with its audience ("A reference for contributors specifying …").
- A page ends where its content ends: no trailing `Status` / `See also` / `Cross-references` / `Guides` section whose
  links the body or `docs/README.md` already carries.
- The committed `*.schema.json` files (`felis-config.schema.json`, the CLI and felis-json schemas) are generated by
  `just schema`; regenerate them, never hand-edit. The prose around them, such as `docs/reference/config.md` "Editor
  support (JSON Schema)", is hand-written.
