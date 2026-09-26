---
title: Attach a session over SSH
sidebar:
  order: 5
---

Run terminal sessions on a remote host, view them in a local window, and keep processes running across network
disconnects using `--host`.

**Prerequisites:** Install felis on the remote host (providing `felis-daemon`) and configure SSH access. felis invokes
`ssh <host> felis-daemon relay` and communicates over standard I/O. SSH handles all authentication; configuration in
`~/.ssh/config` (hosts, identity keys, jump proxies) applies automatically.

## Open a remote session in a local window

```sh
felis --host user@remote
```

The window opens locally and attaches to a shell on the remote daemon. Everything renders locally, while the shell
process and scrollback remain on the remote machine. If no daemon is running on the remote host, felis starts one
automatically.

## Drive the remote daemon headlessly

Every `felis sessions` command accepts `--host` and executes against the remote daemon instead of your local socket:

```sh
felis --host user@remote sessions list
ID=$(felis --host user@remote sessions spawn -- /bin/sh)
felis --host user@remote sessions send "$ID" 'cd /srv && ./deploy.sh' --key enter
felis --host user@remote sessions capture "$ID"
```

## Reconnect after disconnects

Closing the local window, losing network connectivity, or terminating the SSH session leaves remote sessions intact.
When network connectivity drops, an attached window attempts to redial the remote session before closing. The remote
daemon keeps its endpoint until the host reboots, so reattaching needs nothing of the login that started it. To reattach
afterwards:

```sh
felis --host user@remote sessions list        # verify running sessions
felis --host user@remote attach "$ID"         # attach in a local window
```

### SSH agent forwarding persistence

The remote daemon keeps `SSH_AUTH_SOCK` usable across disconnects: remote shells see a symlink the daemon owns
(`<daemon socket>.agent`), which it points at the newest live connection's forwarded agent socket. Reconnecting with
`felis --host user@remote` restores agent access in existing shells without restarting sessions or exporting new
variables:

- While disconnected, the agent socket is unavailable to the remote shell.
- On reconnect, agent access resumes automatically in the processes of the daemon the reconnect reaches.

Enable agent forwarding for the target host in your SSH configuration:

```
Host remote
    ForwardAgent yes
```

Without agent forwarding, remote shells run without an agent. On Windows, OpenSSH agent uses a persistent named pipe, so
socket proxying is not required.

## Retarget an existing window

`felis --host` opens a new window connected to a remote daemon. To point an existing window to another daemon, run
`felis ssh` or `window retarget` from within that window:

```sh
felis ssh user@devbox                  # re-point this window at devbox
felis --host user@devbox               # open a new window on devbox instead
felis window retarget                  # return to the local daemon
felis window retarget ~/.felis-alt/daemon.sock   # a second daemon on this machine
```

The target is the window from which the command runs. `felis ssh` takes an SSH destination; `window retarget` takes a
local daemon socket, and omitting it returns the window to your default local daemon. A socket outside the default
endpoint needs a dedicated `0700` directory you own ([cli.md](../reference/cli.md#carrier-and-connection-lifetime)
"Socket directory").

The window connects to the new daemon and attaches to the target session before disconnecting from the prior daemon. If
the destination is unreachable, the window remains connected to its existing session. Attach to a specific session with
`--session <prefix>`, or run a command in a new session with trailing arguments (such as `-- <cmd>`).

Terminating a shell with `Ctrl+D` (or `exit`) ends the remote process, and the window returns to the preceding daemon
automatically. In contrast, running `felis window retarget` detaches cleanly and leaves the remote session running in
the background.

### Hostname resolution

Retargeting invokes `ssh` on the machine displaying the window. Hostnames resolve against that machine's
`~/.ssh/config`. Running `felis ssh prod` from a window attached to `devbox` connects your local machine directly to
`prod`, without tunneling through `devbox`. For multi-hop connections, configure jump hosts in `~/.ssh/config`.

### Ad-hoc connections and custom SSH flags

To connect to hosts without `~/.ssh/config` entries, specify SSH URIs or pass command-line flags directly:

```sh
felis ssh ssh://root@192.168.122.5:2222
felis ssh vm --ssh-arg=-p --ssh-arg=2222
felis ssh vm --ssh-arg=-i --ssh-arg=~/.ssh/vm_key
```

`--ssh-arg` passes a single argument token verbatim and may be repeated. The same flag is supported globally on
`felis --host ... --ssh-arg`.

### OpenSSH ControlMaster configuration

Each window retarget and session switch over SSH launches an `ssh` process. For frequent switching, enable OpenSSH
`ControlMaster` in `~/.ssh/config` so that subsequent connections share a persistent multiplexed socket.

## Remote notifications

Notifications emitted by remote tasks travel over the felis connection to your local desktop. For subscriber
configuration, see [Get notified when a job finishes](enable-notifications.md).

## Operational notes

- **Remote windows support local actions:** session switching chords, `pipe` actions, and `run` actions operate within
  `--host` windows, and switching sessions redials the transport. The commands themselves run on the client machine; to
  reach the remote one, branch on the host and working directory the action carries in its environment
  ([keybindings.md](../reference/keybindings.md)). To run a command on the remote host of a `--host` window, have the
  script branch on `$FELIS_HOST`:

  ```sh
  #!/bin/sh
  # Open lazygit on remote repository when attached over SSH.
  [ -n "$FELIS_HOST" ] || exec lazygit
  case $FELIS_CWD in file://*) DIR=/${FELIS_CWD#file://*/} ;; *) DIR= ;; esac
  ID=$(felis --host "$FELIS_HOST" sessions spawn ${DIR:+--cwd "$DIR"} -- lazygit)
  felis ssh "$FELIS_HOST" --session "$ID"
  ```

- **Persistent daemons on a server:** every connection to one remote account shares one daemon, at an endpoint a logout
  does not move ([cli.md](../reference/cli.md) "Carrier and connection lifetime"). One case still ends a daemon at
  logout: on a systemd host the client hands a new daemon to the user manager, which logind stops shortly after the last
  session of a non-lingering user. Run `loginctl enable-linger <user>` once on a host meant to serve persistent daemons.
  A daemon the relay forked (the `--host` path) is a plain process in the login session's scope, so logind's default
  `KillUserProcesses=no` is what leaves it running: a host that sets `KillUserProcesses=yes` needs that account in
  `KillExcludeUsers=` to keep its daemon across a logout ([cli.md](../reference/cli.md) "Auto-spawning").
