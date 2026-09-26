---
title: Desktop notification protocols (OSC 9 / 99 / 777)
sidebar:
  order: 5
---

Wire formats, conformance scopes, and the JSONL relay contract of the desktop notification protocols (OSC 9, OSC 99, OSC
777).

felis adopts a **decode-and-relay** architecture: it decodes OSC 9, OSC 99, and OSC 777 into typed notification events
and exposes them on its extension surface, but it never renders popups directly or links OS notification backends.
Architectural design rationale is documented in [notifications.md](../../explanation/protocols/notifications.md).
Feature support is tracked in the protocol [support matrix](support-matrix.md#osc).

Upstream protocol references:

- OSC 99: <https://sw.kovidgoyal.net/kitty/desktop-notifications/>
- OSC 9: <https://iterm2.com/documentation-escape-codes.html>
- OSC 777: <https://man.archlinux.org/man/urxvt.1.en>

## Wire formats

### OSC 9 (iTerm2 / ConEmu)

```
OSC 9 ; <message> ST
```

Body only, no title. `<message>` is UTF-8. Disambiguated from the ConEmu progress family by the first parameter: a
single ASCII digit with further parameters following (`OSC 9 ; 4 ; …`) is window chrome, not a notification. All other
parameter forms are parsed as iTerm2 notification bodies.

### OSC 777 (rxvt-unicode)

```
OSC 777 ; notify ; <title> ; <body> ST
```

Matches the `notify` perl-extension dispatch. Only the `notify` subcommand produces notifications; other OSC 777
subcommands are ignored.

### OSC 99 (kitty)

```
OSC 99 ; <metadata> ; <payload> ST
```

`metadata` is a colon-separated list of `key=value` pairs. Recognized keys and handling:

| Key                     | Meaning                                                                 | Handling                                             |
| ----------------------- | ----------------------------------------------------------------------- | ---------------------------------------------------- |
| `i`                     | Notification identifier (multiplexer/chunk key)                         | Decoded: chunk key and echoed in events              |
| `d`                     | Done: `0` more chunks follow, `1` final                                 | Decoded: drives reassembly                           |
| `p`                     | Payload type: `title`, `body`, `icon`, `buttons`, `close`, `?`, `alive` | `title`/`body` decoded; `?` answered; others ignored |
| `u`                     | Urgency: `0` low, `1` normal, `2` critical                              | Decoded                                              |
| `e`                     | Encoding: `1` Base64, `0` plain UTF-8                                   | Decoded                                              |
| `a`                     | Actions: `focus` / `report`                                             | Rejected by design (see conformance scope)           |
| `c`                     | Close events                                                            | Ignored (no close back-channel; `p=?` omits it)      |
| `o`                     | Occasion: `always` / `unfocused` / `invisible`                          | Ignored                                              |
| `f`,`t`,`n`,`g`,`s`,`w` | App name / type / icon / sound / auto-expire                            | Ignored                                              |

**Chunking**: Escapes sharing an `i=` identifier concatenate; `d=0` indicates continuation, `d=1` completes
transmission. `title` and `body` can span multiple chunks. A `d=1` escape whose `i=` has no chunks in flight is relayed
on its own, without passing through reassembly.

**Capability query**: Producers probe capabilities with:

```
OSC 99 ; i=<id> : p=? ; ST
```

felis answers on the query-response channel with:

```
OSC 99 ; i=<id> : p=? ; p=title,body : u=0,1,2 ST
```

Both blocks are protocol notation: `OSC` is `ESC ]`, `ST` is `ESC \`, and the spaces separate tokens rather than
reaching the wire. `<id>` echoes the query's `i=`, or `0` when the query carried none. The reply advertises `title` and
`body` payload types and urgency levels 0–2, and omits action buttons (`a`) and close reports (`c`).

## Conformance scope

Statuses are the [support matrix](support-matrix.md) legend's; `—` marks a surface the protocol has no form for.

| Surface                                 | OSC 9        | OSC 777 | OSC 99                       |
| --------------------------------------- | ------------ | ------- | ---------------------------- |
| Title + body + urgency → relay          | ✅ body only | ✅      | ✅                           |
| Base64 (`e=`) + multi-chunk (`i=`/`d=`) | —            | —       | ✅                           |
| `p=?` capability reply                  | —            | —       | ✅ truthful (no `a`, no `c`) |
| Activation / close reports              | —            | —       | ⚠️ deferred (inbound)        |
| Action buttons                          | —            | —       | 🚫 rejected by design        |
| Icons / sound / auto-expire             | —            | —       | ⚠️ accepted and ignored      |

Design rationale for deferred and rejected features is detailed in
[notifications.md](../../explanation/protocols/notifications.md).

## Surfacing: `felis notifications subscribe`

Decoded notifications reach two surfaces:

1. **Window attention**: Sessions with attached GUI clients trigger taskbar/dock urgency hints via
   `Window::request_user_attention` (matching BEL semantics).
2. **Subscription stream**: `felis notifications subscribe` connects an observer to the daemon and streams notifications
   from all sessions.

Machine-readable JSONL stream (`--format jsonl`):

```console
$ felis notifications subscribe --format jsonl
{"v":1,"session_id":"a1b2…",
 "title":"Build finished","body":"0 errors","urgency":"normal",
 "notification_id":"build-42","session_title":"make",
 "cwd":"/home/you/src/felis","attached":false}
```

Field specifications:

- `session_id`: Full 32-hex session ID ([cli.md](../cli.md)).
- `title`, `notification_id`, `session_title`, `cwd`: Explicit `null` when absent from payload.
- `attached`: Boolean indicating whether active GUI windows are currently attached. Scripted CLI connections (`send`,
  `capture`) do not count.

Stream lifecycle events carry an `event` field:

| Object                                                       | Condition                                               |
| ------------------------------------------------------------ | ------------------------------------------------------- |
| `{"v":1,"event":"lag","dropped":N}`                          | Slow reader dropped N events from daemon ring buffer    |
| `{"v":1,"event":"end","count":N}`                            | Clean stream termination reporting total events emitted |
| `{"v":1,"event":"error","error":{"kind":"…","message":"…"}}` | Stream error termination                                |

The stream operates over a bounded buffer ring; lagging consumers receive explicit `lag` events rather than silently
missed items.

Filter and lifecycle flags (`--session`, `--once`, `--timeout`) and exit codes are specified in [cli.md](../cli.md) §
"Other verbs".

**Cross-host transport**: Notifications stream across SSH connections via
`felis --host user@remote notifications subscribe` ([ipc.md](../ipc.md)).

For desktop notification helper integrations, see [enable-notifications.md](../../how-to/enable-notifications.md).

## Limits

- **OSC body**: 8192 bytes between the `OSC` introducer and the terminator, the numeric prefix included. The upstream
  spec caps an OSC 99 chunk's encoded payload at 4096 bytes, so a spec-legal chunk always fits. A longer body is
  truncated to the cap and still reaches the dispatcher.
- **Reassembly size**: 64 KiB of `title` plus `body` per `i=` identifier. Crossing it discards the partial notification
  outright; nothing is relayed for that identifier.
- **Reassembly identifiers**: 32 in flight. While 32 identifiers are partially assembled, a chunk introducing a
  thirty-third is dropped.
- **Outbox**: 64 decoded notifications between daemon drains. One past the cap is dropped and the bell rings.
