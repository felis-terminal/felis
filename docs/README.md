---
title: "felis: documentation"
---

The tree follows [Diátaxis](https://diataxis.fr/): four quadrants, split by what the reader is doing. **Learning** →
tutorials. **Doing a task** → how-to guides. **Looking something up** → reference. **Understanding why** → explanation.
The explanation pages are the design docs: each owns its decisions (rationale, rejected alternatives, "Revisit if …"
triggers) inline; history lives in `git log` and the repo-root `CHANGELOG.md` (user-affecting changes), not here.

Where a subject spans both normative facts and architectural rationale (IPC, the protocols, testing, …), the pages link
each other: the reference page carries the normative lookup material, the explanation page carries the argument.
Explanation does not mirror every reference topic 1:1; it exists only where architectural decisions require
understanding why.

The whole `docs/` tree is published as the documentation site; the felis-docs repository builds it via a submodule of
this one. Every page carries Starlight frontmatter, and links that leave `docs/` are rewritten to repository URLs at
site build time.

## Tutorials

Learning-oriented, hands-on. Start here.

- [Your first session](tutorials/first-session.md) — open a window, close it while work keeps running, and reattach. The
  fastest way to meet felis's one core idea.
- [Drive a session without a window](tutorials/drive-without-a-window.md) — create, steer, and read a session entirely
  from the command line, then open a window last.

## How-to guides

Task-oriented; one goal per page.

- [Install](how-to/install.md) — the Nix flake package, the home-manager module (and its Stylix hook), the Linux, macOS
  and Windows release archives, a from-source build, and the terminfo entry.
- [Update felis](how-to/update-felis.md) — install a new version, then restart the daemon onto it; the restart ends the
  running sessions.
- [Do your tmux workflows without tmux](how-to/tmux-workflows-without-tmux.md) — the everyday tmux operations without a
  multiplexer: sessions via the daemon, layout via your WM.
- [Get notified when a job finishes](how-to/enable-notifications.md) — emit a notification from a long command and pop
  it on your desktop.
- [Attach a session over SSH](how-to/attach-over-ssh.md) — open and drive a remote machine's session that survives
  disconnects.
- [Search and capture scrollback](how-to/search-and-capture-scrollback.md) — grep your day's history, snapshot a session
  for a bug report, preview sessions with fzf.
- [Label sessions with tags](how-to/label-sessions-with-tags.md) — tag sessions by what they are for, then list, pick,
  or end them by tag.
- [Reap dead or stale sessions](how-to/reap-sessions.md) — find and destroy stale sessions by idle time or by tag.
- [Drive a session beside its window](how-to/drive-a-session-beside-its-window.md) — steer a session from a script or
  editor while a window shows it.
- [Fix terminfo and terminal identity problems](how-to/fix-terminfo-problems.md) — symptom-first fixes: missing terminfo
  entries and terminal-identity allowlist workarounds.
- [Fix keyboard input problems](how-to/fix-keyboard-input-problems.md) — symptom-first fixes for keys that type the
  wrong character: the JIS yen keycap, and where the modifier-combination fixes live.

## Reference

Information-oriented lookup; consulted, not read in order.

- [CLI](reference/cli.md) — every `felis` verb, the window-launch forms, the `bridge` subcommand, `--format` and the
  exit codes.
- [Configuration](reference/config.md) — config file paths per platform, the JSON schema, every key with its default and
  range, and the loader's behavior on missing / malformed values.
- [Keybindings and mouse](reference/keybindings.md) — every default chord per platform, mouse gestures, chord grammar,
  and the actions that ship unbound.
- [Post-process shaders](reference/shaders.md) — what `shader.post` can name, the `fs_post` entry point and bind layout,
  and every field of the uniform contract.
- [IPC wire specification](reference/ipc.md) — stream / frame / application layers, message families and kind numbers,
  what the row payload carries, scrollback capture and search replies, the frozen preface, correlation and stream
  lifecycle, versioning.
- [Row codec](reference/row-codec.md) — the normative, language-neutral spec of the `packed_cells` bytes: layout,
  integer widths, limits enforced before allocation, and the golden vectors every implementation must reproduce.
- [Control-surface name map](reference/control-surfaces.md) — one table mapping each operation across its planes:
  concept, CLI verb, keymap `kind`, `IpcAction`, wire message.
- [Workspace](reference/workspace.md) — the canonical crate map, dependency direction, filesystem layout, build and
  platform matrix, and where the lint policy lives.
- [Testing](reference/testing.md) — the test-layer table, per-layer targets, fuzz targets, the Kani proof inventory,
  mutation testing, and the CI shape.
- [Security audits](reference/security-audits.md) — the standing audits: `O_CLOEXEC`+`O_NOFOLLOW` open sites, the OS
  hand-off sites, and `felis-protocol` crate purity.
- [Spec summary](reference/spec.md) — numbered (REQ-XXX) index of every requirement, citing the doc that commits to it.
- [Glossary](reference/glossary.md) — terms used across the docs.
- [Terminal identity](reference/terminal-identity.md) — `TERM=xterm-felis`, the verified-capabilities terminfo entry,
  and the `FELIS_TERM` / `FELIS_TERM_PROGRAM` escape hatch.
- [esctest compatibility](reference/esctest-compatibility.md) — the PTY-driven `esctest2` ratchet (`PASS_BASELINE`) and
  the deferred- and rejected-cluster lists accounting for every remaining failure.
- [Benchmarks](reference/benchmarks.md) — cross-terminal comparison harness, suite specifications, and published results
  on macOS and Linux against kitty, alacritty, wezterm, ghostty and foot.

Protocols:

- [Protocol support matrix](reference/protocols/support-matrix.md) — at-a-glance status across every escape-sequence
  family and extension protocol.
- [VT / ANSI compliance](reference/protocols/vt-compliance.md) — per-sequence behavior and the exact reporting / query
  replies (status lives in the support matrix).
- [Kitty graphics](reference/protocols/kitty-graphics.md) — wire format, transmission methods, placement, animation,
  limits.
- [Kitty text sizing](reference/protocols/kitty-text-sizing.md) — the OSC 66 wire format with its metadata keys, and the
  limits; the behavior status is in the support matrix.
- [Notifications](reference/protocols/notifications.md) — OSC 9 / 777 / 99 wire grammars and the conformance subset.
- [Key encoding](reference/protocols/key-encoding.md) — the PTY-bound byte encodings: Kitty keyboard protocol, legacy
  fallback, mouse reporting, bracketed paste.

## Explanation

Understanding-oriented: the design record. Reading order, top to bottom.

- [Vision](explanation/vision.md) — what felis is and is not; the foundation document every other doc builds on.
- [Design values](explanation/design.md) — the values felis pursues, stated positively; the worldview the principles
  enforce.
- [Principles](explanation/principles.md) — non-negotiable rules, each with its rejection **Test:** clause.
- [Non-goals](explanation/non-goals.md) — explicitly out of scope.
- [Comparison](explanation/comparison.md) — felis vs. other terminals: per-terminal architectural sketches, what felis
  takes from each, and what it refuses.
- [Feature baseline](explanation/feature-baseline.md) — which generation of terminal protocol felis implements in full,
  and the latency, throughput, and diagnostics budgets that go with it.
- [Security model](explanation/security-model.md) — threat model, trust boundaries, and per-area rules.
- [Input](explanation/input.md) — the input layers, keybinding design, the `pipe` action's carrier and transient
  session, "why pipe, not a copy mode", mouse and selection, the clipboard gates, and the bars that share the bottom
  chrome row.

Architecture:

- [Overview](explanation/architecture/overview.md) — process model, lifecycle, the daemon / client split responsibility
  table, prior art, and the crate-boundary decision record.
- [Session lifecycle](explanation/architecture/session-lifecycle.md) — attach, detach, rehydrate, destroy.
- [Control surfaces](explanation/architecture/control-surfaces.md) — the five surfaces, placement criterion for new
  knobs, and asymmetries.
- [IPC design](explanation/architecture/ipc.md) — goals, connection modes rather than per-feature bits, why protobuf and
  the rejected encodings, correlation and the corrupt-frame rule, schema evolution and the frozen surfaces, what is
  deliberately not in the protocol, extensibility.
- [Terminal identity](explanation/architecture/terminal-identity.md) — why felis identifies honestly and impersonation
  is opt-in.

Data model:

- [Grid and cells](explanation/data-model/grid-and-cells.md) — cell representation, primary/alternate screen models, and
  style handle interning.
- [Scrollback](explanation/data-model/scrollback.md) — flat cell ring buffer, zero-copy eviction, and reflow on resize.
- [Image store](explanation/data-model/image-store.md) — daemon-side Kitty graphics storage, refcounting, and memory
  bounds.

Rendering:

- [Render pipeline](explanation/rendering/pipeline.md) — client GPU pipeline stages, shadow screen snapshotting, and
  wgpu frame presentation.
- [Text shaping](explanation/rendering/text-shaping.md) — client-side swash shaping, run grouping, font fallback chains,
  and glyph caching.
- [Damage tracking](explanation/rendering/damage-tracking.md) — row-granularity bitset tracking that decides what the
  daemon ships and which rows the client rebuilds.

Protocols:

- [Protocol admission decisions](explanation/protocols/landscape.md) — why the escape-sequence families that took an
  argued decision took it, and what would reverse it.
- [VT / ANSI compliance](explanation/protocols/vt-compliance.md) — conscious omissions and how the compliance set is
  versioned.
- [Kitty graphics design](explanation/protocols/kitty-graphics.md) — dispatcher architecture, the rejected transfer and
  playback shapes, and eviction under the byte cap.
- [Kitty text sizing design](explanation/protocols/kitty-text-sizing.md) — daemon vs. client responsibilities and
  interactions.
- [Notifications design](explanation/protocols/notifications.md) — why felis relays instead of acting.

Engineering and verification:

- [Implementation style](explanation/implementation.md) — the crate-level choices (Tokio, wgpu, swash, the self-hosted
  VT and PTY layers), each with the alternative it rejects and the trigger that would reopen it.
- [Testing strategy](explanation/testing.md) — why the layers exist, why sampling is not enough, which peer conformance
  practices felis adopted and rejected, and what is deliberately not verified.
- [Benchmark-harness design](explanation/benchmarks.md) — why the cross-terminal comparison is a local report rather
  than a gate, why workloads and instruments are shaped the way they are, and how repetition ensures comparable results.
