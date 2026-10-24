---
name: felis
description:
  Drive a felis terminal session from a coding agent — run a long build or test in a detached session and collect its
  exit code and output, steer a REPL or TUI with keystrokes and read the screen back, or wait for another session (a TUI
  agent, a build) to finish. Use when a command would outlive the agent's own shell tool, needs a real PTY, or must be
  watched from outside. The interface is `felis sessions` with `--format json` / `--format jsonl`; not for editor
  plugins, keymap scripts, or config debugging.
allowed-tools: Bash
---

# Driving felis sessions

felis keeps session state in a daemon, so a session runs with no window attached and the `felis` binary is a plain
automation target: `spawn` to create, `send` to drive, `capture` / `search` to read, `kill` to clean up. There is no
multiplexer and no scripting language inside felis.

This skill covers the three things a coding agent does with it:

1. **Run and check**: a build, test, or `nix build` in a detached session, then its exit code and output.
2. **Drive**: a REPL, debugger, or TUI, sending keys and reading the screen.
3. **Watch**: wait for a session someone else started (a full-screen TUI agent, a long command) to finish or go idle.

This file holds the contract and the core workflow; "Where to read next" maps the rest to reference files, loaded only
when a task reaches them.

## The contract

**`--format` is the interface, and the value follows the verb's class.** Point verbs (`list`, `info`, `spawn`, `send`,
`kill`, `evict`, `tag`) take `json` and print one object on stdout. Stream verbs (`capture`, `search`,
`notifications subscribe`) take `jsonl`: one object per line, then exactly one terminal,
`{"v":1,"event":"end","count":N}` or `{"v":1,"event":"error","error":{…}}`, even when the stream fails before its first
item. Never infer the end from EOF; drop the terminal with `select(.event | not)` in `jq`. Asking a verb for the other
format is a usage error (exit `2`). The default `human` output is not a parse target, and there is no `--json` flag.

Every object carries `"v":1`. Fields may be added within an epoch; an existing field is never renamed or retyped without
bumping `v`, so a parser keyed on `v1` shapes keeps working across a felis upgrade.

Every shape is published as JSON Schema (draft 2020-12): [`felis-cli-v1.schema.json`][cli-schema] for these objects,
[`felis-bridge-v1.schema.json`][bridge-schema] for the bridge's requests and replies. Payload objects stay open and
`error.kind` is an open string, so a validator built from them does not reject a newer field or a newer token.

[cli-schema]:
  https://raw.githubusercontent.com/felis-terminal/felis/main/crates/felis-cli/schemas/felis-cli-v1.schema.json
[bridge-schema]:
  https://raw.githubusercontent.com/felis-terminal/felis/main/crates/felis-cli/schemas/felis-bridge-v1.schema.json

**A point verb's failure is one object on stderr**, `{"v":1,"error":{"kind":"no_match","message":"…"}}`, and stdout
stays empty. Branch on `kind`, never on `message`; the `kind` vocabulary grows additively, so treat an unknown one as a
generic failure and read the exit code. A machine format also silences felis's own log lines (`RUST_LOG` restores them).
What the argument parser refuses (missing argument, bad flag pair) is a human line on stderr at exit `2`, before any
framing is chosen.

**Exit codes are a function of `error.kind`.** `0` success · `1` a typed refusal, meaning the operation ran to an answer
(no such session, a request the daemon refused, a ceiling, a wait that elapsed), or an empty answer with no error object
(`sessions search` with no match, `notifications subscribe --once` whose stream ended first) · `2` a failure to ask at
all (bad invocation, unreachable daemon, lost connection, protocol break). "0 sessions" (`0`) and "no daemon" (`2`)
never blur.

**Each invocation does one operation and exits**, and read/drive verbs never auto-spawn a daemon: an absent daemon is
exit `2`, a real bug to surface. Only `spawn` and the window launches (`felis`, `felis attach`) auto-spawn, and they do
it on whichever host they dial, `--host` included.

**Store `id`, display `short_id`.** An id is 32 hex digits. `list` and `info` add `short_id`, the shortest unique prefix
(at least 8 digits): against the roster carried in the same reply on `list`, against the daemon's pool at resolution
time on `info`. It lengthens as the roster grows, so never persist it. Every id slot accepts the full id or any unique
prefix. The mutating verbs see no roster, so their replies carry `id` alone; feed that to the next verb.

## Core workflow

```sh
# 1. Create a detached session; capture its id. A local spawn starts in
#    YOUR cwd (--cwd <dir> overrides; relative resolves against your
#    cwd). A --host spawn lets the remote daemon pick, and any --cwd
#    must be absolute on the remote. The grid is the daemon's default
#    24x80 unless you name it, and --rows/--cols travel as a pair:
#    naming one alone is a usage error (exit 2), and the bridge's
#    sessions.spawn answers half a geometry with invalid_request.
#    A local child inherits YOUR environment; --env adds or overrides
#    but cannot remove, so drop a variable with env -u (it matters when
#    you drive a copy of yourself and inherit your own session markers).
#    Bare spawn runs the daemon's default shell ($SHELL, else /bin/sh
#    or powershell.exe; never the config's shell.command, which only
#    windows read), with the user's rc files and so their shell
#    integration; `-- <cmd>` runs a program instead (a REPL, a TUI),
#    and a shell named there may lack the integration `send --wait`
#    needs.
ID=$(felis sessions spawn)
ID=$(felis sessions spawn --rows 50 --cols 132)
ID=$(env -u SOME_PARENT_MARKER felis sessions spawn --env KEY=VAL)
ID=$(felis sessions spawn -- python3)

# 2. Drive it. `send` pastes text literally (bracketed-paste wrapped
#    when the shell sets ?2004) and does NOT press Enter: a newline in
#    TEXT is inserted, not run. `--key enter` travels as a separate key
#    frame after the paste. `-` reads the payload from stdin. Under
#    ?2004 the paste drops control bytes other than tab, LF and CR;
#    send those with --key or --raw.
felis sessions send "$ID" 'cargo build --release' --key enter  # paste + Enter
felis sessions send "$ID" --key ctrl+c                      # named key (preferred)
printf '\003' | felis sessions send "$ID" --raw -           # same, hand-encoded

#    For a TUI (arrows, Escape, Tab, F-keys) use --key with the keymap
#    chord grammar: repeatable, pressed in order. The daemon encodes
#    each one against the session's CURRENT keyboard modes (Kitty
#    flags, DECCKM…), which you cannot see from outside. Prefer it
#    over --raw escapes.
felis sessions send "$ID" --key down --key down --key enter
#    A mouse-aware TUI also takes clicks at 0-based cells, the same
#    numbers `search --source visible` returns in col_spans:
felis sessions send "$ID" --mouse click:12,4

#    Run-and-check: --wait blocks until the command's OSC 133 D mark and
#    prints its exit code. Race-free (the subscription starts before the
#    input). Needs a shell that emits OSC 133 (zsh: source the shipped
#    share/felis/shell-integration/felis.zsh); always pass --timeout so a
#    mark-less session cannot hang you. CLI exits 0 once seen, 1 on timeout/session end.
felis sessions send "$ID" 'nix build .#foo' --key enter --wait --timeout 600
#    (--format json gives {"v":1,"id":…,"exit_code":N})
felis sessions capture "$ID" --source command-output --format jsonl
#    → rows + terminal {"v":1,"event":"end","count":N,"exit_code":M}

#    To watch a command someone else started, drop the text: --wait
#    alone blocks for the next D mark after it attaches.
felis sessions send "$ID" --wait --timeout 600 --format json

# 3. Read it back. Default region is the visible grid. --lines N tails
#    any source to its last N rows (row indices are NOT renumbered);
#    use it to keep long captures out of your context.
felis sessions capture "$ID" --format jsonl
felis sessions capture "$ID" --source scrollback --lines 50
felis sessions capture "$ID" --source last-command        # OSC 133 B→D range

# 4. Search scrollback (substring; --regex for regex). --source visible
#    searches only the live screen, a fullscreen TUI's included.
felis sessions search "$ID" panic --case-insensitive --format jsonl
felis sessions search "$ID" 'Save changes' --source visible --format jsonl

# 5. Clean up.
felis sessions kill "$ID"
```

Capture rows are `{"v":1,"row":N,"text":"…","soft_wrap_continued":bool}`; scrollback rows use negative `row` indices
(oldest first), and for `command-output` / `last-command` the `row` is 0-based within the mark range. Search matches are
`{"v":1,"line_index":N,"text":"…","byte_spans":[[s,e]…],"col_spans":[[row,start_col,end_col]…]}`, columns 0-based.
`--ansi` composes with `--format jsonl`: each row gains an `ansi` field while `text` stays plain. The mark-range sources
add `exit_code` to the terminal when the closing D mark carried one.

## Where to read next

Load a reference file only when the task reaches it:

| When                                                                     | Read                                                             |
| ------------------------------------------------------------------------ | ---------------------------------------------------------------- |
| Listing, tagging, or waiting on a session someone else started           | [references/watching.md](references/watching.md)                 |
| A verb exits `1` or `2` and the `kind` does not say what to do           | [references/troubleshooting.md](references/troubleshooting.md)   |
| Clicking on text, reading a fullscreen TUI, image placeholders           | [references/reading-the-grid.md](references/reading-the-grid.md) |
| `--host`, or `felis` inside a session reaching an unexpected daemon      | [references/remote.md](references/remote.md)                     |
| The bridge, window moves, keymap chords, `doctor` / `config` diagnostics | [references/other-surfaces.md](references/other-surfaces.md)     |

For a flag not shown here, `felis <verb> --help` and `man felis-<verb>` (for example `man felis-sessions-send`) ship
with every install and work offline. The full contract is the
[CLI reference](https://github.com/felis-terminal/felis/blob/main/docs/reference/cli.md).

## What felis deliberately does not do

- **No content heuristics** (URL detection, prompt parsing, output classification). You get the raw grid and the typed
  status fields; derive the rest in your own annotator.
- **No session groups, broadcast, or input fan-out.** Tags are labels for your picker.
- **No in-felis scripting, eval verb, or hook.** The typed IPC verbs are the whole extension surface.
- **felis is the data source; your tool is the annotator.** Layout, colors, and derived fields live in your `jq` / `fzf`
  / agent layer.
