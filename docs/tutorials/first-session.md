---
title: Your first session
sidebar:
  order: 1
---

By the end of this tutorial you will have started a command in felis, **closed its window while it kept running**, and
reattached to find it exactly where you left it. That survival is the one idea felis is built around, and the fastest
way to understand it is to watch it happen once.

Follow the steps in order. The tutorial assumes felis is already installed; if not, do [Install](../how-to/install.md)
first, then come back.

You will need about five minutes and one terminal you already have open (your normal shell, in any terminal emulator).
We will call that your **launching terminal**.

## Step 1: Open your first felis window

In your launching terminal, run:

```sh
felis
```

A felis window opens with a shell prompt inside it. The first time you run felis there is no daemon yet, so the client
quietly starts one for you; nothing to set up by hand.

Leave this window open and click into it. The rest of the setup happens inside the felis window.

## Step 2: Note the session's id

felis stamps every session's id into its own environment. Inside the felis window, run:

```sh
echo $FELIS_SESSION_ID
```

Copy the value it prints (an id like `3fa7c1d2…`). You will use a short unique prefix of it (say `3fa7`) in the last
steps; any unique prefix works.

## Step 3: Start something that keeps running

Start a counter so you have visible, moving state to check on later. Still inside the felis window, run:

```sh
i=0; while true; do echo "tick $i"; i=$((i + 1)); sleep 1; done
```

You will see `tick 0`, `tick 1`, `tick 2`, … appear once a second. Let it run. Note the number it has reached; you will
see it climb past that in a moment.

## Step 4: Close the window

Now close the felis **window**: use the window's close button or your window manager's close on Linux; on macOS, ⌘Q
(each felis window is its own client process, so quitting the client closes just this window). The counter is still
counting; you just stopped _looking_ at it.

Here is why: the window you closed was only a view. The shell, the counter, and the scrollback all live in the felis
**daemon**, a separate process that keeps running. Closing a view does not end the work behind it.

Your launching terminal gets its prompt back once the window closes.

## Step 5: Confirm the session is still alive

Back in your launching terminal, list your sessions:

```sh
felis sessions list
```

Your session is still there: the daemon kept it. Peek at what it is doing right now, using your id prefix from Step 2:

```sh
felis sessions capture 3fa7 --source scrollback | tail
```

The last lines show the counter has kept ticking _while the window was closed_: the number is higher than where you left
it.

## Step 6: Bring it back

Reopen the session in a fresh window:

```sh
felis attach 3fa7
```

A new window appears with the same shell, and the counter is still running, now well past the number from Step 3.
Nothing was lost, and nothing was restarted; you reconnected to work that never stopped.

Press `Ctrl-C` in this window to stop the counter.

## Step 7: Clean up

You now have a session you do not need. Remove it:

```sh
felis sessions kill 3fa7
```

`felis sessions list` omits it.

## What you just learned

- **`felis`** opens a window on a fresh session, starting the daemon if needed.
- **The window is a view; the daemon holds the session.** Closing the window leaves your shell, programs, and scrollback
  running.
- **`felis attach <id>`** reconnects a window to a running session, from the same machine or another one.
- **`felis sessions …`** drives sessions without a window: here `list`, `capture`, and `kill`.

## Where to go next

- [Drive a session without a window](drive-without-a-window.md) — the other half of the same idea: build and steer a
  session that never had a window at all.
- [CLI reference](../reference/cli.md) — the full `felis sessions` verb set for driving sessions from scripts.
- [Configuration](../reference/config.md) — set your font, theme, and keybindings.
- [Vision](../explanation/vision.md) — why felis draws the line here, and why the daemon is built in rather than bolted
  on.
