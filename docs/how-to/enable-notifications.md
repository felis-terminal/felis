---
title: Get notified when a job finishes
sidebar:
  order: 4
---

Have a long job pop a desktop notification when it finishes: emit a notification escape sequence from the job, then run
a subscriber that displays it. felis decodes OSC 9, 99, and 777 into structured events and streams them over IPC; the
desktop popup itself belongs to a notifier you supply.

## Emit a notification escape sequence

The simplest notification sequence is OSC 9 (body only). Append it to any command so that it executes upon command
completion:

```sh
cargo build --release; printf '\033]9;build finished\033\\'
```

`\033]9;` opens the escape sequence, followed by your message text, and `\033\\` (ESC `\`) closes it. Using `;` triggers
the notification on both success and failure; gate it with `&&` or `||` for status-dependent messages. For structured
messages with both title and body, use OSC 777 or OSC 99. The full wire formats are specified in
[notifications.md](../reference/protocols/notifications.md#wire-formats).

## Verify relayed notifications

felis streams decoded notifications from all sessions. In another terminal, run:

```sh
felis notifications subscribe
```

Trigger the escape sequence from the previous section. A human-readable line appears showing the session ID, urgency
level, and notification text:

```
a1b2c3d4  [normal]  build finished
```

This human-readable framing is intended for live observation. For scripts and automated pipelines, request JSON Lines
framing:

```sh
felis notifications subscribe --format jsonl
```

Each notification is output as a single JSON object per line:

```json
{
  "v": 1,
  "session_id": "a1b2…",
  "title": null,
  "body": "build finished",
  "urgency": "normal",
  "notification_id": null,
  "session_title": "cargo",
  "cwd": "/home/you/src",
  "attached": false
}
```

The `attached` field indicates whether a client window is currently attached to the session. When `attached` is `false`,
no window is open, so desktop delivery notifies you while you work elsewhere. The most recent notification for a session
is also available in `felis sessions info <id> --format json` under `last_notification`.

## Display desktop notifications

felis includes no internal desktop notification UI; wire a notification tool (such as `notify-send` on Linux or
`terminal-notifier` on macOS) to the `--format jsonl` stream:

```sh
felis notifications subscribe --format jsonl | while read -r evt; do
  # Notifications omit the `event` key. The stream also outputs
  # `{"event":"lag",…}` if the daemon's ring buffer drops events,
  # and terminates with an `end` or `error` event.
  [ "$(printf '%s' "$evt" | jq -r 'has("event")')" = false ] || continue
  [ "$(printf '%s' "$evt" | jq -r .attached)" = false ] || continue
  notify-send \
    "$(printf '%s' "$evt" | jq -r '.title // .session_title // "felis"')" \
    "$(printf '%s' "$evt" | jq -r .body)"
done
```

Without `--format jsonl`, output uses human-readable formatting and cannot be parsed with `jq`
([cli.md](../reference/cli.md#machine-output)).

Filtering for `attached == false` skips the sessions whose windows already raise a taskbar urgency hint.

### Home Manager configuration

Nix users can enable an automated user service (systemd user unit on Linux or launchd agent on macOS) via the Home
Manager module:

```nix
programs.felis.notifications.enable = true;
```

Available options include `notifier`, `onlyDetached`, and `host` (for subscribing to a remote daemon). The module design
is documented in [notifications.md](../explanation/protocols/notifications.md).

## Remote jobs, local desktop

When running commands under a remote daemon, notifications travel over the felis connection. Subscribe to the remote
daemon from your local machine:

```sh
felis --host user@remote notifications subscribe
```

Notifications from remote tasks display on your local desktop. For connection setup, see
[Attach a session over SSH](attach-over-ssh.md).
