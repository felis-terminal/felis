---
title: Program status protocol (OSC 7501)
sidebar:
  order: 7
---

A program reports what it is doing (working, waiting on the user, done) with the OSC 7501 Program Status Protocol; felis
keeps the latest reports per session and shows them in `felis sessions` and over IPC. It draws nothing: no window flash,
no attention hint, no badge. Support is **experimental** and follows spec revision 0.4 (2026-10-10):
<https://www.superlogical.com/rex/docs/build/program-status>. For the tasks, see
[Find sessions waiting on you](../../how-to/find-sessions-waiting-on-you.md); why felis takes the protocol this way is
in the [design page](../../explanation/protocols/program-status.md); the verdict row is in the
[support matrix](support-matrix.md#osc).

## Wire format

```
OSC 7501 ; <key>=<value> ( : <key>=<value> )* ST
OSC 7501 ; ? ST
```

BEL is accepted as the terminator alongside ST.

The keys, states, kinds, and limits are the spec's:

| Key        | Accepted values                                                   | Otherwise                                                             |
| ---------- | ----------------------------------------------------------------- | --------------------------------------------------------------------- |
| `state`    | `idle`, `working`, `done`, `blocked`, `error`, `clear`            | absent or unknown: the report is discarded                            |
| `id`       | `/`-separated segments of `[A-Za-z0-9_.+-]{1,32}`, at most 8 deep | absent: the root record; malformed: the report is discarded           |
| `kind`     | `permission`, `question`, `auth`; with `blocked` only             | treated as absent                                                     |
| `progress` | integer `0` to `100`; with `working` or `blocked` only            | treated as absent                                                     |
| `app`      | `[A-Za-z0-9_.+-]{1,32}`                                           | longer than 32 bytes: the report is discarded; else treated as absent |
| `title`    | base64 UTF-8, ≤ 256 bytes encoded, ≤ 192 decoded                  | the report is discarded                                               |
| `msg`      | base64 UTF-8, ≤ 2732 bytes encoded, ≤ 2048 decoded                | the report is discarded                                               |

Whitespace around a key or value is trimmed. An unknown key is ignored, and for a repeated key the last one wins. A
value holding a byte outside `A-Z a-z 0-9 _ . , + / = -` is ignored as if its key were absent, except in `id`, where it
discards the report. A report is also discarded whole when the sequence exceeds 4096 bytes from `OSC` through the
terminator, a key exceeds 16 bytes, `id` exceeds 128 bytes, or decoded `title` or `msg` contains a control character. A
discarded report changes no record.

### Feature detection

`OSC 7501 ; ?` is always answered, with or without a window attached:

```
ESC ] 7501 ; ? ESC \
```

The reply lists no `states=` or `kinds=`, which per the spec means exactly the base vocabulary in the table above. It is
the only byte sequence felis writes back for this protocol.

## Records

One felis session is the spec's "terminal". A program with one status omits `id` and reports to the root record, the
session-wide one; one that runs several tasks reports each under its own path (`build/test`). A session holds at most
**256 records**, keyed by `id`.

| Event                                                                         | Effect on the session's records                                          |
| ----------------------------------------------------------------------------- | ------------------------------------------------------------------------ |
| A report with `state` other than `clear`                                      | replaces the record with that `id` whole; a key the report omits is gone |
| A new `id` while 256 records are held                                         | the least recently updated record is removed first                       |
| `state=clear` with an `id`                                                    | removes that record and every record beneath it (`a` removes `a`, `a/b`) |
| `state=clear` without an `id`                                                 | removes every record                                                     |
| The shell draws a new prompt (`OSC 133 ; A`; a continuation, `k=s`, does not) | removes `working` and `blocked` records; `idle`, `done`, `error` stay    |
| The session's program exits (the end of the PTY)                              | removes `working` and `blocked` records, after its final report applies  |
| A hard reset (RIS, `ESC c`)                                                   | removes every record                                                     |
| A soft reset (DECSTR), alternate-screen switches, detach, reattach            | none                                                                     |
| In-place daemon upgrade                                                       | records held before the upgrade are dropped; reports after it apply      |

Records change in the order the program wrote its reports, prompts, and resets.

Prompt expiry needs a shell that marks its prompts ([Mark shell prompts](../../how-to/mark-shell-prompts.md)). Without
those marks, a `working` or `blocked` record stays while the shell lives, so a program should report `done`, `error`, or
`clear` on every exit path, an interrupt included.

`OSC 9 ; 4` progress is not mapped to a record; it stays
[consumed without effect](vt-compliance.md#consumed-without-effect).

## Where records surface

`felis sessions list --format json` and `felis sessions info` show a session's records as `program_status`, sorted by
`id` ([cli.md](../cli.md)). The bridge and IPC carry the same list as `SessionInfo.program_status` ([ipc.md](../ipc.md)
"Ops (kind = 5)"). Each record is:

| Field      | Type                           | Meaning                                                         |
| ---------- | ------------------------------ | --------------------------------------------------------------- |
| `id`       | string                         | `""` for the root record                                        |
| `state`    | string                         | an open vocabulary; a reader treats an unknown value as unknown |
| `kind`     | string, optional               | an open vocabulary, as `state`                                  |
| `progress` | integer `0` to `100`, optional | absent means indeterminate                                      |
| `app`      | string, optional               | the record's own value; an ancestor's `app` is not filled in    |
| `title`    | string, optional               | decoded UTF-8, direction controls kept                          |
| `msg`      | string, optional               | decoded UTF-8, direction controls kept                          |

No event announces a change: a consumer polls, and one `felis sessions list --format json` covers every session. A
program that needs the user at once also sends a [notification](notifications.md), which `felis notifications subscribe`
streams.

`title` and `msg` reach IPC and JSON exactly as decoded. The human `felis sessions info` line spells every direction
control and control character in producer text as `<U+XXXX>`.
