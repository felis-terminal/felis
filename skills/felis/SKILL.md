---
name: felis
description:
  Drive a felis terminal session from a coding agent — run a long build or test in a detached session and collect its
  exit code and output, steer a REPL or TUI with keystrokes and read the screen back, or wait for another session (a TUI
  agent, a build) to finish. Use when a command would outlive the agent's own shell tool, needs a real PTY, or must be
  watched from outside. The interface is `felis sessions` with `--format json` / `--format jsonl`; not for editor
  plugins, keymap scripts, or config debugging (see `docs/reference/cli.md`).
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

The contract is `docs/reference/cli.md` (task recipes in `docs/how-to/`); read it before relying on a flag not shown
here. The surfaces an agent rarely reaches (editor bridge, window moves, keymap scripts, config and GUI diagnostics) are
listed at the end with pointers only.

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
ID=$(felis sessions spawn -- /bin/zsh)
ID=$(felis sessions spawn --rows 50 --cols 132 -- /bin/zsh)

# 2. Drive it. `send` pastes text literally (bracketed-paste wrapped
#    when the shell sets ?2004) and does NOT press Enter: a newline in
#    TEXT is inserted, not run. `--key enter` travels as a separate key
#    frame after the paste. `-` reads the payload from stdin.
felis sessions send "$ID" 'cargo build --release' --key enter  # paste + Enter
felis sessions send "$ID" --key ctrl+c                      # named key (preferred)
printf '\003' | felis sessions send "$ID" --raw -           # same, hand-encoded

#    For a TUI (arrows, Escape, Tab, F-keys) use --key with the keymap
#    chord grammar: repeatable, pressed in order. The daemon encodes
#    each one against the session's CURRENT keyboard modes (Kitty
#    flags, DECCKM…), which you cannot see from outside. Prefer it
#    over --raw escapes.
felis sessions send "$ID" --key down --key down --key enter

#    Run-and-check: --wait blocks until the command's OSC 133 D mark and
#    prints its exit code. Race-free (the subscription starts before the
#    input). Needs a shell with OSC 133 integration (fish/zsh/bash
#    integrations emit it); always pass --timeout so a mark-less session
#    cannot hang you. CLI exits 0 once seen, 1 on timeout/session end.
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

# 4. Search scrollback (substring; --regex for regex).
felis sessions search "$ID" panic --case-insensitive --format jsonl

# 5. Clean up.
felis sessions kill "$ID"
```

Capture rows are `{"v":1,"row":N,"text":"…","soft_wrap_continued":bool}`; scrollback rows use negative `row` indices
(oldest first), and for `command-output` / `last-command` the `row` is 0-based within the mark range. Search matches are
`{"v":1,"line_index":N,"text":"…"}`. `--ansi` composes with `--format jsonl`: each row gains an `ansi` field while
`text` stays plain. The mark-range sources add `exit_code` to the terminal when the closing D mark carried one.

### Payload limits

An over-limit request is refused before it reaches the daemon, exit `1` with `{"error":{"kind":"invalid_request",…}}`.
Chunk rather than raise.

| Verb              | Bound                                                                                                        |
| ----------------- | ------------------------------------------------------------------------------------------------------------ |
| `sessions send`   | 16 MiB − 64 of text; 16 MiB with `--raw`                                                                     |
| `sessions search` | 4 KiB of pattern                                                                                             |
| `sessions spawn`  | 4 KiB each for the program and `--cwd`; 4096 argv entries / 1 MiB of argv; 4096 `--env` pairs / 1 MiB of env |

## Discovering and labeling sessions

```sh
felis sessions list --format json          # {"v":1,"sessions":[…]}
felis sessions info "$ID" --format json    # geometry, idle, title, cwd,
                                           # foreground, last_notification,
                                           # tags, last_exit_code, attachments
felis sessions tag "$ID" agent work        # opaque labels; idempotent
felis sessions tag "$ID" done --remove agent   # add + remove atomically
felis sessions spawn --tag agent -- …      # label at creation, no untagged gap
felis sessions list --tag agent --format json  # client-side filter
```

Tags are bounded (≤32 per session, ≤128 bytes each); an over-cap delta is refused whole at exit `1`.

Two status fields exist for an external picker to annotate with: `foreground`, the program in the terminal foreground
resolved from the PTY foreground pgid (not content sniffing), and `last_notification`, the most recent OSC 9/99/777,
which is the "blocked / done" signal a full-screen TUI agent emits. `last_exit_code` is the youngest OSC 133 D mark and
needs shell integration.

A row with `"exited":true` is a corpse in the post-exit grace: its shell is gone and the daemon reaps it seconds after
the last detach. Never route new work at it (`.sessions[] | select(.exited != true)`).

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
`--session --once` is its completion signal. A plain build emits no OSC 9, so `send --wait` is the tool there.

## When something fails

**`spawn` refused, `send` goes nowhere: `felis daemon status --format json`.** One object listing every admitted
resource as `{"resource","unit",` `"scope","total_used","max_subject_used","per_subject_limit",` `"global_limit"}`;
absent fields mean no value, so read with `.field // empty`. Two rows matter to an agent. A spawn past the `sessions`
cap (256) or a connect past the `connections` cap (1024) is `{"error":{"kind":"at_capacity"}}` at exit `1`: reap or
close and retry rather than treating it as a broken invocation. A `pty_input_bytes` row near its per-subject limit means
the child stopped reading its stdin; `send` then blocks until admitted, and `--wait --timeout N` bounds that wait too.
The verb exits `2` for an unreachable daemon and never starts one.

**Spawn refused with `invalid_request` naming a draining daemon: that daemon is going away.** `"draining": true` in
`daemon status` says the same thing. No reap makes room, so retrying against it never succeeds; start the session on
another daemon (`--socket` / `--host`).

**Ending a daemon you started: `felis daemon stop`.** Point verb, `--format json`, never autospawns. A bare stop refuses
while anything is running (exit `1`, `{"error":{"kind":"refused","sessions":N}}`), so it cannot destroy an agent's work
by accident. `--force` destroys every session and stops; `--when-empty` refuses new sessions and stops after the last
one ends, answering `{"outcome":"draining","mode":"when_empty", "sessions":N}`. A stop that goes through answers
`{"outcome":"stopping","mode":…}` at exit `0`; exit `2` means the daemon is unreachable or too old to answer.

On Linux a daemon started by a client running under the systemd user manager is a transient unit,
`felis-daemon-<hash>.service` in `app.slice`; `systemctl --user stop` on it is refused, so the verb above is the only
way down (`docs/reference/cli.md` "Auto-spawning").

**Stale daemon: `felis version --format json`.** `cli`, `client`, `daemon`, each `{"version","revision","dirty"}`;
`client` and `daemon` are `null` unless their `*_status` is `ok`. The daemon outlives its windows, so compare
`daemon.revision` against `cli.revision`. `felis --version` reports only this binary and dials nothing, so it refuses
`--config`, `--host`, `--socket`, and `--ssh-arg` at exit `2` rather than ignoring them (the per-verb placement matrix
is `docs/reference/cli.md` "Global options").

## Local or remote, same verbs

`felis --host user@remote sessions …` runs every verb over SSH (`ssh <host> felis-daemon relay`) with identical ids,
exit codes, and shapes, auto-spawn included: `--host … sessions spawn` starts a cold remote daemon exactly as a local
`spawn` does, while every read/drive verb runs `relay --no-spawn` and exits `2` instead. `--host` and `--socket` are
mutually exclusive; a relative `--socket` path is anchored to the caller's working directory; `--ssh-arg <TOKEN>`
(repeatable, needs `--host`) splices verbatim tokens before the destination.

The local daemon is resolved as `--socket`, then `$FELIS_SOCKET`, then the platform default, which on Unix is
`/tmp/felis.<uid>/daemon.sock`, derived from the uid alone. No environment variable takes part, so a desktop login, an
SSH login and a relay all reach one daemon. A socket named by `--socket` or `$FELIS_SOCKET` must sit in a dedicated
directory you own with mode `0700`, and the daemon refuses to start otherwise. Every daemon stamps `FELIS_SOCKET` into
its children, so `felis` inside a session reaches the daemon holding it; run `env -u FELIS_SOCKET felis …` to reach the
default endpoint. Do not set `FELIS_SOCKET` yourself; `sessions spawn --env` refuses it.

A session spawned over `--host` inherits the remote login's environment, and its `SSH_AUTH_SOCK` is a stable
daemon-owned path, so agent forwarding survives the SSH connection that opened it. The endpoint outlives the login, so
the only thing that ends a remote daemon short of a reboot is the systemd user manager stopping one it started for a
non-lingering user.

## Surfaces this skill does not cover

Each is documented in `docs/reference/cli.md` under the named section; none is needed for the three scenarios above.

- **`felis bridge`**: one long-lived JSON-lines process for an editor plugin that would otherwise pay a process launch
  per keystroke ("Other verbs"; protocol in `docs/reference/ipc.md`, "Non-Rust clients: the stdio bridge"). Same `kind`
  vocabulary as the CLI, failures on stdout, no `--wait` and no `--key`. Its ops are session automation only
  (`sessions.*`, `notifications.subscribe`, `cancel`): the operator verbs (`daemon`, `config`, `doctor`, `version`), the
  window launches, and the shell plumbing (`completions`, `__mangen`, `__complete-sessions`) are CLI-only, run as point
  verbs with `--format json`. It admits 64 concurrent operations and 32 auxiliary links, bounds stream and stdout
  queues, rejects unknown fields and parameters (the grammar is `$defs/request` of `felis-bridge-v1.schema.json`), and
  exits `1` immediately if stdout fails. A local relative `sessions.spawn` `cwd` is anchored at the bridge process's
  directory.
- **`sessions switch`, `felis ssh <dest>`, `felis window retarget [<socket>]`**: move a live window to another session
  or daemon. They act on windows, which an agent driving a headless session has none of ("Session verbs", "Re-point a
  window across daemons"). `felis ssh devbox` re-points the window it runs in at devbox's daemon; `felis --host devbox`
  opens a new window there instead, and before a headless verb the same flag names the daemon that verb dials.
- **`pipe` / `run` keymap chords** and their `FELIS_ORIGIN_SESSION_ID` / `FELIS_HOST` / `FELIS_CWD` environment:
  user-authored scripts fired from a window's `[keymap]` (`docs/reference/keybindings.md`, "Unbound by default").
- **`felis doctor`, `felis config check`, `felis frontend`**: GUI and config diagnostics for a human at the desk
  ("Doctor", "Config verbs", "Alternate frontends"). The one exception worth reaching for: when a roster looks empty
  where you expected sessions, `felis doctor --format json` on that host carries a `daemon-sibling` row when this
  shell's `$FELIS_SOCKET` names an endpoint other than the default; the row says what the default holds, and when a
  felis daemon answers there its `detail` names the path to pass to `felis --socket <path> sessions list`. `doctor`,
  `config check`, and `config show-effective` are Point-diagnostic verbs: exit `1` means the document or a check
  reported diagnostics. `doctor` and `config check` still write their result object; `config show-effective` writes an
  `invalid_request` error object instead.

## What felis deliberately does not do

- **No content heuristics** (URL detection, prompt parsing, output classification). You get the raw grid and the typed
  status fields; derive the rest in your own annotator.
- **No session groups, broadcast, or input fan-out.** Tags are labels for your picker.
- **No in-felis scripting, eval verb, or hook.** The typed IPC verbs are the whole extension surface.
- **felis is the data source; your tool is the annotator.** Layout, colors, and derived fields live in your `jq` / `fzf`
  / agent layer.
