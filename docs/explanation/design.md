---
title: Design values
sidebar:
  order: 2
---

The [vision](vision.md) places felis among the tools in a working environment. These values explain why it owns some
responsibilities and delegates others, including the costs of those choices. The [principles](principles.md) turn the
values into enforceable rules; this page supplies the reasoning rather than repeating their tests.

## A drawn boundary

A dedicated tool can serve its responsibility across the whole working environment. A window manager arranges more than
terminal panes; a shell-level picker can serve more than terminal settings. Absorbing those jobs into felis would create
narrower substitutes and make the terminal another place to configure the same workflow.

The boundary therefore follows responsibility, not implementation size. A capability belongs inside felis only when no
dedicated tool does it better and a real consumer needs it. Otherwise the question is which existing layer owns it:
layout goes to the window manager, workflow to the shell or an external process. Content heuristics are different: they
are declined, not delegated, because guessing intent is itself the unwanted behavior.

This is not a feature budget. Declining a file picker does not buy permission to add an unrelated convenience; each
capability must justify its own placement. Nor does delegation mean the project cannot provide useful companion tools.
They belong outside the terminal when the responsibility does, regardless of who writes them.

Platform duties can require different implementations of the same responsibility. Window backdrop effects belong to the
compositor on Linux but require application work on macOS and Windows. Such shims follow the OS boundary rather than
assigning a new workflow to the terminal; the precise exceptions live in
[non-goals](non-goals.md#cross-platform-constraints).

## Maximalism inside, strictness at the edge

A narrow responsibility does not call for a reduced implementation of it. Once felis accepts terminal rendering as its
job, protocol fidelity and speed both belong inside that job. A missing rendering capability cannot be excused by
calling the terminal minimal.

This is fidelity-maximalism, not feature-maximalism. The target is the modern protocol set used by real producers, not
new output protocols invented to expand the terminal's role. Kitty provides the fidelity benchmark for the protocols in
scope; the [feature baseline](feature-baseline.md) explains that boundary and the
[support matrix](../reference/protocols/support-matrix.md) records the implementation surface.

## Composition across a process boundary

Extensions need access to terminal state, not residence inside the terminal process. A public, versioned IPC lets
external programs act on that state using their own languages and libraries. The same IPC connects felis's client and
daemon, so external integrations do not depend on a separate privileged plugin API.

An embedded evaluator would make the terminal responsible for a runtime, its dependencies, and the behavior of code
loaded into it. Separate processes instead give extensions an independent lifetime and failure boundary. That is not a
claim that IPC clients are harmless: they can exercise the authority the interface grants them. The trust model is
described in [security model](security-model.md).

The same separation applies to small utilities maintained alongside felis. A picker can select sessions through the
public interface without making its selection policy part of the daemon. A different picker can use that interface
instead. Keeping workflow outside is what makes such tools composable; merely splitting a mandatory integrated workflow
into several executables would not achieve it.

A process boundary costs latency and setup compared with an in-process callback. felis accepts that cost to keep
extension logic independent of the terminal's implementation and to avoid committing users to an embedded language. The
[IPC design](architecture/ipc.md) owns the wire-level trade-offs.

## Mechanism, not policy

Persistent state must remain usable when presentation changes. The daemon therefore stores cells, attributes, image
bytes, scrollback, and protocol-level objects, while the client chooses fonts, colors, and shaping. Reattaching with a
different client configuration should not require restarting the process that owns the shell.

Some state lies on the boundary: a cursor shape requested by an application is not the same as a user's cursor
preference. The distinction is whether the value describes the running session or how a particular client displays it.
Keeping that distinction lets clients with different presentation settings attach to the same state; the
[architecture overview](architecture/overview.md) assigns the concrete responsibilities.

## Correct by construction, not by replay

Reconstructing a terminal by replaying synthetic escape sequences asks a second emulator to interpret a representation
of the first emulator's state. Correctness then depends on both agreeing about every represented behavior. Keeping only
a detached byte stream avoids that particular reconstruction step but does not preserve a terminal grid and scrollback
for a new display.

felis instead keeps the PTY and terminal state in one daemon and transfers that state on attach. The client renders the
state itself rather than interpreting a replay of how it was produced. This removes the replay mismatch; it does not
remove the need for a correct state-transfer protocol and renderer.

The daemon and its protocol are consequently heavier than a byte-stream session holder. That cost buys persistence of
the terminal's actual state, including modern protocol objects, without placing a multiplexer in the rendering path. The
[session lifecycle](architecture/session-lifecycle.md) describes attachment and its invariants.

## Algorithms, not languages

Performance work starts with the data structures and pipelines that consume time. VT parsing and text shaping are the
primary places to investigate: dispatch, parameter parsing, shaping caches, and the data passed to the renderer. A
language choice or GPU API is not evidence that a particular workload is fast.

Rust earns its place through memory safety and implementation ergonomics, but those properties do not replace a profile.
The point is not that implementation languages have identical performance; it is that an optimization needs to identify
the actual cost it removes. The [implementation guide](implementation.md) explains the technology choices.

## Explicit, never heuristic

A terminal protocol supplies instructions; ordinary output supplies content. Keeping those roles separate makes behavior
predictable: an OSC 8 sequence creates a hyperlink because the producer requested one, while URL-shaped text remains
text. Scanning output for inferred intent would make behavior depend on patterns the producer never agreed to.

Explicitness has a usability cost. Producers must emit the relevant sequences, and users must configure behavior that
another terminal might guess. felis accepts that friction rather than making the same bytes behave differently as
content heuristics evolve. The [principles](principles.md#4-explicit-over-heuristic) make this distinction a rejection
test, and the [non-goals](non-goals.md) identify the behaviors it excludes.
