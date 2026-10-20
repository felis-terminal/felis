# When something fails

Read this when a verb exits `1` or `2` and the `error.kind` alone does not say what to do next.

## Contents

- Spawn refused or input not delivered
- A draining daemon
- Ending a daemon you started
- A stale daemon
- Payload limits

## Spawn refused or input not delivered

Run `felis daemon status --format json`. It prints one object listing every admitted resource as `{"resource","unit",`
`"scope","total_used","max_subject_used","per_subject_limit",` `"global_limit"}`; absent fields mean no value, so read
with `.field // empty`. Two rows matter to an agent. A spawn past the `sessions` cap or a connect past the `connections`
cap (each row's `per_subject_limit` or `global_limit`) is `{"error":{"kind":"at_capacity"}}` at exit `1`: reap or close
and retry rather than treating it as a broken invocation. A `pty_input_bytes` row near its per-subject limit means the
child stopped reading its stdin; `send` then blocks until admitted, and `--wait --timeout N` bounds that wait too. The
verb exits `2` for an unreachable daemon and never starts one.

## A draining daemon

A spawn refused with `invalid_request` naming a draining daemon means that daemon is going away. `"draining": true` in
`daemon status` says the same thing. No reap makes room, so retrying against it never succeeds; start the session on
another daemon (`--socket` / `--host`).

## Ending a daemon you started

`felis daemon stop` is a point verb, `--format json`, and never autospawns. A bare stop refuses while anything is
running (exit `1`, `{"error":{"kind":"refused","sessions":N}}`), so it cannot destroy an agent's work by accident.
`--force` destroys every session and stops; `--when-empty` refuses new sessions and stops after the last one ends,
answering `{"outcome":"draining","mode":"when_empty", "sessions":N}`. A stop that goes through answers
`{"outcome":"stopping","mode":…}` at exit `0`; exit `2` means the daemon is unreachable or too old to answer.

On Linux a daemon started by a client running under the systemd user manager is a transient unit,
`felis-daemon-<hash>.service` in `app.slice`; `systemctl --user stop` on it is refused, so the verb above is the only
way down
([cli.md "Auto-spawning"](https://github.com/felis-terminal/felis/blob/main/docs/reference/cli.md#auto-spawning)).

## A stale daemon

`felis version --format json` reports `cli`, `client`, `daemon`, each `{"version","revision","dirty"}`; `client` and
`daemon` are `null` unless their `*_status` is `ok`. The daemon outlives its windows, so compare `daemon.revision`
against `cli.revision`. `felis --version` reports only this binary and dials nothing, so it refuses `--config`,
`--host`, `--socket`, and `--ssh-arg` at exit `2` rather than ignoring them.

`felis daemon upgrade` replaces a stale daemon without losing sessions. It is a point verb, `--format json`, and never
autospawns. The daemon execs the `felis-daemon` installed beside `felis` in place and keeps every session; it answers
`{"outcome":"upgrading","successor":"<path>"}` at exit `0`, and attached windows re-attach on their own. A refusal is
exit `1`, `{"error":{"kind":"refused","reason":…}}`, with the daemon still serving; the fallback is
`felis daemon stop --when-empty`. Exit `2` covers an unreachable daemon and one too old to answer (`unsupported`). With
`--host`, the remote host's own `felis daemon upgrade` runs over `ssh` and its output passes through; a remote `felis`
too old to have the verb is reported as `unsupported`, exit `2`.

## Payload limits

An over-limit request (the text of `sessions send`, the pattern of `sessions search`, the program, cwd, argv or env of
`sessions spawn`) is refused before it reaches the daemon, exit `1` with `{"error":{"kind":"invalid_request",…}}`; the
message names the field, its size, and the cap. Chunk rather than raise.
