---
title: Program status protocol (OSC 7501)
sidebar:
  order: 7
---

What felis implements of the OSC 7501 Program Status Protocol, the records it keeps, and where they surface. Support is
**experimental** and follows spec revision 0.4 (2026-10-10):
<https://www.superlogical.com/rex/docs/build/program-status>. Why felis takes the protocol this way is in the
[explanation twin](../../explanation/protocols/program-status.md); the verdict row is in the
[support matrix](support-matrix.md#osc).

felis keeps the records and exposes them on its extension surface. It draws nothing: no window flash, no attention hint,
no badge.

## Wire format

```
OSC 7501 ; <key>=<value> ( : <key>=<value> )* ST
OSC 7501 ; ? ST
```

BEL is accepted as the terminator alongside ST. A sequence cancelled before its terminator (CAN, SUB, or an `ESC` that
does not begin ST) is not a report and is not answered.

The keys, states, kinds, and limits are the spec's:

| Key        | Accepted values                                                   | When absent or not accepted                   |
| ---------- | ----------------------------------------------------------------- | --------------------------------------------- |
| `state`    | `idle`, `working`, `done`, `blocked`, `error`, `clear`            | the report is discarded                       |
| `id`       | `/`-separated segments of `[A-Za-z0-9_.+-]{1,32}`, at most 8 deep | absent: the root record; malformed: discarded |
| `kind`     | `permission`, `question`, `auth`; with `blocked` only             | treated as absent                             |
| `progress` | integer `0` to `100`; with `working` or `blocked` only            | treated as absent                             |
| `app`      | `[A-Za-z0-9_.+-]{1,32}`                                           | treated as absent                             |
| `title`    | base64 UTF-8, ≤ 256 bytes encoded, ≤ 192 decoded                  | the report is discarded if it does not decode |
| `msg`      | base64 UTF-8, ≤ 2732 bytes encoded, ≤ 2048 decoded                | the report is discarded if it does not decode |

Whitespace around a key or value is trimmed. An unknown key is ignored, and for a repeated key the last one wins. A
report is discarded whole when the sequence exceeds 4096 bytes from `OSC` through the terminator, a key exceeds 16
bytes, `id` exceeds 128 bytes, or decoded `title` or `msg` contains a control character. A discarded report changes no
record.

### Feature detection

`OSC 7501 ; ? ST` (or BEL-terminated) is always answered, with or without a window attached:

```
ESC ] 7501 ; ? ESC \
```

The reply lists no `states=` or `kinds=`, which per the spec means exactly the base vocabulary in the table above. It is
the only byte sequence felis writes back for this protocol.

## Records

One felis session is the spec's "terminal". A session holds at most **256 records**, keyed by `id`.

| Event                                                         | Effect on the session's records                                            |
| ------------------------------------------------------------- | -------------------------------------------------------------------------- |
| A report with `state` other than `clear`                      | replaces the record with that `id` whole; a key the report omits is gone   |
| A new `id` while 256 records are held                         | the least recently updated record is removed first                         |
| `state=clear` with an `id`                                    | removes that record and every record beneath it (`a` removes `a`, `a/b`)   |
| `state=clear` without an `id`                                 | removes every record                                                       |
| A prompt start, `OSC 133 ; A` without `k=s`, on either screen | removes `working` and `blocked` records; `idle`, `done`, `error` stay      |
| End of the PTY (the child's final output parsed first)        | removes `working` and `blocked` records; `idle`, `done`, `error` stay      |
| RIS (`ESC c`)                                                 | removes every record                                                       |
| DECSTR, alternate-screen switches, detach, reattach           | none                                                                       |
| Session reaped                                                | the records go with it                                                     |
| In-place daemon upgrade                                       | records held before the upgrade are dropped; reports parsed after it apply |

Reports, prompt starts, and RIS apply in the order their bytes arrived, including within one read.

`OSC 9 ; 4` progress is not mapped to a record; it stays
[consumed without effect](vt-compliance.md#consumed-without-effect).

## Where records surface

Every `SessionInfo` carries the session's records as `program_status`, sorted by `id` ([ipc.md](../ipc.md) "Ops (kind =
5)"), so `felis sessions list --format json`, `felis sessions info`, the bridge, and any roster reader see them
([cli.md](../cli.md)). Each record is:

| Field      | Type              | Meaning                                                         |
| ---------- | ----------------- | --------------------------------------------------------------- |
| `id`       | string            | `""` for the root record                                        |
| `state`    | string            | an open vocabulary; a reader treats an unknown value as unknown |
| `kind`     | string, absent    | an open vocabulary, as `state`                                  |
| `progress` | `0..=100`, absent | absent means indeterminate                                      |
| `app`      | string, absent    | the record's own value; an ancestor's `app` is not filled in    |
| `title`    | string, absent    | decoded UTF-8, direction controls kept                          |
| `msg`      | string, absent    | decoded UTF-8, direction controls kept                          |

`title` and `msg` reach IPC and JSON exactly as decoded. The human `felis sessions info` line spells every direction
control and control character in producer text as `<U+XXXX>`.
