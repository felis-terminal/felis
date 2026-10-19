---
title: Terminal progress design
sidebar:
  order: 7
---

This page records why felis keeps ConEmu's `OSC 9 ; 4` progress the way it does. The decoding rules, the lifetime, and
the IPC shape live in the [reference twin](../../reference/protocols/terminal-progress.md).

## A relay for agents, not a drawn indicator

The consumer is an agent or a script polling `felis sessions`, the pattern the shipped `felis` skill teaches: it reads
how far a build in another session has got without scraping that session's screen. systemd, zig and flatpak already send
the sequence whenever their output is a terminal, so the relay serves them without asking anything new of a program.

Drawing is a separate question with its own answer. Terminals draw progress in window chrome, a taskbar or dock
indicator, and winit exposes no portable API for one. felis therefore takes the posture it takes for notifications and
program status: decode, keep in the daemon, expose on IPC, and draw nothing.

## Its own field, not an OSC 7501 record

The program-status spec lets a terminal map `OSC 9 ; 4` onto the root record, and felis does not. The two vocabularies
do not line up: cargo, zig and pytest send `st=2` for "failed so far, still running", where a 7501 `error` is a program
that failed and stopped and survives the next prompt, and `st=4` (paused) has no 7501 state at all. A mapping would also
need a rule for which record a progress clear removes and a latch that stops it once a program reports 7501 itself. The
shipped 7501 implementations agree: iTerm2, kitty and libghostty keep progress and program status apart. So felis
exposes both side by side, each with its own meaning.

## Lifetimes come from events felis already sees

Other terminals expire a stale progress on a clock (15 s or 60 s with no new report). felis uses the events it already
parses instead: a primary `OSC 133 ; A` clears the progress, as do `st=0` and RIS. The prompt is what covers the two
reports a clear never follows, a producer killed mid-task and cargo ending a failed build on `2;100`. A timer would make
a slow step look finished, and the shell's prompt already says when the task is over.

The end of the PTY does not clear it. The field means live progress; the row's `exited` flag already says the program is
gone, and the report is harmless while a window keeps the exited session open. A finished task's outcome is not this
field's job: the shell's `OSC 133 ; D` exit code, which `felis sessions send --wait` returns, is.

## No value is invented

Terminals disagree on `st=2` or `st=4` without a value: some keep the previous value, some show `0`, and some ignore the
report. felis reports the state with no value rather than pick one of those readings, and no report inherits a field
from the one before it, so a consumer sees exactly what the program sent.

## The identity cargo looks for is not advertised

cargo enables the sequence only for terminals it recognizes, through environment variables such as `TERM_PROGRAM` or
iTerm2's `TERM_FEATURES` codes. felis claims no other terminal's identity, and `TERM_FEATURES` is no lighter: its codes
are present exactly when a feature is, so advertising one means maintaining felis's whole code set in iTerm2's
vocabulary, and its progress code promises a visible indicator felis does not draw. A cargo user opts in with
`term.progress.term-integration = true` instead.
