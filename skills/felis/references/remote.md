# Local or remote, same verbs

Read this before a `--host` call, or when `felis` inside a session reaches a different daemon than you expected.

## Remote daemons

`felis --host user@remote sessions …` runs every verb over SSH (`ssh <host> felis-daemon relay`) with identical ids,
exit codes, and shapes, auto-spawn included: `--host … sessions spawn` starts a cold remote daemon exactly as a local
`spawn` does, while every read/drive verb runs `relay --no-spawn` and exits `2` instead. `--host` and `--socket` are
mutually exclusive; a relative `--socket` path is anchored to the caller's working directory; `--ssh-arg <TOKEN>`
(repeatable, needs `--host`) splices verbatim tokens before the destination.

A session spawned over `--host` inherits the remote login's environment, and its `SSH_AUTH_SOCK` is a stable
daemon-owned path, so agent forwarding survives the SSH connection that opened it. The endpoint outlives the login, so
the only thing that ends a remote daemon short of a reboot is the systemd user manager stopping one it started for a
non-lingering user.

## Which local daemon a call reaches

The local daemon is resolved as `--socket`, then `$FELIS_SOCKET`, then the platform default, which on Unix is
`/tmp/felis.<uid>/daemon.sock`, derived from the uid alone. No environment variable takes part, so a desktop login, an
SSH login and a relay all reach one daemon. A socket named by `--socket` or `$FELIS_SOCKET` must sit in a dedicated
directory you own with mode `0700`, and the daemon refuses to start otherwise. Every daemon stamps `FELIS_SOCKET` into
its children, so `felis` inside a session reaches the daemon holding it; run `env -u FELIS_SOCKET felis …` to reach the
default endpoint. Do not set `FELIS_SOCKET` yourself; `sessions spawn --env` refuses it.

When a roster looks empty where you expected sessions, `felis doctor --format json` on that host carries a
`daemon-sibling` row when this shell's `$FELIS_SOCKET` names an endpoint other than the default; the row says what the
default holds, and when a felis daemon answers there its `detail` names the path to pass to
`felis --socket <path> sessions list`.
