---
title: Non-goals
sidebar:
  order: 4
---

Things felis explicitly will not do. This list is load-bearing: disagreements about scope should be resolved by adding
to this file rather than by relaxing it, and a feature being cheap to implement is not a reason to add it. Items can be
promoted out of "non-goal" status only through an explicit design decision recorded in the owning design doc, with
rationale, rejected alternatives, and revisit trigger included.

## Multiplexer features (delegated to the WM)

- **Tabs.** Use the WM's tab/group features (yabai spaces, Hyprland workspaces, i3 stacks, AeroSpace groups, etc.).
- **Splits and panes.** Use the WM's tiling.
- **Window grouping or layouts.** WM concern.
- **Session groups (tmux-style).** A session tag is a label an external picker filters on
  ([control-surfaces.md](architecture/control-surfaces.md#tags-are-cli-only-labels-felis-stores-but-never-interprets));
  binding a group to shared layout, synchronized attach, or broadcast input is rejected, because a window driving more
  than one PTY at once is the multiplexer behavior principle 1 forbids.
- **Session-list UI.** No selector inside the terminal.
- **Window-management chords.** New window, tile or split, and move between displays are the WM's keybinds; felis binds
  none of them.
- **Escape-sequence window manipulation** (XTWINOPS `CSI 1–9 t`: move, iconify, raise / lower, resize). Window geometry
  belongs to the WM, and a Wayland client cannot position its own surface in any case. Which codes are answered and
  which are accepted and ignored is [the support matrix](../reference/protocols/support-matrix.md#vt--ansi).

## Cross-host and multi-user features

Single-user SSH stdio attach is implemented, and the protocol is transport-pluggable by design
([ipc.md](architecture/ipc.md#cross-host-attach-ssh-stdio)). The hard non-goals in this category are:

- **Multi-user attach.** One daemon serves one user account: no sessions shared between UIDs, no permission model, no
  audit log. The boundary guards multiple accounts, not multiple devices, so same-user mirroring
  ([session-lifecycle.md](architecture/session-lifecycle.md#same-user-mirroring)) does not breach it.
- **User-namespaced session catalogs.** The felis daemon manages window-to-session bindings, not human-readable session
  names. If naming is desired, layer it in a wrapping tool.
- **Sandbox-transparent endpoints.** The daemon endpoint is `/tmp/felis.<uid>/daemon.sock`, so a process with a mount
  namespace of its own (`PrivateTmp=` services, bwrap or flatpak sandboxes) reaches no daemon outside that namespace
  ([overview.md](architecture/overview.md#where-an-auto-spawned-daemon-lands)). felis detects none of this; run it
  outside the sandbox, or name an endpoint both sides share with `--socket`.
- **Seamless network-roaming reattach (mosh-style UDP).** Surviving an IP change or a sleep without re-establishing the
  connection is out of scope: the session lives in the remote daemon, so reconnecting over SSH and reattaching to it
  recovers the work until the remote host reboots, and what a UDP listener with local echo would add is invisibility,
  not survival. _Revisit if_ typing latency over a long-haul link is what users report: mosh's non-blocking local echo
  is separable from its transport, and predicting the echo of local keystrokes rides the stream felis already has.
- **Cross-session image sharing.** Each session owns its image store. A global cache across sessions would need
  multi-tenant lifetime tracking, which is the accounting the single-user, per-session boundary exists to avoid.

## State persistence and recovery

- **Restoring sessions or scrollback across reboot or daemon restart.** Daemon state is RAM-only. Closing a window does
  not kill the session, but terminating the daemon ends all sessions and drops their scrollback. Persisting scrollback
  to disk or checkpointing shell processes across reboots is out of scope.
- **Migrating sessions between daemon processes or hosts.** A session is pinned to the daemon process that spawned its
  PTY. Live process migration (such as CRIU-style checkpoint and restore) is not supported.
- **Compression or disk-paging of scrollback.** Inactive scrollback stays in the unified in-memory ring until evicted by
  capacity bounds. Compressing old rows or paging them to disk adds search and traversal latency without saving enough
  memory to justify the complexity.

## Scripting and extensibility

- **Embedded scripting language.** No Lua, Python, JavaScript, or Wasm plugin host. User-supplied WGSL post-process
  shaders sit outside this boundary: a shader is a pure function from typed uniforms to pixels, executed on the GPU,
  with no access to actions, events, or I/O. The decision drawing that line is recorded in
  [rendering/pipeline.md](rendering/pipeline.md) "Cursor trail and user post-process shaders".
- **A configuration language.** The config file is TOML felis reads, never a program it evaluates: no conditionals, no
  expressions, no callbacks into the terminal, and no per-user dialect on top of the keys. Live reload re-reads that
  same declarative file, so reloading changes values and never runs code (REQ-012).
- **Event callbacks at the terminal layer.** Reactive behavior belongs in the shell, in the editor, or in an external
  process consuming the typed events felis already publishes (`felis notifications subscribe`).
- **Custom keybind DSL.** Keybinds map to a fixed enum of actions. Anything beyond that should be a shell command.
- **Keybindings over the CLI or the wire.** There is no `sessions bind`. A keymap is a config document the client reads,
  not a runtime IPC operation, so a binding cannot be set by a script or by a program running in a session.
- **Macros / record-and-replay.** In-terminal macro recording and playback are omitted; automation belongs in external
  tooling or across the IPC interface.
- **Pointer warp / mouse capture.** The window does not lock or warp the host pointer; mouse reporting follows standard
  terminal tracking protocols.

## Compatibility theater

- **Bit-perfect emulation of historical terminals.** felis targets a modern, opinionated subset. Software that only
  works on a literal DEC VT220 is not the audience.
- **tmux passthrough integration.** Being a "good citizen" inside tmux is the opposite of the project's purpose. felis
  rejects the premise that tmux should be in the local loop.
- **Colorimetric color specs** (X11's `CIELab`, `CIELuv`, `CIEXYZ`, `CIExyY`, `CIEuvY`, `TekHVC` forms in
  `OSC 4 / 5 / 10–12`, and the `rgbi:` intensity form alongside them). `rgb:` and `#hex` are the committed subset; the
  remaining forms serve xterm's own extension rather than any tool felis targets. The argument and its revisit trigger
  are in [protocols/vt-compliance.md](protocols/vt-compliance.md) "Conscious omissions".
- **Sixel.** Kitty graphics is the only inline-image protocol felis implements. Software that emits only Sixel is
  second-class. (Kitty itself does not implement Sixel; rejecting it keeps felis aligned with the upstream protocol
  set.)
- **XTSMGRAPHICS graphics sizing query (`CSI ? Pi ; Pa ; Pv S`).** It sizes xterm's raster surfaces, the Sixel
  color-register count and the Sixel and ReGIS geometry, none of which felis keeps. No replacement query stands in for
  it: Kitty graphics' `a=q` is a capability probe that answers which transmission media and formats decode, never a
  numeric limit. While there is no raster surface for a producer to size, there is nothing for one to ask. _Revisit if_
  felis adopts a graphics family of its own whose bounded raster or color-index resources a producer must size before
  transmitting.

## Convenience features that hide complexity

- **Auto-detection of intent.** Configuration is explicit. Heuristics decay; the user is in charge.
- **Heuristic URL highlighting.** OSC 8 hyperlinks are supported. Pattern-matching URLs in plain text is not.
- **Heuristic prompt or command detection.** Prompt positions and marks are derived strictly from explicit OSC 133
  sequences. felis never sniffs shell prompt patterns (such as `PS1` regexes) from plain text.
- **Built-in image viewer, file picker, completion menu.** Shell-level tools handle these.
- **An in-client performance overlay.** An overlay spends GPU time inside the frame it is measuring, so it perturbs the
  number it reports, and it leaves no artifact to compare against a previous run; diagnostics are trace-driven from
  outside the process instead ([feature-baseline.md](feature-baseline.md#diagnostics-and-performance-budgets)). _Revisit
  if_ a class of rendering bug appears that a trace taken outside the client cannot localize.
- **An in-process desktop-notification surface.** The terminal is not a notification daemon: felis never links zbus /
  portal / NSUserNotification / Windows Toast and never draws a popup itself; that role lives one process boundary away,
  with the popup a consumer's job (`notify-send` / `terminal-notifier` / `osascript`). Decoding the notification
  protocols (OSC 9 / 99 / 777) and relaying a typed event to `felis notifications subscribe` is in scope; the scope line
  and its rationale are in [protocols/notifications.md](protocols/notifications.md).

## Layout features superseded by uniformity

- **Per-row tab stops.** felis carries one global tab-stops table. Modern TUIs never depend on per-row stops outside
  VT420 page memory, which felis does not implement either
  ([support-matrix.md](../reference/protocols/support-matrix.md#cursor-and-screen)).
- **Bidi paragraph reordering across cells.** felis renders bidi-marker glyphs for the Trojan-Source class
  (CVE-2021-42574) and shapes RTL scripts within a single cell, but does not re-order cells inside a paragraph the way
  mlterm / wezterm partial do. Full paragraph bidi requires per-paragraph reflow that conflicts with the cell-grid
  invariant. Promote out of non-goal if a real producer emerges.

## Cross-platform constraints

- **macOS-only or Linux-only features.** Where a feature is not uniformly implementable across macOS, Linux, and
  Windows, it is omitted rather than gated. The exception is a duty whose _intent_ is uniform but whose _mechanism_ the
  OS forces to differ. That is a platform shim rather than a gated feature, and felis takes four of them:

- **The PTY abstraction** over ConPTY and `/dev/ptmx`, whose keyboard-input half is win32-input-mode (`?9001`)
  ([input.md](input.md#windows-win32-input-mode)).
- **Background blur**, discharged only on the platforms whose OS puts it on the application rather than the compositor,
  through `window.backdrop`'s per-OS value set ([config.md](../reference/config.md#complete-annotated-example)).
- **Kitty graphics transmission methods**, where Windows implements `t=d` alone and the protocol's own `a=q` probe
  carries the fallback ([protocols/kitty-graphics.md](protocols/kitty-graphics.md#windows-takes-td-only)).
- **Daemon supervision at auto-spawn**, asked of an init system only on Linux, where systemd's user manager would
  otherwise give the launching window's `OOMPolicy` power over the daemon
  ([overview.md](architecture/overview.md#where-an-auto-spawned-daemon-lands)).

None of the four is a precedent for OS-gated _convenience_: each meets the same intent on every target.

## Future revisits (require a recorded decision to lift)

- Display-side broadcast (one input, many windows).
- A second built-in image protocol (e.g. iTerm2 inline images).
