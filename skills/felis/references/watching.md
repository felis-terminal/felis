# Watching sessions and waiting for status

Scenario 3: find the sessions on a daemon, label them, and block until one finishes or waits on input.

## Contents

- Discovering and labeling sessions
- Status fields for a picker
- Waiting for status: pick by producer

## Discovering and labeling sessions

```sh
felis sessions list --format json          # {"v":1,"sessions":[…]}
felis sessions info "$ID" --format json    # geometry, idle, title, cwd,
                                           # spawn_cwd, foreground, last_notification,
                                           # tags, last_exit_code, attachments,
                                           # program_status
felis sessions tag "$ID" agent work        # opaque labels; idempotent
felis sessions tag "$ID" done --remove agent   # add + remove atomically
felis sessions spawn --tag agent -- …      # label at creation, no untagged gap
felis sessions list --tag agent --format json  # client-side filter
```

Tags are bounded in count per session and in bytes per tag; an over-cap delta is refused whole at exit `1`, with a
message naming the cap.

`cwd` is the directory the program last announced with OSC 7 and is absent without shell integration; `spawn_cwd` is the
directory the daemon started the child in, fixed at creation, so group sessions by it rather than storing a path in a
tag.

A row with `"exited":true` is a corpse in the post-exit grace: its shell is gone and the daemon reaps it seconds after
the last detach. Never route new work at it (`.sessions[] | select(.exited != true)`).

## Status fields for a picker

Two status fields exist for an external picker to annotate with: `foreground`, the program in the terminal foreground
resolved from the PTY foreground pgid (not content sniffing), and `last_notification`, the most recent OSC 9/99/777,
which is the "blocked / done" signal a full-screen TUI agent emits. `last_exit_code` is the youngest OSC 133 D mark and
needs shell integration.

`program_status` (experimental) is the session's OSC 7501 records, for producers that report their own state:
`[{id, state, kind?, progress?, app?, title?, msg?}]`, `id` `""` the root record. `state` is `idle`, `working`, `done`,
`blocked` or `error`; `blocked` with `kind` (`permission`, `question`, `auth`) is the "waiting on the user" signal, and
`msg` says why. Treat a `state` or `kind` you do not know as unknown. A new shell prompt (with shell integration) or the
session's exit drops `working` and `blocked`; `done` and `error` stay until the program replaces them. A row holds the
latest set only; `sessions events --kind program_status` streams every change.

```sh
felis sessions list --format json | jq '.sessions[] | select(any(.program_status[]; .state == "blocked")) | .id'
```

`terminal_progress` is the last ConEmu OSC 9;4 progress a program in the session reported: `{state, value?}`, `state`
`normal`, `error`, `indeterminate` or `paused`, `value` `0` to `100`; absent when none is held. `error` means failed so
far, not stopped: the build may still be running. A new shell prompt (with shell integration) clears it; without one, a
killed producer's last report stays. It is live progress, not an outcome, so take the result from `send --wait`'s exit
code. cargo sends it only with `term.progress.term-integration = true`.

## Waiting for status: pick by producer

```sh
felis notifications subscribe --format jsonl   # OSC 9/99/777 from every session
felis notifications subscribe --session "$ID" --once --timeout 600 --format jsonl
                                               # block for THAT session's next one
```

Items are `{"v":1,"session_id","title","body","urgency","notification_id",` `"session_title","cwd","attached"}`, then
the stream's one terminal (`--once` writes one too). A consumer that falls behind sees
`{"v":1,"event":"lag","dropped":N}`; the dropped events may include the one it waited for. `--once` exits `1` on
`--timeout`, on a clean end (daemon shutdown), or on an unresolvable `--session` prefix, and `2` if the daemon dropped
the connection without ending the stream.

A full-screen TUI agent (Claude Code etc.) emits no OSC 133 marks, so `send --wait` never fires for it;
`--session --once` is its completion signal only when it is configured to emit OSC 9/99/777. A plain build emits no OSC
9, so `send --wait` is the tool there.

`sessions events` streams what happens to sessions instead of a poll. `--kind` is required; the kinds and their items
are in `felis sessions events --help` and
[cli.md "Verb details"](https://github.com/felis-terminal/felis/blob/main/docs/reference/cli.md#verb-details). Items are
hints, not a mirror: start the stream, then `sessions list`, and list again after a `lag` event.

A producer that reports OSC 7501 status is the cheapest to wait on. The stream never ends by itself, and a pipe into
`head` would not end it either, so read it from a FIFO and kill it on the first match or at the deadline:

```sh
dir=$(mktemp -d) && mkfifo "$dir/events"
felis sessions events --session "$ID" --kind program_status,lifecycle --format jsonl >"$dir/events" &
stream=$!
(sleep 600 && kill "$stream") >/dev/null 2>&1 &   # the deadline
watchdog=$!
jq -cen 'first(inputs | select(.event == "error" or .type == "removed" or (.type == "program_status"
         and (.records | length > 0) and all(.records[]; .state != "working"))))' <"$dir/events"
rc=$?                              # 0 printed the item, 4 the deadline passed or the stream ended
kill "$stream" "$watchdog" 2>/dev/null; rm -r "$dir"
```

The item printed is the first set with no `working` record, the session's removal, or the stream's error terminal. A set
without `working` may be `blocked`, which means the producer waits on input, so read the states before acting. An error
of kind `unsupported` is a daemon too old for `sessions events`: poll `sessions info` for `program_status` instead.

When a producer emits none of these, poll the visible grid against a deadline: `search --source visible` exits `0` on a
match and `1` on none. Retry only on `1`; a `2` (a daemon too old for `--source visible`, a lost connection) does not
clear by waiting, so stop and report it. The check sees only what is on screen now, so anchor the pattern on a row your
own input cannot produce (a status line, a line-anchored regex), or the echo of your prompt satisfies it before the
reply exists:

```sh
tries=300                          # 2 s apart: a 10-minute deadline
while :; do
  felis sessions search "$ID" '^● Done' --source visible --regex >/dev/null
  rc=$?                            # 0 matched, 1 not yet, 2 failed to ask
  [ "$rc" -ne 1 ] && break
  tries=$((tries - 1)); [ "$tries" -le 0 ] && break
  sleep 2
done
```
