---
title: Notifications design
sidebar:
  order: 5
---

This page records why felis decodes desktop-notification escapes but never acts on them itself, and the posture around
notification helpers. The wire grammar, key tables, and conformance scope live in the
[reference twin](../../reference/protocols/notifications.md).

Producers post a desktop notification by emitting an escape code on the terminal stream. felis adopts a
**decode-and-relay** posture: it decodes OSC 9, OSC 99, and OSC 777 into typed notification events and exposes them on
its extension surface, but it **never draws a popup or links an OS notification backend**. Owning per-platform
notification glue (zbus / portal / NSUserNotification / Toast) is exactly the scope Principle 1 rejects (no capability
without a consumer, no embedded backend; IPC is the extension surface).

## Why relay, not act

A monolithic local terminal can tell producers to "just call `notify-send`." felis cannot: it ships a daemon/client
split with cross-host attach ([`ipc.md`](../architecture/ipc.md)), so a program under a _remote_ daemon has no
`notify-send` path to the _local_ desktop: the terminal byte stream is the only channel. That is the exact problem kitty
built OSC 99 to solve. felis therefore decodes the escape and hands a typed event to a consumer of the user's choosing;
the consumer (not felis) calls the platform notification API.

ECMA-48 standardizes only OSC _framing_; the semantics of any given OSC code are left to the implementation. "Implement
OSC 99" therefore means "decode it and honor the contract," not "draw a popup": protocol acceptance and the notification
action are separable concerns, and felis takes only the first. The rejected alternatives bracket the posture from both
sides:

- **Dropping the escapes entirely** under-weights cross-host, where felis is most differentiated. The non-goal that
  holds is narrower: no in-process notification daemon ([`non-goals.md`](../non-goals.md)).
- **Embedding a notification backend** is the exact "desktop-notification surface one process boundary away" the
  non-goal forbids, plus per-OS glue that rots.
- **Pure IPC with no in-window attention at all** is over-pure: BEL already crossed the "flash the window on a producer
  signal" line, and a notification is a strictly stronger BEL. The attention flash is therefore the _only_ place felis
  itself acts, bounded to what BEL established.
- **An in-window clickable banner drawn by felis** crosses back toward "felis draws notifications" and needs a new
  clickable surface; kept only as a possible future inbound route.

## The interactive surface: three-way split

Three of OSC 99's surfaces go unsupported for three different reasons, which the reference twin's conformance table
distinguishes. They must not be conflated:

- **Action buttons / `a=report` / `a=focus`: rejected by design.** Buttons are an input surface a remote or untrusted
  producer could drive inside felis, and a callback shape Principle 1 forbids. The title/body still relays; only the
  action is dropped.
- **`c=` close-events: accepted and ignored**, like the other unsurfaced keys; there is no error. This is _not_ deferred
  work: felis simply has no close _back-channel_ to report against, which is why `p=?` omits `c`.
- **Inbound activation/close _reports_: deferred, not impossible.** There is no architectural blocker: the daemon
  already injects input on the CLI surface, so synthesizing `OSC 99 ; i=<id> ; <event> ST` into a session's PTY is
  trivial. The real cost is on the _helper_, which would graduate to a stateful D-Bus client round-tripping freedesktop
  `ActionInvoked`/`NotificationClosed` signals; the toast is still drawn outside felis. Revisit when a real consumer
  needs the reports; when built, the inject path **must** be constrained to well-formed OSC-99 reports referencing an
  `i=` the daemon actually relayed, not a general injection backdoor.

Icons / sound / auto-expire (`n/g/s/w`) have no surface to honor them; revisit if a producer needs them surfaced rather
than accepted-and-ignored.

The `p=?` capability reply is static: it does not check whether a subscriber is actually running. An app thus learns not
to wait for activation reports it will never receive, rather than inferring support from silence. Revisit gating the
reply on a live surface (subscriber connected _or_ attached) if "supported" proves misleading because no subscriber is
running often enough that it is effectively false.

## Helpers: the bright line

felis bundles no notification helper. The consumer is a standalone Unix filter outside the felis process, reading the
subscribe stream (the working recipe lives in [Enable notifications](../../how-to/enable-notifications.md));
`terminal-notifier` (macOS) or `osascript` stands in for `notify-send` where needed. It is not a kitty-kitten mechanism
(kittens need an embedded runtime Principle 1 rejects), and **auto-start is the bright line**: if a felis _binary_ ever
auto-spawns the helper, felis is a notification daemon again.

The wire shape the helper consumes (`NotifyToDaemonMsg::Subscribe`, `NotifyToClientMsg::Event`, and the `Observer`
connection mode that admits them) lives in [`ipc.md`](../../reference/ipc.md); revisit it if a non-Rust client cannot
reuse the observer connection.

### The opt-in home-manager relay

For Nix users, that filter ships as the opt-in home-manager option `programs.felis.notifications.enable`
(`nix/hm-module.nix`); the precedent for distributing a reference helper this way is the `tic`-installed
`share/terminfo/felis.terminfo` ([`terminal-identity.md`](../architecture/terminal-identity.md)). It generates the same
`subscribe`-to-notifier bridge and wires it as a **user service** (a systemd user unit on Linux, a launchd agent on
macOS), so the per-OS daemonization that the shell recipe leaves to the reader is expressed once. `notifier` defaults to
a `notify-send` / `terminal-notifier` wrapper and takes any `<title> <body> <urgency>` command; `onlyDetached` keeps the
`attached == false` filter. It subscribes to the local daemon by default, and `host = "user@remote"` points it at a
_remote_ daemon over SSH instead: cross-host attach bridges every connection to the one persistent per-UID daemon on the
remote ([`ipc.md`](../../reference/ipc.md) "Cross-host carrier: SSH stdio"), so the persistent subscriber shares that
daemon's broadcast hub with any `--host` window and reaches the same sessions.

This does **not** breach the bright line. The line guards the felis _binary_ auto-spawning a helper; here the _user_
declares the service (the option defaults off and is independent of `programs.felis.enable`), it runs one process
boundary away consuming the public IPC, and no felis crate links a notification backend: the `libnotify` /
`terminal-notifier` dependency is the module's, pulled into the user's profile, never into a felis binary. The rejected
alternative is wiring the relay on whenever `programs.felis.enable` is set: that would make the popup automatic for
every Nix user and _would_ cross the line. _Revisit_ if a non-Nix packaging path needs the same turnkey wiring; a
`contrib/` script is the natural shape for it.
