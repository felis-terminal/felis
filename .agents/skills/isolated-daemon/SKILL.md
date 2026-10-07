---
name: isolated-daemon
description:
  Run a private felis daemon on a per-run socket for any unattended experiment (a debug build, a profile, a producer
  run, a GUI probe, an A/B against an older commit) and tear it down without touching the user's own daemon, windows, or
  `felis sessions` streams. Use before launching `felis-daemon` or a felis window from an agent shell on Linux or macOS.
  The producer-traffic-debug, felis-macos-gui-debug, and perf-trace skills build on it.
compatibility: Linux or macOS with felis built. The macOS Claude Code Bash sandbox blocks the socket bind.
allowed-tools:
  Bash(.agents/skills/isolated-daemon/scripts/*) Bash(./target/release/felis:*) Bash(./target/release/felis-daemon:*)
  Bash(pgrep:*) Bash(ps:*) Read
---

# A private felis daemon

The user's per-uid daemon holds their persistent shells. An experiment that joins it, restarts it, or kills it by
process name loses them, so every unattended run gets its own daemon on its own socket.

## Start, attach, stop

```sh
eval "$(.agents/skills/isolated-daemon/scripts/isolated-daemon.sh start)"   # sets SOCK and LOG
env SHELL=/path/to/probe.sh ./target/release/felis --socket "$SOCK" >client.log 2>&1 &
# … experiment …
.agents/skills/isolated-daemon/scripts/isolated-daemon.sh stop "$SOCK"
```

`start` binds a fresh `0700` directory under `/tmp`, runs `felis-daemon serve` there with a debug `RUST_LOG`, waits
until the socket is served, and prints `SOCK=` and `LOG=`. `stop` ends the clients on that socket first, then the
daemon, then removes the directory. Scripts source the same file for `felis_dbg_socket`, `felis_dbg_wait`,
`felis_dbg_pids`, and `felis_dbg_stop`.

## Rules the helper encodes

- **A fresh socket per run.** A reused socket connects to the daemon an earlier run left behind, which runs the old
  build and writes no fresh logs.
- **Start the daemon yourself.** An auto-spawned daemon's stderr goes to `/dev/null` or the systemd journal, and its
  lines reach only `daemon.log` in the log directory (`docs/reference/cli.md` "Log files"). On a Linux desktop with a
  systemd user manager, autospawn also hands the daemon to `systemd-run`, outside any profiler's process tree.
- **The socket's parent must be a `0700` directory you own.** `/tmp` and the `$TMPDIR` root are refused by name, a
  `0755` directory for its mode, and a long path (an agent scratchpad, a sandboxed `$TMPDIR`) fails `SUN_LEN`. All three
  appear only in the daemon log, never on the client's stderr.
- **`SHELL` goes on the client.** A local session's shell comes from the environment the client sends; the daemon's own
  `SHELL` is only the fallback.
- **Tear down by the full socket path.** `pkill -x felis` / `pkill -x felis-daemon` end the user's windows and sessions,
  `pkill -f <socket>` matches the calling shell's own argv, and a basename match reaches the user's daemon, whose argv
  carries a `.sock` too. `felis_dbg_pids` matches one whole argv element against the full path.
- **Clients before the daemon.** A client that outlives its daemon reconnects, and the reconnect autospawns a
  replacement on the same socket.

## Config and environment

Name a config copy with `felis --config <absolute path>`; the user's real one is often a read-only home-manager symlink.
Never point `$HOME` or `$XDG_CONFIG_HOME` at a scratch directory instead: the variables leak into an auto-spawned daemon
(shells start without their rc files), and the only cure is killing the shared per-uid daemon.

## Comparing against an older commit

```sh
git worktree add .claude/worktrees/felis-pre <rev>
CARGO_TARGET_DIR=/tmp/felis-pre-target cargo build --release \
  --bin felis --bin felis-client --bin felis-daemon   # inside the worktree
```

Build all three: the front door execs a sibling `felis-client`, and autospawn prefers a sibling `felis-daemon` over
`PATH`. Point `FELIS_DAEMON_BIN` and the client at the same build. Strip keys the older build does not know from the
test config (the `add-config-key` skill's gotcha), or the comparison silently runs on defaults.
