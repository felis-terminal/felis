---
title: Search and capture scrollback
sidebar:
  order: 6
---

Search a session's retained scrollback and pull its grid out as text or JSON, without opening a window or selecting
anything by hand. Parameter details are documented in [cli.md](../reference/cli.md).

## Search session scrollback

Search for a pattern across an active session:

```sh
felis sessions list
felis sessions search abc1 panic --case-insensitive
```

`felis sessions search` returns exit code `0` when at least one match is found and `1` on no matches, so it works
directly in shell conditionals.

## Snapshot session state

`felis sessions capture` dumps grid cell contents, while `felis sessions info` inspects session metadata. Use `--format`
for stable machine-readable output:

```sh
ID=abc1   # any unique ID prefix from felis sessions list
felis sessions capture "$ID" --format jsonl > grid-state.jsonl
felis sessions info    "$ID" --format json  > session-info.json
```

Pass `--source scrollback` for retained history, or `--source last-command` or `--source command-output` for output
bounded by OSC 133 prompt marks ([cli.md](../reference/cli.md)).

Restrict output to the most recent lines of a buffer with `--lines N`:

```sh
felis sessions capture "$ID" --source scrollback --lines 50
```

## Preview sessions interactively with fzf

`--ansi` reconstructs SGR color and style escape sequences for each cell for formatted terminal rendering in tools like
`fzf`:

```sh
felis sessions list --format json \
  | jq -r '.sessions[].id' \
  | fzf --ansi --preview 'felis sessions capture {} --ansi'
```

## Open the pager where the window was looking

To open the pager where the window was looking, pass the anchor to the editor:

```toml
[keymap]
# nvim as the pager, opened at the top of the window viewport.
"ctrl+shift+h" = { kind = "pipe", source = "scrollback", ansi = true, target = { command = [
  "nvim", "-c", "execute 'normal! ' . $FELIS_INPUT_LINE_NUMBER . 'zt'",
] } }
```

The variables are [keybindings.md](../reference/keybindings.md) "Viewport anchor variables".
