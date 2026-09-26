---
title: CLI reference
sidebar:
  order: 1
---

The `felis` CLI: verbs, global flags, exit codes, and the machine-output contract. The wire protocol underneath is in
[ipc.md](ipc.md); what puts a control on this surface rather than another is in
[control-surfaces.md](../explanation/architecture/control-surfaces.md).

## Global options

Global flags precede the verb (`felis --config ./work.toml doctor`):

- `--config <path>`: selects the configuration file to read instead of the platform default ([config.md](config.md)).
  Relative paths resolve against the working directory where `felis` was launched. A missing explicitly selected file is
  an error. Verbs that read no config refuse the flag with exit `2`.
- `--host <user@host>`: dials a remote daemon over an SSH carrier.
- `--socket <path>`: dials a local daemon at an explicit socket path, on the host it runs on. A relative path is
  anchored to the caller's working directory. It is mutually exclusive with `--host` (exit `2`).
- `--ssh-arg <token>`: passes an argument token to `ssh` when `--host` is used (repeatable).

### Placement matrix

Every verb form answers every global flag; none is silently dropped. A cell reads _forwarded_ (rebuilt onto the
frontend's command line, `--config` in its absolute form), _read_ (this process consumes the selection itself:
`config path` resolves it, the rest parse it), _dials_ (names the daemon the verb connects to), or _refused_ (a usage
error, exit `2`).

| Verb form                                                                  | `--config` | `--host`                | `--socket` | `--ssh-arg`             |
| -------------------------------------------------------------------------- | ---------- | ----------------------- | ---------- | ----------------------- |
| Window launch: bare `felis`, `felis -- <cmd>`, `attach`                    | forwarded  | forwarded               | forwarded  | forwarded               |
| Headless verbs: `sessions`, `notifications`, `daemon`, `version`, `bridge` | refused    | dials                   | dials      | dials                   |
| `config path` / `check` / `show-effective`                                 | read       | refused                 | refused    | refused                 |
| `doctor`                                                                   | read       | dials                   | dials      | dials                   |
| `felis ssh`, `window retarget`                                             | refused    | refused                 | refused    | refused                 |
| `frontend <name>`                                                          | refused    | refused                 | refused    | refused                 |
| `completions <shell>`, `__mangen <dir>`                                    | refused    | refused                 | refused    | refused                 |
| `__complete-sessions`                                                      | refused    | no candidates, exit `0` | dials      | no candidates, exit `0` |
| `--version`                                                                | refused    | refused                 | refused    | refused                 |

`doctor` dials, so `--host` makes its daemon row report the remote daemon; every other row still describes this machine.
`felis ssh` and `window retarget` carry their own destination in place of the global carrier flags.
`__complete-sessions` runs once per `<TAB>` and never runs `ssh` (see "Other verbs").

`--ssh-arg` requires `--host`: on its own it is a usage error (exit `2`) on every verb form, so its column describes
only the cells `--host` already selects.

## Exit codes

The exit code is a function of `error.kind` alone, apart from the exceptions the table names. `config check`, `doctor`,
a `sessions search` that matched nothing, and a `notifications subscribe --once` whose stream the daemon ended first
exit `1` without an error object, and `felis bridge` overrides the code its own pipe failure would otherwise carry:

| Code | Verbs                                               | Meaning                                                                                                                                                                                                                    |
| ---- | --------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `0`  | all                                                 | Success.                                                                                                                                                                                                                   |
| `1`  | all                                                 | Typed refusal: valid request, but refused on the merits (no such session, daemon at capacity, wait timed out).                                                                                                             |
| `1`  | `config check`, `doctor`                            | Diagnostics were reported: the document has errors, or a check reported `fail`. Nothing was refused, and the result object still lands on stdout.                                                                          |
| `1`  | `sessions search`, `notifications subscribe --once` | Nothing to report: the search matched nothing, or the daemon ended the stream before a matching notification arrived. The stream's clean terminal still lands; no error object is written.                                 |
| `1`  | `felis bridge`                                      | The bridge's own stdin or stdout failed, or its first `Ops` dial was refused `at_capacity`. The kinds `input_failed` and `output_failed` exit `2` everywhere else; a stdout failure cannot carry its own correlated error. |
| `2`  | all                                                 | Failure to ask: bad syntax, unreachable daemon, connection lost, protocol break, or unsupported protocol major.                                                                                                            |

`config show-effective` is not one of the exceptions above: past a document error it refuses with `invalid_request`.

## Machine output

The `--format` flag selects machine-readable framing on supported verbs:

```
--format human|json|jsonl
```

`human` is the default and is not a parse target. Verbs are partitioned into four classes:

| Class                | `--format` values | Output convention                                                                                                                                                                                                                                                    |
| -------------------- | ----------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Point**            | `human`, `json`   | Exactly one JSON object on stdout. Failures write one error object on stderr with exit code `1` or `2`.                                                                                                                                                              |
| **Point-diagnostic** | `human`, `json`   | Point framing, but `1` reads as "the diagnostics the verb was asked for": a result object with the findings on `config check` and `doctor`, an `invalid_request` error object on `config show-effective`, which has no honest result to print past a document error. |
| **Stream**           | `human`, `jsonl`  | One JSON object per line on stdout, ending in one terminal object. stderr carries human diagnostics only.                                                                                                                                                            |
| **Exempt**           | _(none)_          | Fixed text or raw streaming (window launches, completions, bridge).                                                                                                                                                                                                  |

Classification by verb:

| Class            | Verbs                                                                                                                                                                                                                               |
| ---------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Point            | `sessions list`, `sessions info`, `sessions spawn`, `sessions send`, `sessions kill`, `sessions evict`, `sessions tag`, `sessions switch`, `felis ssh`, `window retarget`, `daemon status`, `daemon stop`, `config path`, `version` |
| Point-diagnostic | `config check`, `config show-effective`, `doctor`                                                                                                                                                                                   |
| Stream           | `sessions capture`, `sessions search`, `notifications subscribe`                                                                                                                                                                    |
| Exempt           | `felis`, `felis attach`, `felis frontend`, `felis completions`, `felis bridge`                                                                                                                                                      |

A verb with no row in this table has no `--format` flag. `felis bridge` streams exactly the operations whose verb is a
Stream here.

Selecting an incompatible format (such as `sessions list --format jsonl`) is a clap usage error: it exits `2` with human
text on stderr, no error object, and lists the values the verb takes. The `felis` binary's own tracing logs go to stderr
at `WARN` and above; setting `--format json` or `jsonl` silences them. `RUST_LOG` overrides either default.

### The envelope

Every top-level JSON object carries `"v":1`:

| Object           | Shape                                                             | Channel |
| ---------------- | ----------------------------------------------------------------- | ------- |
| Point result     | `{"v":1, ...}`                                                    | stdout  |
| Point failure    | `{"v":1,"error":{"kind":"...","message":"..."}}`                  | stderr  |
| Stream item      | `{"v":1, ...}`                                                    | stdout  |
| Lag event        | `{"v":1,"event":"lag","dropped":N}`                               | stdout  |
| Clean terminal   | `{"v":1,"event":"end","count":N}` (plus optional `"exit_code":N`) | stdout  |
| Failing terminal | `{"v":1,"event":"error","error":{"kind":"...","message":"..."}}`  | stdout  |

`"v"` is the epoch of this CLI contract. Within an epoch, fields may be added additively; existing fields are never
renamed or retyped without bumping `v`.

`error.kind` is an additive vocabulary: `usage`, `no_match`, `ambiguous`, `timeout`, `no_input_owner`,
`no_such_attachment`, `not_queued`, `session_ended`, `malformed_request`, `invalid_request`, `at_capacity`,
`unsupported`, `daemon_unreachable`, `daemon_lost`, `refused`, `protocol`, `canceled`, `input_failed`, `output_failed`,
`internal`. `error.message` is human-readable diagnostic prose.

Stream verbs emit their terminal object even when failing before the first item. An EOF with no terminal object is a
protocol failure (exit `2`).

**Frozen for `v:1`.** The two format words and the class each binds to, the error object's shape, the `error.kind`
spellings above with the exit codes the table gives them, and the two transports (the CLI's per-class stdout/stderr
split, the bridge's single stdout correlated by `id`) hold for the whole epoch. The vocabulary still grows: new tokens
land within `v:1`, existing ones are never respelled, remapped, or narrowed: a new token may not take over part of what
an existing one already covers. The rationale is in
[control-surfaces.md](../explanation/architecture/control-surfaces.md).

Framing begins once the arguments parse. A refusal clap states (an unknown flag, a `--format` value the verb's class
does not take, a missing positional) is human text on stderr with exit `2`, before any format is chosen; from there
every failure is framed, the steps that run before the verb body included: resolving `--config` (`usage`), resolving the
daemon the verb dials (`daemon_unreachable`), building the runtime (`internal`), and reading a `-` payload from stdin
(`input_failed`). On the bridge a shape failure is correlated whenever a valid `id` was recovered from the line, and
carries `id: null` otherwise.

### JSON Schema

Two JSON Schema (draft 2020-12) documents describe every machine object. Every output body is generated from the serde
types the binary serializes; the bridge's per-operation request parameters are explicit schema types, held to the
runtime's parameter table by a guard test. Both documents live in
[crates/felis-cli/schemas](../../crates/felis-cli/schemas):

- [`felis-cli-v1.schema.json`](https://raw.githubusercontent.com/felis-terminal/felis/main/crates/felis-cli/schemas/felis-cli-v1.schema.json)
  covers the objects above. Its `$defs` name the classes of the table: `point_result`, `point_error`, `stream_item`,
  `lag_event`, `end_terminal`, `error_terminal`, plus the unversioned `point_body`, `stream_item_body` and `error_body`
  that `felis bridge` reuses.
- [`felis-bridge-v1.schema.json`](https://raw.githubusercontent.com/felis-terminal/felis/main/crates/felis-cli/schemas/felis-bridge-v1.schema.json)
  covers the bridge. `$defs/request` is one stdin line; the remaining `$defs` are the six shapes stdout carries.

The `v1` in each name is the epoch of `"v"`: a later epoch adds a file rather than rewriting one. Payload objects stay
open, because a field may be added within an epoch, and `error.kind` is an open string whose `x-known-values` annotation
lists this epoch's tokens, so a validator does not reject a token minted after it was written.

A request parameter whose vocabulary is finite is enumerated rather than typed as a bare string. Two constraints the
grammar does not close: `attachment` is constrained by pattern, so it admits any decimal-digit string and one too large
for a 64-bit id is still answered `malformed_request`; and a validator comparing JSON numbers as `f64` admits values
within one `f64` step of a 64-bit `minimum` or `maximum`, about 2048 past an `i64` maximum and 4096 past a `u64` one.
Every field carrying such a bound is annotated `x-bound-exceeds-f64-precision`, and the exact bound is read by parsing
the number as a 64-bit integer.

The same precision limit fixes how integer ids are spelled, and the rule is frozen for the epoch: an id whose range
exceeds what an `f64` holds exactly is a **decimal string** in every machine object, never a JSON number.
`Attachment.id` is the one such id today; it is a `uint64` on the wire and a string here, while a session id is 32
hexadecimal digits in both. A reader therefore never has to know which ids round.

JSON Schema cannot state the lifecycle rules: exactly one terminal per stream, an id free again after its terminal,
nothing after a terminal. Those are pinned by the golden conversations under `crates/felis-cli/tests/fixtures/bridge/`
([testing.md](testing.md) "Machine-surface schemas").

### Session identity

A session id is 32 hexadecimal digits (`id`). Stored and scripted identifiers must always use the full 32-hex
representation.

`short_id` is provided on `list` and `info` as a display convenience. It is the shortest unique prefix for the current
roster (at least 8 digits): shortened by the daemon against its own pool on `info`, and against the roster in the reply
on `list`. Because roster membership changes, `short_id` is display-only.

Mutating verbs resolve daemon-side and return the full `id`. All verbs accept either the full 32-hex id or any unique
prefix as an input argument.

### Result objects

Inside the envelope, each verb carries a body of its own keys. A key marked _omitted when unset_ is absent from the
object rather than `null`.

`sessions list` answers `{"v":1,"sessions":[…]}`, one session object per roster entry and an empty array for an empty
roster; `sessions info` answers a single session object beside `"v"`. Both carry the same keys:

| Key                 | Type             | Presence                                                           |
| ------------------- | ---------------- | ------------------------------------------------------------------ |
| `id`                | 32-hex string    | always                                                             |
| `short_id`          | string           | always                                                             |
| `rows`, `cols`      | number           | always                                                             |
| `idle_seconds`      | number           | omitted while a window is attached; it counts from the last detach |
| `title`             | string           | omitted until the session sets one                                 |
| `cwd`               | string           | omitted when the daemon has none for the session                   |
| `tags`              | array of strings | always, empty when untagged                                        |
| `last_notification` | object           | omitted until the session emits one                                |
| `foreground`        | string           | omitted when no foreground command is known                        |
| `last_exit_code`    | number           | omitted until an OSC 133 `D` mark carries one                      |
| `exited`            | boolean          | omitted when false; `true` marks a session in its post-exit grace  |
| `attachments`       | array of objects | always, empty when no window is attached                           |

`last_notification` carries `body` (string), `urgency` (`low`, `normal`, or `critical`) and `age_seconds` (number) on
every occurrence, and `title` (string) except for a body-only notification. Each `attachments[]` entry carries `id` (a
decimal **string**, the spelling `--attachment` accepts), `attached_at` (RFC 3339, UTC) and `input_owner` (boolean), all
three always present.

The mutating verbs answer a point result naming what they reached:

| Verb                     | Body                                                                                    |
| ------------------------ | --------------------------------------------------------------------------------------- |
| `spawn`, `kill`, `send`  | `{"id"}`                                                                                |
| `send --wait`            | `{"id","exit_code"}`; `exit_code` is omitted for a bare `OSC 133 ; D` that carries none |
| `evict`                  | `{"id","was_attached"}`                                                                 |
| `tag`                    | `{"id","tags"}`, the daemon's sorted list after the change                              |
| `switch`                 | `{"from","to","queued"}`                                                                |
| `ssh`, `window retarget` | `{"from","queued"}`                                                                     |

`queued` counts the subscriber outboxes that took the push, not the windows that finished attaching. It is the whole
completion claim these verbs make ([ipc.md](ipc.md) "Ops (kind = 5)"): exit `0` from `switch`, `ssh` and
`window retarget` means the request was accepted by that many outboxes, and the landing is not observable from the
invocation that asked for it. A window that takes the push and then fails to attach, exits, or cannot reach the target
daemon leaves the exit code and every field of the result unchanged.

Human framing states the same limit in words. `switch` and `window retarget` print
`queued on 1 window; not landed — each window runs its own attach` (`re-dials on its own` for a retarget), and a switch
onto the session the window already holds prints `already on session <id>; nothing queued`.

The stream verbs frame one object per item between the envelope's `"v"` and their terminal:

| Verb                      | Item keys                                                                                       |
| ------------------------- | ----------------------------------------------------------------------------------------------- |
| `capture`                 | `row`, `text`, `soft_wrap_continued`, plus `ansi` under `--ansi`                                |
| `search`                  | `line_index`, `text`, `byte_spans`, `col_spans`                                                 |
| `notifications subscribe` | `session_id`, `title`, `body`, `urgency`, `notification_id`, `session_title`, `cwd`, `attached` |

A `capture` `row` is the region's own coordinate: scrollback rows count backwards from `-1` (the youngest), live rows
carry their grid index, and mark ranges start at 0. `search`'s `line_index` follows the same convention; its
`byte_spans` are `[start, end]` pairs into `text`, and its `col_spans` are per-row `[row, start_col, end_col]` triples,
one per row a match touches. A notification item omits no key: an unknown `title`, `notification_id`, `session_title` or
`cwd` is `null`.

## Session verbs

All session verbs run under `felis sessions <verb>`:

| Verb                | Summary                                      |
| ------------------- | -------------------------------------------- |
| `list`              | List sessions in the daemon pool.            |
| `info <id>`         | Show metadata and attachments for a session. |
| `spawn [-- cmd...]` | Create a new detached session.               |
| `send <id> [text]`  | Paste text or send keystrokes to a session.  |
| `capture <id>`      | Stream terminal screen or scrollback rows.   |
| `search <id> <pat>` | Search scrollback history.                   |
| `kill <id>`         | Terminate a session and its process group.   |
| `evict <id>`        | Disconnect all clients from a session.       |
| `switch <id>`       | Retarget one window onto another session.    |
| `tag <id> [tag...]` | Add or remove session tags.                  |

Exit codes follow the `error.kind` rule ("Exit codes"). Two are worth naming: `search` exits `1` when nothing matched,
and `list` exits `0` on an empty roster.

### Verb details

- **`list`**: `--tag <t>` filters sessions by tag. A session in exit grace is marked `(exited)` or `"exited":true`.
- **`info <id>`**: displays geometry, idle time, title, cwd, foreground command, exit code from OSC 133 `D` marks
  (`exit:` / `"last_exit_code"`), and active window attachments (`window:` / `"attachments"`).
- **`spawn`**: `--tag <t>` sets initial tags (up to 32 tags of 128 bytes). `--env KEY=VAL` sets child environment
  variables. `--rows N` and `--cols N` set initial geometry (1..=2048, REQ-605a); they are one flag pair, so naming only
  one is a usage error (exit `2`) and naming neither asks for the daemon's default grid (24 rows by 80 cols).
  `--cwd <dir>` sets the child's working directory. Limits follow REQ-105a (4 KiB paths, 1 MiB argv/env). The program,
  its argv, and the cwd travel as UTF-8 ([ipc.md](ipc.md)), so a `--cwd` that is not valid UTF-8, and a caller working
  directory that is not valid UTF-8 when the spawn is local, are each a usage error (exit `2`) rather than a lossy path.
  Starts a daemon on the host it dials, `--host` included ("Auto-spawning").
- **`send`**: one operation, and three constraints are its whole grammar: at least one of TEXT, `--key` or `--wait`;
  `--raw` requires TEXT; `--timeout` requires `--wait`. Each unmet constraint is a usage error (exit `2`). It pastes
  text; `--raw` sends literal bytes without bracketed paste; `-` reads stdin. `--key <chord>` (repeatable) presses keys
  after text (`up`, `escape`, `ctrl+c`, `enter`). A chord travels as the keystroke itself and the daemon encodes it
  against the session's current keyboard modes, so it needs no knowledge of them from the caller. A chord's character is
  capped at 32 UTF-8 bytes (REQ-105a); past it the verb exits `1` under `invalid_request`. `--wait` blocks until the
  session emits an OSC 133 `D` mark and prints its exit code, or `-` for a bare `D` that carries none, so
  `felis sessions send <id> --wait` watches a command someone else started. `--timeout <secs>` bounds everything past
  the writes: the daemon's admission of the input and the mark watch alike. An unbounded `--wait` waits indefinitely,
  and `--timeout 0` is a deadline already past, so neither wait gets a chance to finish and the verb exits `1` under
  `timeout`. Payload cap is 16 MiB minus 64 bytes of text, 16 MiB with `--raw` (REQ-105a).
- **`capture`**: `--source <visible|scrollback|command-output|last-command>` picks the region; `scrollback` streams
  retained history oldest-first, `command-output` and `last-command` the output bounded by OSC 133 prompt marks.
  `--lines N` limits output to the last N rows, and under `--source scrollback` the daemon transmits that tail alone
  rather than the whole buffer. Output is plain text; `--ansi` adds the SGR escapes that reconstruct each cell's color
  and style, on every `--source`.
- **`search`**: substring search by default; `--regex` enables regular expressions, `--case-insensitive` folds case.
  Pattern limit is 4 KiB.
- **`switch`**: moves one window attached to the current session (`$FELIS_SESSION_ID` or `--from <id>`) to target
  `<id>`. When several windows mirror that session, the one that most recently received user input moves;
  `--attachment <ID>` names a window explicitly. A background `sessions send` is not a window and never becomes that
  input owner. Exit `1` covers an unknown target, an `--attachment` that has detached, and a source session no window
  can be chosen for unambiguously.
- **`tag`**: positional tags are added; `--remove <tag>` removes tags.

## Config verbs

All config verbs operate locally on the configuration file and **never dial a daemon**:

| Verb             | Summary                                        |
| ---------------- | ---------------------------------------------- |
| `path`           | Print resolved `config.toml` path.             |
| `check`          | Validate configuration and report diagnostics. |
| `show-effective` | Print effective config with defaults applied.  |

`check` exits `1` only when the document has errors; warnings alone exit `0`. It reports all errors and warnings in one
run. Missing default config files are treated as valid (defaults apply). Missing files explicitly named via `--config`
are reported as errors. `--client <id>` selects a specific client overlay (default `felis`).

## Daemon status

`felis daemon status` inspects a running daemon and reports resource utilization against configured ceilings:

```
version:  0.1.0 (e3abf80c9d21)
wire:     1.0
workers:  8
draining: no
connections             3  /  1024 per daemon
sessions                2  /  256 per daemon
image_store_bytes       total 4 MiB  ·  max session 4 MiB  /  256 MiB per session
in_flight_decodes       0  /  256 per daemon
in_flight_decode_bytes  total 0 B  ·  max session 0 B  /  64 MiB per session
subscriber_queue_bytes  total 0 B  ·  max subscriber 0 B  /  512 MiB per subscriber
pty_input_bytes         total 0 B  ·  max session 0 B  /  16 MiB per session
```

JSON output (`--format json`) yields:

```json
{"v":1,"version":"...","protocol":{"major":1,"minor":0},"worker_threads":N,"draining":false,"resources":[...]}
```

### Accounted resources

| `resource`               | Unit  | Scope        | Ceiling                                                            |
| ------------------------ | ----- | ------------ | ------------------------------------------------------------------ |
| `connections`            | count | `daemon`     | 1024 connections (REQ-916).                                        |
| `sessions`               | count | `daemon`     | 256 sessions (REQ-915).                                            |
| `image_store_bytes`      | bytes | `session`    | 256 MiB per session (REQ-1008). Oldest images evicted on pressure. |
| `in_flight_decodes`      | count | `daemon`     | 256 transmissions.                                                 |
| `in_flight_decode_bytes` | bytes | `session`    | 64 MiB per session (REQ-908).                                      |
| `subscriber_queue_bytes` | bytes | `subscriber` | 512 MiB per subscriber. Slow subscribers evicted.                  |
| `pty_input_bytes`        | bytes | `session`    | 16 MiB per session (REQ-1011a). Socket reads pause until drained.  |

`resource`, `unit`, `scope` and `total_used` are on every row. The rest are the wire's two scope arms flattened into
keys, so **absent is not `0`**:

| Field               | Absent means                                                                 | `0` means                      |
| ------------------- | ---------------------------------------------------------------------------- | ------------------------------ |
| `max_subject_used`  | `scope` is `daemon`, whose one subject is `total_used`                       | every subject is empty         |
| `per_subject_limit` | `scope` is `daemon`, or the subject arm's ceiling is unlimited               | a subject may hold none of it  |
| `global_limit`      | the arm's ceiling is unlimited: felis budgets no aggregate for this resource | the daemon may hold none of it |

In `--format json` an absent field is an **omitted key**, never `null` or `0`. The human output spells the same arms in
words: an unlimited ceiling prints `unlimited`, except on a subject-scoped row whose aggregate is unlimited, which drops
the daemon-wide column rather than naming it: the ceiling that binds there is the per-subject one beside it, and a
reader offered two `unlimited`s would have to work out which. A zero prints as `0`, and a `daemon`-scope row prints no
per-subject column at all.

Exit codes: `0` when answered; `1` if at connection capacity; `2` if unreachable or protocol version is unsupported.

`draining` is `true` between a `felis daemon stop --when-empty` and the daemon's exit.

## Daemon stop

`felis daemon stop` ends a running daemon over the same IPC carrier every other verb uses, so it works on every
supported platform and against a daemon selected with `--socket` or `--host`. It never starts a daemon. `felis-daemon`
has no `stop` subcommand and installs no signal handler, though `SIGINT` and `SIGTERM` still end the process; the
portable way down is `felis daemon stop` over IPC.

```
felis daemon stop [--force | --when-empty] [--format human|json]
```

| Mode           | Behavior                                                                                                                         |
| -------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| _(default)_    | Stops only while the daemon admits nothing. With sessions or a create in flight, nothing is destroyed and the count is reported. |
| `--force`      | Destroys every session, waits for each child to be reaped, then stops.                                                           |
| `--when-empty` | Enters the draining state: new sessions are refused and the daemon exits after the last one ends.                                |

`--force` and `--when-empty` are mutually exclusive (exit `2`).

A stop that is answered reports which mode it took beside the outcome:

```json
{"v":1,"outcome":"stopping","mode":"force","sessions":0}
{"v":1,"outcome":"draining","mode":"when_empty","sessions":2}
```

A refused stop is an error object, and the one carrying a `sessions` key:

```json
{ "v": 1, "error": { "kind": "refused", "message": "...", "sessions": 2 } }
```

Once a daemon is draining, a further default or `--when-empty` stop reports the same state: `draining` with the sessions
left, or `stopping` once none remain. `--force` escalates instead and destroys them. A create arriving meanwhile is
refused with `invalid_request`, whose message names the draining daemon (the wire reason is
`CreateFailure::DaemonDraining`, [ipc.md](ipc.md)).

Exit codes: `0` when the daemon is stopping or draining; `1` when the default mode is refused; `2` if unreachable or too
old to answer, in which case that daemon's own generation of instructions applies.

## Doctor

`felis doctor` validates runtime dependencies and system health:

```
ok      daemon         running, build 0.1.0 (e3abf80c9d21), negotiated wire 1.0
warn    config         ~/.config/felis/config.toml: 2 warning(s)
fail    terminfo       no xterm-felis entry in terminfo database
ok      gpu            AMD Radeon RX 7900 XT (vulkan, discrete_gpu, radv)
ok      clipboard      OS clipboard reachable
ok      remote_helper  ssh at /usr/bin/ssh
```

JSON output (`--format json`):

```json
{"v":1,"failed":N,"warned":N,"checks":[{"check":"...","status":"ok|warn|fail|skipped","detail":"..."}]}
```

| `check`          | Verified property                                                      | Failure conditions                                                                                         |
| ---------------- | ---------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------- |
| `daemon`         | Daemon connectivity and protocol major compatibility.                  | Protocol major mismatch. Not running, undialable, unverified, refused, or at capacity reports `warn`.      |
| `daemon-sibling` | What the default endpoint holds while this shell is stamped elsewhere. | A daemon answering it reports `warn`; a cold default reports `ok`. Emitted on the local Unix carrier only. |
| `config`         | `config.toml` syntax and validation.                                   | Configuration parse errors. Warnings report `warn`.                                                        |
| `terminfo`       | `xterm-felis` entry presence in terminfo database.                     | Entry not found on Unix systems.                                                                           |
| `gpu`            | WGPU graphics adapter initialization.                                  | No adapter found. CPU fallback adapters report `warn`.                                                     |
| `clipboard`      | OS clipboard accessibility.                                            | Unavailable clipboard reports `warn` (falls back to in-process clipboard).                                 |
| `remote_helper`  | SSH executable availability on `PATH`.                                 | Missing executable reports `warn`.                                                                         |

`gpu` and `clipboard` are probed by executing `felis-client` in probe mode. In headless installations lacking the GUI
binary, these report `skipped`. Exit codes: `1` if any check reports `fail`, `0` otherwise.

The `daemon` row dials the target the global flags name. On the platform default that dial is bounded at 2 s and its
failures are split: `ENOENT` or `ECONNREFUSED` reports "not running", any other connect error reports "could not dial
\<path\>", and a peer that answered something short of the felis handshake reports "something listens at \<path\> but
did not answer as a felis daemon". The deadline covers the connect and the handshake together and is reported as the
phase it caught. `--socket`, a `$FELIS_SOCKET` stamp, `--host`, and Windows targets keep an unbounded dial and the
single "not running" rendering. On every target, a daemon that answers with a preface status this build does not know,
or with a refusal of any reason, reports "running, but …".

On the local Unix carrier, a `daemon` row that reports "not running" after a refused connect says what is at the
endpoint. A socket inode there is what a stopped daemon leaves behind, so the row reads "not running (stale socket,
replaced on the next start)"; a directory, a symlink, or a regular file keeps the generic wording and names what was
found.

`doctor` reports a **sibling daemon** when this shell's `$FELIS_SOCKET` stamp names an endpoint that is not the default
one. It probes the default with one bounded read-only dial and emits a `daemon-sibling` row stating what it holds: a
felis daemon answering is a `warn` row naming what answered and pointing at `felis --socket <path> sessions list`; a
default that is cold, that cannot be dialed, or that answered as something other than a felis daemon is an `ok` row
saying which. The `daemon` row gains a note that this shell keeps targeting the stamped endpoint unless run with
`env -u FELIS_SOCKET` or `--socket <the default>`. An explicit `--socket` gets no sibling row, and neither does a target
that is the default. When the default cannot be resolved, the `daemon` row carries the reason and nothing is probed.

## Window launches

Top-level invocations that launch the GUI client:

| Form                        | Action                                                    |
| --------------------------- | --------------------------------------------------------- |
| `felis`                     | Create a new session and launch a window attached to it.  |
| `felis attach <id>`         | Open a window attached to an existing session.            |
| `felis -- <cmd...>`         | Open a window running `<cmd>` in a persistent session.    |
| `felis frontend <name> ...` | Launch an alternate frontend executable (`felis-<name>`). |

`felis -- <cmd>` creates a standard persistent session; closing the window does not terminate the process. For a
detached command without a window, use `felis sessions spawn -- <cmd>`.

When its shell exits, a window takes the first of these it can:

1. a retarget or reattach it had parked while busy;
2. its trail, the places the user themself took the window, newest first, each an exact live session on its own daemon;
3. the nearest live neighbor of the exited session in the ring on the current daemon.

With none of that left it closes and exits `0`. When the transport to the daemon drops instead, the window marks its
title `— disconnected` and re-dials the same session for up to six attempts: roughly 23 seconds of backoff between them,
plus up to 10 seconds for each attempt itself, so a ladder whose attempts hang runs about 83 seconds rather than 23. A
reconnect that finds the session gone enters the same trail unwind rather than closing outright, so it ends the same way
any other shell exit does: attached elsewhere, or closed with exit code `0`. Every other reconnect failure closes the
window directly with exit `2`, the reason and the remedy in the log: the daemon refused the window, or no daemon
answered inside the retry budget (`felis attach <id>` once it is back).

## Re-point a window across daemons: `felis ssh`, `felis window retarget`

Retargets an active window from inside its session to another daemon. `felis ssh <dest>` re-points **this** window at
the daemon on `<dest>`; `felis --host <dest>` is the other operation, opening a **new** window over there.

```
felis ssh <user@host> [--ssh-arg <token>]... [--from <prefix>]
                      [--attachment <id>] [--session <prefix>]
                      [-- <cmd>...]
felis window retarget [<socket>] [--from <prefix>]
                      [--attachment <id>] [--session <prefix>]
                      [-- <cmd>...]
```

- `<user@host>`: any destination `ssh` accepts, passed verbatim; required.
- `<socket>`: a local daemon socket, valid UTF-8 because the retarget descriptor carries it as a `string`
  ([ipc.md](ipc.md)); anything else is a usage error (exit `2`). Absent: the default local daemon, which is the way back
  from `felis ssh`.
- `--ssh-arg <token>`: arguments passed to SSH (repeatable), on `felis ssh` only.
- `--from <prefix>`: selects source session (defaults to `$FELIS_SESSION_ID`).
- `--attachment <id>`: selects specific window attached to the source session. Absent, the window moved is the source
  session's last input owner.
- `--session <prefix>`: attaches to an existing session on the target daemon.
- `-- <cmd...>`: spawns a new command on the target daemon.

Destination descriptors are bounded at 64 KiB (REQ-105a). Exit codes: `0` once the window's outbox has taken the push
and not once it has landed ("Result objects"), `1` if window not found or descriptor over limit, `2` on invalid
arguments or unreachable daemon.

## Alternate frontends: `felis frontend <name>`

Executes `felis-<name>` found beside the `felis` binary or on `$PATH`, forwarding all remaining arguments verbatim.

Global carrier flags (`--host`, `--socket`, `--ssh-arg`) are refused before the frontend name: each frontend manages its
own connection flags. `--config` is refused there for the same reason: `felis` does not know how `felis-<name>` spells
it. Write it after the name, in the frontend's own spelling: `felis frontend tui --config ./work.toml`.

## Carrier and connection lifetime

- **Connect, operate, disconnect**: CLI verbs connect, perform a single operation, and disconnect. Verbs do not keep
  connections open, except for `felis bridge`.
- **Carrier resolution order**: local daemon socket resolution checks `--socket`, then `$FELIS_SOCKET`, then the
  platform default, and anchors a relative path to the caller's working directory. `--host` uses SSH exclusively and
  ignores local socket variables.

  The platform default is derived from the uid alone:

  | OS      | Default endpoint                                           |
  | ------- | ---------------------------------------------------------- |
  | Linux   | `/tmp/felis.<uid>/daemon.sock`                             |
  | macOS   | `/tmp/felis.<uid>/daemon.sock`                             |
  | Windows | `\\.\pipe\felis.<sid>.daemon`, derived from the user's SID |

  No environment variable takes part: `$XDG_RUNTIME_DIR`, `$TMPDIR` and `/run/user/<uid>` are not consulted
  ([ipc.md](../explanation/architecture/ipc.md#cross-host-attach-ssh-stdio)).

- **Socket directory**: whichever of the three sources named the socket, its parent must be a directory the uid owns
  with mode exactly `0700`, and the daemon creates the default one that way. A parent that is a symlink, another user's,
  or any other mode fails the daemon's start naming what was found; it is never tightened. So `--socket /tmp/x.sock` is
  refused, while `--socket ~/.felis-alt/daemon.sock` with a `0700` `~/.felis-alt` is not. Place that directory where no
  other user can rename it. Two daemons may share one dedicated directory. A squatted or missing parent reaches a client
  as an ordinary failed dial.

### Auto-spawning

Which forms may leave a daemon behind is a property of the verb, the same over a local socket and over `--host`. Over
SSH the non-spawning forms dial `felis-daemon relay --no-spawn`, which reports "no daemon on this host" instead of
starting one; the spawning forms dial `relay` without that flag and let it apply the local spawn-then-retry policy on
the remote side.

| Form                                                                                                                                                                                                  | Local socket            | `--host`                                    |
| ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------- | ------------------------------------------- |
| `sessions spawn`, window launches (bare `felis`, `felis attach`, `felis -- <cmd>`)                                                                                                                    | starts a daemon         | the relay starts one                        |
| Read and drive verbs (`list`, `info`, `capture`, `search`, `send`, `kill`, `evict`, `tag`, `switch`, the retargets' source dial), `notifications subscribe`, `daemon status` / `stop`, `felis bridge` | unreachable, exit `2`   | `relay --no-spawn`, then exit `2`           |
| `version`, `doctor`                                                                                                                                                                                   | reported as not running | `relay --no-spawn`, reported as not running |

A **cold socket** is a connect that failed with `ENOENT` or `ECONNREFUSED` (`ERROR_FILE_NOT_FOUND` for an absent Windows
pipe, which reports as not-found). Only that licenses a spawn, on the local dial, the systemd hand-off and the relay
alike: any other connect error (`EACCES`, an exhausted descriptor table, …) ends the dial naming the path and the error.
A failure after the connect succeeded (a preface that never arrived, an EOF before `Welcome`) keeps the spawn-and-retry
behavior, with one exception: a peer-identity failure (REQ-106, a uid or user-SID mismatch, or a credential query that
fails) ends the dial with that error on every path. A listener another account serves holds the endpoint, so a spawn
there would put a second daemon beside a live one.

How a cold daemon is started depends on where the launcher runs. On Linux, a launcher under the systemd user manager
asks the manager to run the daemon as a transient service, `felis-daemon-<hash>.service` in `app.slice` with
`OOMPolicy=continue`, from the launcher's own binary on the launcher's socket; `<hash>` is derived from that socket
path, so a `--socket` daemon and the default daemon hold separate units. `systemctl --user stop` and `restart` on the
unit are refused (`RefuseManualStop=yes`), leaving [`felis daemon stop`](#daemon-stop) the way down. The rest of the
unit is `Type=notify`, so the start job completes only once that daemon has bound that socket; `TimeoutStartSec=15s` as
the budget for it; `--collect`, so a unit that ends inactive or failed unloads itself and leaves the name free for the
next window; and systemd's default `KillMode=control-group`, which felis does not override. Every call the launcher
makes to the manager is bounded, so a manager that stops answering delays a window by seconds rather than holding it.
Every other launcher (a login-session shell, macOS, Windows, a host without systemd), and any hand-off that does not
yield a reachable daemon, starts it as a detached child instead, which stays in the launching process's control group.

The two kinds then differ in how long the process lives, which is the host's policy rather than anything felis holds. A
transient unit lives as long as `user@<uid>.service`, which logind stops `UserStopDelaySec` (10 s by default) after the
last logout of a non-lingering uid, so a host that serves daemons across logouts wants `loginctl enable-linger <user>`.
A detached child sits in the login session's own scope: logind's default `KillUserProcesses=no` leaves it running past
the last logout, while a host that sets `KillUserProcesses=yes` kills it there unless that account is named in
`KillExcludeUsers=`.

Shell completion is the one form that never dials over `--host` at all: `__complete-sessions` drops the carrier tokens
and exits `0` with no candidates ("Global options"), so a `<TAB>` cannot start a daemon on either host.

A retarget is two dials, and only the second one lands: `felis ssh` and `window retarget` reach the _source_ daemon as a
drive verb, then the window they move dials the **target** host itself. That landing follows the window-launch spawn
policy, not the drive policy
([control-surfaces.md](../explanation/architecture/control-surfaces.md#a-session-verb-never-spawns-a-daemon-implicitly)).

| Retarget landing           | Behavior                                                                                             |
| -------------------------- | ---------------------------------------------------------------------------------------------------- |
| `felis ssh <user@host>`    | The relay starts a cold daemon, for `--session` and `-- <cmd>` alike.                                |
| `window retarget <socket>` | A cold socket fails the retarget (`felis-client` reports an unreachable daemon); nothing is started. |

## Other verbs

### `felis notifications subscribe`

Streams OSC 9 / 99 / 777 desktop notifications. A stream verb (`--format jsonl`):

- `--session <id>`: filters notifications to one session.
- `--once`: waits for the next matching notification, prints it, and exits `0`. It exits `1` when `--timeout` elapses
  first and when the daemon ends the stream first, and `2` when the daemon closes the connection without a terminal.
- `--timeout <secs>`: bounds the wait time and requires `--once` (exit `2` without it). `0` is a deadline already past,
  so no notification is waited for and the verb exits `1` under `timeout`.
- Slow consumers receive `{"v":1,"event":"lag","dropped":N}` in-band events when events are dropped from the daemon ring
  buffer.

### `felis bridge`

Persistent JSON-lines helper on stdio for editors and non-Rust clients speaking the CLI contract. Its op set is closed
to session automation: the `sessions.*` ops, `notifications.subscribe`, and `cancel`. Three verb groups are CLI-only,
and a bridge client reaches them by running the point verb with `--format json`:

- operator verbs: `daemon status`, `daemon stop`, `config path|check|show-effective`, `doctor`, `version`;
- window launches: bare `felis`, `attach`, `frontend <name>`, `ssh`, `window retarget`;
- shell plumbing: `completions <shell>`, `__mangen`, `__complete-sessions`.

The stdio contract:

- Standard input: newline-delimited requests `{"v":1,"id":<id>,"op":"<op>","params":{...}}`. The only accepted top-level
  keys are `v`, `id`, `op`, and `params`; `params` is an object or may be omitted. Unknown operation parameters are
  refused rather than ignored. A line may contain at most `MAX_BRIDGE_LINE_BYTES` (8 × `MAX_PASTE_BYTES`, just under 128
  MiB); an over-limit line is discarded through its newline and answered as `invalid_request` with `id: null`.
- A request `id` is either a string of at most 128 UTF-8 bytes or a non-negative integer no greater than
  `9007199254740991` (the largest integer represented exactly by common JSON consumers). It remains in use until the
  request's reply or stream terminal is published as a complete stdout line.
- Standard output: responses, items, and terminal objects mirroring the CLI contract. At most 64 non-terminal objects
  may be waiting for stdout; each of the at most 64 in-flight operations reserves separate capacity for its terminal.
- Standard error: diagnostic log lines only. The request grammar is published as `$defs/request` of
  `felis-bridge-v1.schema.json` ("JSON Schema" above); a line that fails its shape is answered `malformed_request`, with
  two exceptions the grammar also states but the bridge charges to the request rather than to its spelling: the
  `rows`/`cols` pairing below, and `sessions.switch`'s required `from`. Both answer `invalid_request`.
- Closing stdin cleanly cancels in-flight operations and exits `0`. A stdin read failure exits `1`. A stdout write or
  flush failure stops every operation, closes every daemon link, and exits `1` without waiting for stdin EOF. A first
  `Ops` dial the daemon refuses `at_capacity` is answered with `id: null` before any line is read, and exits `1` as that
  kind does everywhere.
- One bridge admits at most 64 concurrent operations and 32 auxiliary daemon links in addition to its long-lived ops
  link. Each daemon stream queues at most 16 frames before applying backpressure to that link. Request shape, parameter
  types, cross-field constraints, and payload limits are validated before admission. A valid request beyond an operation
  or link limit is answered with `at_capacity` while the bridge continues serving admitted work. An auxiliary link holds
  its permit, and its operation its in-flight entry, until detach and carrier shutdown finish, not until the stream's
  terminal; terminal capacity is reserved at admission and held through the flush.

Supported ops and their accepted parameters:

| Op                                | Parameters                                        |
| --------------------------------- | ------------------------------------------------- |
| `sessions.list`                   | `tags`                                            |
| `sessions.info`                   | `session`                                         |
| `sessions.spawn`                  | `cwd`, `env`, `rows`, `cols`, `tags`, `cmd`       |
| `sessions.send`                   | `session`, `text`, `raw`                          |
| `sessions.kill`, `sessions.evict` | `session`                                         |
| `sessions.switch`                 | `to`, `from`, `attachment`                        |
| `sessions.tag`                    | `session`, `add`, `remove`                        |
| `sessions.capture`                | `session`, `source`, `ansi`, `lines`              |
| `sessions.search`                 | `session`, `pattern`, `regex`, `case_insensitive` |
| `notifications.subscribe`         | `session`                                         |
| `cancel`                          | `target` (an in-flight request id)                |

For a local daemon, a relative `sessions.spawn.params.cwd` is resolved against the bridge process's current directory,
and the child's base environment is captured from the bridge process, both before the request is sent. Over an SSH relay
the path passes through unchanged and no environment is captured. `rows` and `cols` are a pair, as they are on the flag:
one without the other is answered `invalid_request` by the bridge itself, and neither asks for the daemon's default grid
("Verb details"). A whole geometry travels at the wire's width, so one past the REQ-605a bounds is the daemon's refusal
to make and comes back `invalid_request` from there.

Three ops take less than their CLI verb, by policy rather than by a limit of the wire:

- `sessions.send` requires `text` and takes neither the CLI's `--key` chords nor its `--wait`. Both would be additive
  optional parameters within `v:1`.
- `notifications.subscribe` takes no `once` or `timeout`: `cancel` is how a bridge client ends a stream.
- `sessions.switch` requires `from`, which the CLI defaults from `$FELIS_SESSION_ID`, because the bridge runs inside an
  editor rather than a felis window. A switch that queued no window is a result carrying `"queued":0`, where the CLI
  verb fails with `not_queued`.

`sessions.capture.source` takes the four region names `sessions capture --source` takes, and
`sessions.switch.attachment` the decimal id `sessions info --format json` prints.

### `felis completions <shell>`

Generates a completion script for `bash`, `elvish`, `fish`, `powershell`, or `zsh`. The live session-id overlay is added
for `zsh` and `fish` only, where it queries the local daemon asynchronously; the other shells get the static grammar.
Running `felis completions` itself dials nothing: it prints a script built from this binary's own argument grammar, so
the global carrier flags are refused on this verb. That grammar carries the finite option vocabularies with it:
`--format` and `sessions capture --source` complete to their listed values in every generated script, as they list them
in `--help` and in the man page.

## Version reporting

`felis --version` prints local CLI binary build information without external calls. It reads no config file and dials no
daemon, so it refuses every global flag and every verb on the same line:

```
felis 0.1.0 (e3abf80c9d21)
```

`felis version` compares versions across the full installation:

```
cli    0.1.0 (e3abf80c9d21)
client 0.1.0 (e3abf80c9d21)
daemon 0.1.0 (aaaaaaaaaaaa-dirty)
```

JSON output (`--format json`):

```json
{
  "v": 1,
  "cli": { "version": "0.1.0", "revision": "e3abf80...", "dirty": false },
  "client": { "version": "0.1.0", "revision": "e3abf80...", "dirty": false },
  "client_status": "ok",
  "daemon": { "version": "0.1.0", "revision": "aaaaaaaa...", "dirty": true },
  "daemon_status": "ok"
}
```

Status fields: `client_status` (`ok`, `unavailable`, `unrecognized`); `daemon_status` (`ok`, `not_running`,
`at_capacity`, `incompatible`, `untyped`). `untyped` is a daemon whose `Welcome` carried no identity: the human table
prints "running, but the daemon reported no identity" and the JSON `daemon` key is `null`.

## Log files

Log output from `felis-client` and `felis-daemon serve` is teed to platform-specific user log files:

| Binary               | File         |
| -------------------- | ------------ |
| `felis-daemon serve` | `daemon.log` |
| `felis-client`       | `client.log` |

Log directory locations:

- macOS: `~/Library/Logs/felis/`
- Windows: `%LOCALAPPDATA%\felis\data\`
- Linux/other: `$XDG_STATE_HOME/felis/` (fallback `~/.local/state/felis/`)

Files exceeding 8 MiB are rotated to `<name>.old` at process startup. The console half of the tee is stderr for
`felis-daemon serve` and stdout for `felis-client`, whose stderr a `.app` launch does not keep; both halves are filtered
by `RUST_LOG` and the `--trace-perf` preset. A daemon the systemd user manager started writes its console to the journal
(`journalctl --user -u felis-daemon-<hash>`); a locally forked child's console is `/dev/null`, while one
`felis-daemon relay` forks inherits the relay's stderr, the SSH channel, until that link drops. The tee is the record
either way. A window launch whose hand-off was attempted and fell back to a forked daemon writes one `WARN` line naming
the reason to `client.log`; a launcher that never asked the manager (no manager, no `systemd-run`) says so at `DEBUG`.
Headless verbs open no log file: the attempted hand-off's `WARN` line reaches their stderr in `human` format, and the
`DEBUG` line needs `RUST_LOG`.
