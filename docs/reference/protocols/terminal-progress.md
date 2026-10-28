---
title: Terminal progress (OSC 9 ; 4)
sidebar:
  order: 8
---

A program reports how far a task has got with ConEmu's `OSC 9 ; 4` progress sequence; felis keeps the last report per
session and shows it in `felis sessions` and over IPC. It draws nothing: no taskbar, dock, or title-bar indicator. Why
felis takes the sequence this way is in the [design page](../../explanation/protocols/terminal-progress.md); the verdict
row is in the [support matrix](support-matrix.md#osc).

## Wire format

```
OSC 9 ; 4 ; <st> [ ; <pr> ] ST
```

BEL is accepted as the terminator alongside ST. A sequence that never terminates (cut off by CAN, SUB, or an ESC that
does not start ST) is ignored.

| `st` | Report                                                       |
| ---- | ------------------------------------------------------------ |
| `0`  | clears the session's progress                                |
| `1`  | `normal`                                                     |
| `2`  | `error`: the task has failed so far; it may still be running |
| `3`  | `indeterminate`                                              |
| `4`  | `paused`                                                     |

The fields decode in this order:

1. `st` is the field after `4`. When it is empty or not a single digit `0` to `4`, the sequence is ignored.
2. `st=0` clears, whatever follows it.
3. `st=3` reports `indeterminate` with no value, whatever follows it.
4. For `st=1`, `2` and `4`, a missing or empty `pr` reports no value, and a decimal integer reports `min(pr, 100)`.
   Anything else in `pr` ignores the sequence. Fields after `pr` are ignored.

Each report replaces the previous one whole: a report without a value does not keep the last one's.

Bare `OSC 9 ; 4`, with no field after the `4`, is the iTerm2 notification "4" ([notifications.md](notifications.md)).
The other ConEmu subcommands, `OSC 9 ; <digit> ; …`, are
[consumed without effect](vt-compliance.md#consumed-without-effect).

## Lifetime

A session holds at most one progress, the last report it received.

| Event                                                                         | Effect                           |
| ----------------------------------------------------------------------------- | -------------------------------- |
| A report with `st` other than `0`                                             | replaces the progress            |
| `st=0`                                                                        | clears the progress              |
| The shell draws a new prompt (`OSC 133 ; A`; a continuation, `k=s`, does not) | clears the progress              |
| A hard reset (RIS, `ESC c`)                                                   | clears the progress              |
| The session's program exits (the end of the PTY)                              | none; the row is marked `exited` |
| A soft reset (DECSTR), alternate-screen switches, detach, reattach, upgrade   | none                             |

The progress changes in the order the program wrote its reports, prompts, and resets. No timer expires it.

The progress is the last one a program reported, not proof that it is still running. Prompt clearing needs a shell that
marks its prompts ([Mark shell prompts](../../how-to/mark-shell-prompts.md)); without those marks, a report that a
program left behind (it was killed, or it ended on `error` without a clear) stays until the next report, a clear, RIS,
or the end of the session.

## Where progress surfaces

`felis sessions list --format json` and `felis sessions info` show it as `terminal_progress`, and the human `info` reads
`progress: <state>[ <value>%]` ([cli.md](../cli.md)). IPC carries it as `SessionInfo.terminal_progress`
(`crates/felis-protocol/proto/felis.proto`). It is absent when the session holds none, and otherwise is:

| Field   | Type                           | Meaning                                                         |
| ------- | ------------------------------ | --------------------------------------------------------------- |
| `state` | string                         | an open vocabulary; a reader treats an unknown value as unknown |
| `value` | integer `0` to `100`, optional | absent when the report carried none                             |

No event announces a change: a consumer polls, as it does for [program status](program-status.md).

## Producers

cargo detects its terminal by identity, which felis does not claim, so it sends the sequence only when
`term.progress.term-integration = true` is set in its config (or `CARGO_TERM_PROGRESS_TERM_INTEGRATION=true` in the
environment).
