---
title: Find sessions waiting on you
sidebar:
  order: 7
---

Find the session whose program is waiting for your answer, or wait for one to finish, using the status the program
reports itself. A program that speaks the OSC 7501 program-status protocol (support is experimental) tells felis whether
it is working, blocked on you, or done; felis keeps that per session and lists it in `felis sessions`, but shows nothing
in the window, so the steps below read it from the CLI. The fields and lifetimes are in
[program-status.md](../reference/protocols/program-status.md).

## List the sessions waiting on you

A `blocked` record is a program waiting on the user; `kind` says for what (`permission`, `question`, `auth`) and `msg`
why. Print each such session with its message:

```sh
felis sessions list --format json | jq -r '
  .sessions[] | select(any(.program_status[]; .state == "blocked"))
  | "\(.short_id) \(.title // "-") \([.program_status[] | select(.state == "blocked") | .msg // .kind] | join("; "))"'
```

Open a window on one with `felis attach <short_id>`.

The list is a snapshot: a status changes nothing on screen and sends no event. For a popup the moment a program blocks,
the program also has to send a notification ([Get notified when a job finishes](enable-notifications.md)).

## Read one session's status

`felis sessions info` lists one `status:` line per record the session holds; the first word is the record's `id`,
`(root)` when the program gave none, and `[<app>]`, when present, names the program:

```sh
felis sessions info <id>
```

```
status:   (root) blocked permission [mytool]: Allow network access?
status:   build working 40%
```

No `status:` line means no record is held: the program does not report, or its last `working` or `blocked` was dropped
at a prompt. To check that felis takes reports at all, run this in the session's shell and repeat the `info`; `done`
stays past the next prompt, where `working` would not:

```sh
printf '\033]7501;state=done:app=test\033\\'
```

## Wait for a session to stop working

Poll until the session holds at least one record and none of its records is `working`. The loop ends on `done`, `error`,
`idle`, or `blocked`, so check which before acting:

```sh
deadline=$((SECONDS + 600))
while :; do
  status=$(felis sessions info "$ID" --format json) || { rc=2; break; }   # session gone, daemon lost
  echo "$status" | jq -e '.program_status | length > 0 and all(.[]; .state != "working")' >/dev/null && { rc=0; break; }
  [ "$SECONDS" -ge "$deadline" ] && { rc=1; break; }
  sleep 2
done
echo "$status" | jq '.program_status'
```

`rc` is `0` when the program stopped working, `1` at the deadline, and `2` when the session or the daemon is gone.

## Report status from your own program

Write the sequence to the terminal. `title` and `msg` are base64; strip the line breaks `base64` adds:

```sh
printf '\033]7501;state=working:app=mytool\033\\'
printf '\033]7501;state=blocked:kind=question:app=mytool:msg=%s\033\\' \
  "$(printf %s 'Which branch?' | base64 | tr -d '\n')"
printf '\033]7501;state=done:app=mytool\033\\'
```

A program with several tasks gives each its own `id` (`id=build/test`), and `state=clear` with an `id` removes that
record and everything under it. To detect support, write `OSC 7501 ; ?` followed by a Device Attributes query (DA1,
`ESC [ c`), which every terminal answers: felis answers the status query first, and a terminal without support answers
only the DA1.

## When a status never goes away

felis drops `working` and `blocked` when the shell draws its next prompt or the session's program exits, and the prompt
needs shell marks ([Mark shell prompts](mark-shell-prompts.md)). Without them, a program interrupted before its last
report leaves its `working` or `blocked` in place. Clear it from the session's shell:

```sh
printf '\033]7501;state=clear\033\\'
```

A program you write avoids this by reporting `done`, `error`, or `clear` on every exit path, an interrupt included.
