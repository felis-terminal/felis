---
title: Do your tmux workflows without tmux
sidebar:
  order: 3
---

felis has no multiplexer: no tabs, splits, or prefix key. Window layout belongs to the system window manager or
compositor, while the felis daemon handles process persistence and headless automation. Features out of scope are
cataloged in [non-goals](../explanation/non-goals.md). Map your everyday tmux operations to felis and window manager
capabilities as shown below.

## Command map

| tmux workflow                         | felis approach                                                                                                                                             |
| ------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `tmux` / `tmux new`                   | `felis` opens a window on a new session                                                                                                                    |
| (start work with no window)           | `felis sessions spawn -- /bin/sh` starts a detached session and prints its ID ([Drive a session without a window](../tutorials/drive-without-a-window.md)) |
| `tmux attach` / `tmux a -t NAME`      | `felis attach <id>`                                                                                                                                        |
| `tmux ls`                             | `felis sessions list` (add `--format json` for scripts)                                                                                                    |
| `C-b d` (detach)                      | Close the window; the session keeps running. Or run `felis sessions evict <id>` to disconnect every window                                                 |
| `tmux kill-session -t NAME`           | `felis sessions kill <id>` (bulk reaps: [Reap dead or stale sessions](reap-sessions.md))                                                                   |
| `tmux switch-client` / `choose-tree`  | `felis sessions switch <id>`, or bind the `switch_session` chords (below)                                                                                  |
| `tmux send-keys -t X 'cmd' Enter`     | `felis sessions send <id> 'cmd' --key enter` (see [Drive a session without a window](../tutorials/drive-without-a-window.md))                              |
| `tmux send-keys -t X Up Escape C-c`   | `felis sessions send <id> --key up --key escape --key ctrl+c`                                                                                              |
| `tmux wait-for` + shell-hook plumbing | `felis sessions send <id> 'cmd' --key enter --wait` blocks and prints the command's exit code                                                              |
| `tmux capture-pane -p`                | `felis sessions capture <id>` ([Search and capture scrollback](search-and-capture-scrollback.md))                                                          |
| Session names                         | Session IDs plus opaque `felis sessions tag <id> <label>` ([Label sessions with tags](label-sessions-with-tags.md))                                        |
| `C-b [` copy mode / scroll            | Native scrollback: `Shift+Page_Up`, search with `Ctrl+Shift+F` (`Cmd+F` on macOS)                                                                          |
| tmux over SSH on remote box           | `felis --host user@remote` ([Attach a session over SSH](attach-over-ssh.md))                                                                               |

## Window manager responsibilities

Your window manager or compositor manages geometry and tiling:

| tmux feature                   | Window manager equivalent                                                         |
| ------------------------------ | --------------------------------------------------------------------------------- |
| `C-b c` (new window)           | Bind `new_session` for an in-window swap, or open another felis window in your WM |
| `C-b %` / `C-b "` (split pane) | Tile two felis windows with your window manager or compositor                     |
| `C-b z` (zoom pane)            | Your WM's fullscreen or monocle toggle                                            |
| Resize panes                   | Resize windows in your WM                                                         |

On Linux every felis window carries the Wayland app ID `felis` and the X11 `WM_CLASS` `felis`, `felis`, so a window rule
matching that name applies to all of them.

## Optional: tab-like session switching in one window

Session actions ship unbound to allow custom chord bindings. Bind them in your configuration to enable tab-like session
navigation without multiplexer tabs:

```toml
[keymap]
"ctrl+shift+]" = { kind = "switch_session", to = "next" }
"ctrl+shift+[" = { kind = "switch_session", to = "previous" }
"ctrl+shift+n" = { kind = "new_session" }
```

`switch_session` cycles the current window through active sessions in creation order without ending them, and skips
sessions whose shell has exited. `new_session` creates a session and switches the window to it. To pick interactively,
filter sessions in the shell:

```sh
felis sessions switch "$(felis sessions list --format json \
    | jq -c '.sessions[]' \
    | fzf | jq -r .id)"              # pick interactively in the shell
```

Bind the `run` action to invoke that picker on a chord, or execute it from any shell prompt. See
[keybindings.md](../reference/keybindings.md) for the action specification.
