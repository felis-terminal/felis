---
title: Update felis
sidebar:
  order: 2
---

Upgrade the felis package, drain your sessions, and restart the daemon onto the new binary. Upgrading replaces the
binary on disk; the daemon already in memory keeps serving its sessions until you restart it.

Wire protocol changes across minor versions are backward-compatible. Restart the daemon when you need changes to the
daemon binary itself, such as bug fixes or protocol major version bumps.

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

From the Linux or macOS archive, drain and stop the daemon first (the next two sections), then unpack the new tarball
into a directory of its own and point your `PATH` at it:

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
Remove the tree you replaced once the new one runs.

## Drain your sessions

Restarting the daemon ends every running session because stopping the daemon closes PTY masters and active shells. felis
does not persist session state on disk (documented in
[session-lifecycle.md](../explanation/architecture/session-lifecycle.md)). Save your work before restarting:

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
2. Check the roster with `felis --host user@remote sessions list`, then drain it with
   `felis --host user@remote daemon stop --when-empty`.

The next `felis --host user@remote` invocation automatically spawns the updated daemon binary over SSH.

## Changes not requiring a daemon restart

Config changes need no restart: running clients reload `config.toml` on save
([config.md](../reference/config.md#live-reload)).
