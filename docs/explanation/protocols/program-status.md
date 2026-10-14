---
title: Program status design
sidebar:
  order: 6
---

This page records why felis keeps OSC 7501 program-status records the way it does. The wire grammar, the record
lifetimes, and the IPC shape live in the [reference twin](../../reference/protocols/program-status.md).

A program that waits on its user (a TUI agent asking for permission, a build blocked on a login) has no reliable way to
say so through the byte stream felis already reads: OSC 133 `D` marks a finished shell command, and a full-screen TUI
never emits one. OSC 7501 is a producer's own statement of what it is doing, so it is the signal felis can relay without
guessing from screen content (Principle 4).

## Relay, like notifications

felis takes the posture it takes for desktop notifications ([notifications.md](notifications.md)): decode, keep, expose
on IPC, and draw nothing. A status badge, a tab-bar dot, or a flash on `blocked` are the consumer's to build; felis has
no tab bar (Principle 1), and the window already flashes for the producer signal that is meant to interrupt, a
notification. A program that wants the user's attention when it blocks sends both.

The records live in the daemon, not the window, for the reason every session fact does: a record must outlive the window
that was open when it arrived. A `done` that lands while nobody is attached is the case the protocol exists for.

## Experimental, without a looser contract

"Experimental" names the spec revision implemented and nothing more. The IPC and CLI surfaces carry the same
compatibility promise as every other field, so the shape is built to absorb the two moves an experimental protocol
makes:

- **Following a revision.** `state` and `kind` are open strings on IPC rather than enums, and readers treat an unknown
  value as unknown. A later revision's state reaches consumers by changing the decoder's table alone, with no schema
  change. The feature-detection reply follows the same table: it is bare because the decoder accepts exactly the base
  vocabulary, and it lists `states=` the day the decoder accepts more.
- **Withdrawing support.** Stop answering `?`, stop decoding, and `program_status` stays empty. No field has to be
  removed.

Unknown report keys are dropped rather than passed through to consumers. Passing them through would make the public
shape whatever producers invent, outside any spec revision. _Revisit if_ the spec defines an extension namespace.

## Lifetimes come from events felis already sees

The spec ties a record's life to the prompt and to process exit, not to a heartbeat, and felis maps those to events it
already parses: a primary `OSC 133 ; A` (a continuation prompt, `k=s`, is not a new prompt) and the end of the PTY. The
PTY end is taken after the child's last bytes are parsed, so the `done` a program writes right before exiting survives
the expiry it would otherwise race. Child exit observed some other way does not expire records: a background job can
still hold the PTY and be the program reporting.

`idle` survives both events, though the spec allows dropping it: an idle record at a prompt is still true, and keeping
it is the conservative reading. Whether the user has seen a `done` or `error` is the consumer's judgment; felis keeps
both until they are replaced or cleared.

## Raw values on IPC

`app` is shipped as the record's own value. The spec has a record without `app` take its nearest ancestor's, but the
consumer has every record and the tree, so filling it in daemon-side would only hide which record actually said it.
`title` and `msg` likewise ship as decoded, direction controls included: disarming them is the job of whoever draws
them, and the right disarm depends on the surface. felis's own human-readable output disarms its copy.
