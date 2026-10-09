---
title: Update felis
sidebar:
  order: 2
---

Upgrade the felis package, then switch the running daemon to the new binary. Upgrading replaces the binary on disk; the
daemon already in memory keeps serving its sessions on the build it started from until you switch it. On Linux and macOS
`felis daemon upgrade` switches it in place and every session survives; on Windows, or when the daemon refuses, drain
the sessions and restart the daemon instead.

Wire protocol changes across minor versions are backward-compatible, so an old daemon keeps working with a new client
until you switch. `felis doctor` notes when the running daemon is not the installed build.

## Install the new binary

With a profile install, upgrade the profile entry:

```sh
nix profile upgrade felis
```

With the Home Manager module, update the flake input and activate:

```sh
nix flake update felis   # in your home-manager / NixOS flake
home-manager switch      # or nixos-rebuild switch
```

With Homebrew, upgrade the formula:

```sh
brew upgrade felis
```

From the Linux or macOS archive, unpack the new tarball into a directory of its own and point your `PATH` at it:

```sh
mkdir -p ~/.local/opt/felis-0.1.1
tar -xzf felis-x86_64-linux.tar.gz -C ~/.local/opt/felis-0.1.1 --strip-components=1
~/.local/opt/felis-0.1.1/bin/felis --version   # then point your PATH at it
```

On macOS the tarball is `felis-aarch64-darwin.tar.gz` and the unpacked tree needs
`xattr -dr com.apple.quarantine ~/.local/opt/felis-0.1.1` before anything in it will launch
([install](install.md#the-macos-archive)).

Every archive unpacks under the same name, so each version needs a directory of its own. Unpacking one over a tree
already in use keeps every library that version drops, and the launchers then load a mixture of the two; on macOS it
also leaves the app's resource seal covering files the new version does not contain, which stops it launching at all.
Remove the tree you replaced once the daemon has switched to the new one.

## Switch the daemon in place

On Linux and macOS, run the upgrade from the newly installed `felis` while you are not using any session:

```sh
felis daemon upgrade
```

The daemon replaces its own binary with the `felis-daemon` installed beside that `felis` and keeps its sessions: every
shell keeps running with its screen and scrollback. Open windows lose their connection once and re-attach on their own.
Do it while idle because a few things in flight do not cross the switch: keystrokes typed during it are dropped, a
running `felis sessions send --wait` reports the session ended, and `felis notifications subscribe` streams end and need
restarting.

An accepted upgrade prints `the daemon is switching to <path>` and exits `0`. To confirm the switch, run `felis doctor`:
its `daemon` row shows the running build, and names the installed one only while the two differ.

If the new daemon fails to restore the sessions after it has taken over, they end the way a restart ends them, so save
work you cannot lose before upgrading.

A refusal changes nothing: the daemon keeps serving every session as before, and the message names the reason
([cli.md](../reference/cli.md#daemon-upgrade)). Retry a timeout once the sessions are quiet; after any other refusal,
drain and restart below.

## Drain your sessions

On Windows, or after a refused upgrade, restart the daemon instead. A restart ends every running session, so save your
work first. List what is running:

```sh
felis sessions list
```

Put the daemon into its draining state, where it refuses new sessions and exits after the last one ends:

```sh
felis daemon stop --when-empty
```

Then close the listed sessions at your own pace. Until the last one ends, `felis daemon status` reports `draining: yes`,
and any attempt to create a session on that daemon is refused.

## Restart the daemon

If you drained, the daemon is already gone once the last session ended. Otherwise stop it directly:

```sh
felis daemon stop            # refuses while sessions remain, and says how many
felis daemon stop --force    # destroys every session, then stops
```

On Linux the daemon is often a transient systemd user unit, and `systemctl --user stop felis-daemon-<hash>` on it is
refused; use `felis daemon stop` ([cli.md](../reference/cli.md#auto-spawning)).

Two forms start the freshly installed daemon binary: a window launch (bare `felis`, `felis attach`, `felis -- <cmd>`)
and `felis sessions spawn`. No other headless verb starts one: `felis sessions list` and `felis daemon status` report
the daemon unreachable, while `felis version` and `felis doctor` report it as not running. The full table is in
[cli.md](../reference/cli.md#auto-spawning).

Sessions you cannot find after the upgrade are held by a daemon on an endpoint the default rule does not name; stop it
from a shell inside one of its own sessions, where `$FELIS_SOCKET` still names it, with
`felis --socket "$FELIS_SOCKET" daemon stop --when-empty`.

Windows left open across a restart do not survive it: sessions terminate with the daemon holding their PTYs. Launch a
fresh window afterwards to connect to the updated daemon.

If a protocol major version mismatch occurs, the daemon refuses the connection before exchanging messages, and the
client reports:

```
protocol major mismatch: client speaks 1, daemon serves 2-2 — rebuild and restart felis (both halves)
```

Across a protocol major version bump, drain sessions using the current client before upgrading binaries so that you can
disconnect or terminate sessions cleanly. Then restart the window onto the new build. The versioning specification is in
[ipc.md](../reference/ipc.md#versioning).

## Update a remote machine reached with --host

When updating a remote machine accessed via `--host`:

1. Update the felis package on the remote machine.
2. Switch its daemon in place with `felis --host user@remote daemon upgrade`, which runs the remote machine's own
   `felis daemon upgrade` over SSH.

If that refuses, check the roster with `felis --host user@remote sessions list` and drain it with
`felis --host user@remote daemon stop --when-empty`; the next `felis --host user@remote` invocation spawns the updated
daemon binary over SSH.

## Changes not requiring a daemon restart

Config changes need no restart: running clients reload `config.toml` on save
([config.md](../reference/config.md#live-reload)).
