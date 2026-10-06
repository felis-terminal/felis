---
title: Drive a session without a window
sidebar:
  order: 2
---

In [Your first session](first-session.md) you saw that a session outlives its window. This tutorial shows the other half
of the same idea: a session does not need a window _at all_. The felis daemon can hold a running shell that you create,
drive, and read entirely from the command line; the window is just one optional way to look at it.

By the end you will have built and steered a session without ever opening a window, then opened one at the very end to
see the result waiting for you.

Follow the steps in order. You need one terminal you already have open; we will call it your **launching terminal**.
Everything here runs there.

## Step 1: Create a session with no window

In your launching terminal, create a detached session running a shell, and keep its id in a variable:

```sh
ID=$(felis sessions spawn -- /bin/sh)
echo "$ID"
```

`spawn` starts the session in the daemon and prints its id; no window opens. If no daemon was running yet, `spawn`
starts one for you. The `echo` just shows you the id you captured.

## Step 2: Confirm it exists

```sh
felis sessions list
```

Your new session appears in the roster, even though nothing is on screen. It is a real shell, running, waiting for
input, just headless.

## Step 3: Send it some work

Type commands _into_ the session with `send`. `send` pastes its text **literally** and does not press Enter, so to run a
command add `--key enter`, which presses Enter once the paste ends:

```sh
felis sessions send "$ID" \
  'for i in 1 2 3; do echo "step $i of 3"; sleep 1; done; echo finished' --key enter
```

The command returns immediately; the work now runs inside the session. Give it a few seconds to finish:

```sh
sleep 4
```

A real script does not guess with `sleep`: with OSC 133 prompt marks
([Mark shell prompts](../how-to/mark-shell-prompts.md)), `send … --key enter --wait` blocks until the command finishes
and prints its exit code (see the [CLI reference](../reference/cli.md)).

## Step 4: Read its output, still no window

Ask the daemon what the session's screen looks like right now:

```sh
felis sessions capture "$ID"
```

You will see `step 1 of 3`, `step 2 of 3`, `step 3 of 3`, and `finished`: the output of a command you ran in a session
you have never looked at.

For a machine-readable version, add `--format jsonl` (one JSON object per row, then one terminal object saying the
stream ended); it is the stable interface scripts use:

```sh
felis sessions capture "$ID" --format jsonl | tail -n 5
```

## Step 5: Now open a window, last

Only now, attach a window to the session you built:

```sh
felis attach "$ID"
```

A window opens onto the same `/bin/sh`, with your `finished` line still on screen. The window did not create anything:
it gave you a view of a session that was already there. Close the window when you are done looking; the session keeps
running. Typing `exit` ends the `/bin/sh` you spawned, and with it the session: that is the one action that does.

## Step 6: Clean up

```sh
felis sessions kill "$ID"
```

## What you just learned

- **`felis sessions spawn -- <cmd>`** creates a running session with no window and prints its id.
- **`felis sessions send <id> '…' --key enter`** types into it and presses Enter; **`capture`** reads its screen back;
  no window required.
- **A window is an optional, late-added view.** The daemon is the thing that runs and holds your session; you can build,
  drive, and inspect one without ever opening a window, then attach one when you want to look.

## Where to go next

- [CLI reference](../reference/cli.md) — the full `felis sessions` verb set; for recipes built on it, see
  [Search and capture scrollback](../how-to/search-and-capture-scrollback.md),
  [Label sessions with tags](../how-to/label-sessions-with-tags.md), and
  [Reap dead or stale sessions](../how-to/reap-sessions.md).
- [Your first session](first-session.md) — if you skipped it, the survival half of the same model.
