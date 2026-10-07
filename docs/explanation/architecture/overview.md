---
title: Architecture overview
sidebar:
  order: 1
---

felis is split across two processes: a long-running **daemon** and a thin **client**. Both speak a common protocol over
a transport: a local carrier (Unix socket on Unix, named pipe on Windows) or a cross-host SSH-stdio carrier. The
canonical crate map is [reference/workspace.md](../../reference/workspace.md).

## Process model

```
┌─────────────────────────────┐                  ┌──────────────────────────────┐
│        felis-daemon        │                  │        felis-client         │
│        (per user)           │  IPC: protocol   │       (per window)           │
│                             │ ◄──────────────► │                              │
│  ┌───────────────────────┐  │                  │  ┌────────────────────────┐  │
│  │ PTY pool              │  │                  │  │ winit window + GPU     │  │
│  │ (one PTY per session) │  │                  │  │ surface (wgpu)         │  │
│  └─────────┬─────────────┘  │                  │  └──────────┬─────────────┘  │
│            ▼                │                  │             │                │
│  ┌───────────────────────┐  │                  │  ┌──────────▼─────────────┐  │
│  │ VT parser             │  │                  │  │ Text shaper            │  │
│  │ + grid + scrollback   │  │                  │  │ (swash)                │  │
│  │ + image store         │  │                  │  └──────────┬─────────────┘  │
│  └─────────┬─────────────┘  │                  │             │                │
│            ▼                │                  │  ┌──────────▼─────────────┐  │
│  ┌───────────────────────┐  │                  │  │ Glyph atlas + render   │  │
│  │ Session lifecycle     │  │                  │  │ pipeline               │  │
│  │ (attach / detach /    │  │                  │  └────────────────────────┘  │
│  │  rehydrate)           │  │                  │                              │
│  └───────────────────────┘  │                  │                              │
└─────────────────────────────┘                  └──────────────────────────────┘
```

## Lifecycle

The front door is `felis` (felis-cli): a window launch (bare `felis`, `felis attach <id>`, `felis -- <cmd>`) execs the
GUI client, a headless verb runs in-process, and either one spawns a daemon and waits for its socket if none is
listening. The window then attaches to a session, input events flow client → daemon and grid updates flow back, and
closing the window leaves the session and its PTY running for the next attach
([session-lifecycle.md](session-lifecycle.md)).

## Daemon process lifecycle

On Linux and macOS the daemon is upgraded in place: every session keeps its shell and its state while the daemon process
switches to the newly installed binary. On Windows it is replaced: the procedure is drain the sessions, then restart
([update-felis](../../how-to/update-felis.md); the stop postures that carry the drain are argued in
[control-surfaces.md](control-surfaces.md) "Stopping the daemon"). Most skew needs neither. A new daemon and an old
client settle on the lower of the two minors ([ipc.md](../../reference/ipc.md) "Versioning"), so only a daemon-_binary_
change calls for an upgrade or a restart. Across a protocol **major** the successor must still serve the major every
connected client speaks, which a major bump's side-by-side decoders provide; an upgrade whose successor misses any one
of them refuses, and the drain with the existing client remains the path.

### In-place upgrade

The daemon binary changes in most releases, because the grid, the parser, and the PTY handling all live in it. Under
drain-and-restart each of those releases ends every session, and the sessions that cost the most to lose are the
long-lived ones the daemon exists to keep (principle 3).

So the upgrade crosses `execve` rather than a process boundary. The daemon keeps its PID, which keeps every shell its
child (reaping still works) and keeps the systemd user unit's main process unchanged. Three things pass through the
exec: the listen socket and each PTY master as inherited file descriptors, and the session state as a structured dump in
an anonymous memory file. The successor restores the grid, scrollback, images, and parser state directly from the dump
and never re-derives them by feeding synthetic escape sequences through its own parser ([design.md](../design.md)
"Correct by construction, not by replay"); it sets close-on-exec again on each descriptor as it adopts it.

The upgrade is an operation the user runs on purpose, rarely, and while nothing else is happening, and the design is
drawn along that line. A hazard the user can avoid by not working while it runs is accepted and stated in the update
procedure rather than engineered away: input typed during the upgrade, a script blocked on `send --wait` or a
notification stream, and an install replacing the successor binary while the upgrade runs. Each of those is visible to
the user who causes it. A hazard that holds no matter what the user does is designed out, because nobody could see it
coming: a session that cannot reach a stopping point, a descriptor leaking into a shell, a signal reaching a process
felis does not own.

The steps before the exec run in a fixed order:

1. **Barrier.** The daemon stops reading client connections and stops admitting spawns, then waits until every spawn it
   already admitted has registered or failed, since an admitted spawn forks after admission. From here the daemon forks
   nothing but the probe in step 4. A frame a client sends past the barrier is lost with its connection at the exec.
2. **Quiesce.** Each session parks where nothing it owns is moving: the PTY reader at the top of its loop, the parse
   thread once it has consumed what was read, the writer between two writes. The quiesce wakes the two waits a session
   spends most of its life in, the reader's poll on an idle PTY and the writer's write to a child that is not reading,
   instead of waiting them out. An idle shell and a job stopped with its input queued are the ordinary state of an
   unattended daemon, so a quiesce that waited on either would fail exactly when the user follows the procedure. Output
   the child writes meanwhile waits in the kernel's PTY buffer and reaches the successor.
3. **Dump.** The dump carries the input the writer has not yet written, from the byte its last write ended at, and the
   parser's state as it stands, since a child can stop mid-escape-sequence and stay there.
4. **Probe.** The daemon runs the successor binary as a child that is handed the dump and nothing else, reads all of it,
   and adopts nothing. The listen socket and the PTY masters are still close-on-exec, so the probe inherits neither. A
   refusal unparks the sessions and reopens the connections.
5. **Exec.** Only now does close-on-exec come off exactly the listen socket, the masters, and the dump: the probe has
   exited and nothing else forks, so no child can inherit a master. An `execve` that returns an error leaves the
   predecessor running, and it restores close-on-exec and refuses the way the probe does.

Every step before the exec is bounded in time, and one that runs out refuses the way the probe does.

The dump answers the objection that sinks most in-place upgrades: a second serialization surface that every change to
the grid, the image store, or the parser state must keep compatible. Its promise is deliberately narrower than the
wire's. It is read only by the immediate successor (old to new, never back), never written to disk, and never exposed
over IPC. Fields are tagged and an absent field takes its default, so adding state, the usual change, costs no dump
work. A change the successor cannot read from its predecessor bumps the dump version instead of growing a conversion
shim, and the upgrade then refuses before anything stops: before the barrier, the successor is asked whether it reads
the running daemon's dump version, and a refusal leaves the running daemon serving with drain-and-restart as the remedy.
The probe in step 4 then checks the contents of a dump whose version is already known to be readable. A breaking state
change therefore costs one release of drain-and-restart, not a permanent compatibility layer.

`execve` cannot be undone, which is why the probe runs before it. But the probe proves the dump readable, not the
restore successful, so a successor can still fail after the exec, and re-executing the predecessor cannot be the answer:
a package manager may already have removed its files. The successor therefore reads the dump's child table before
anything else: the pid and process group of every session, and whether the predecessor has already reaped that child. On
a failure it cannot recover from, it ends each session whose child is unreaped the way a destroyed one ends
([session-lifecycle.md](session-lifecycle.md) "Destruction"): hang up, wait out the grace, `SIGKILL`, reap. An unreaped
child is still its own across the exec, so its pid cannot have been reused and the signal and the reap reach it; a
reaped child's pid may belong to anyone by now, so it is never signaled. The failure costs the sessions without leaving
a shell that ignores `SIGHUP` orphaned on a terminal no daemon lists.

The upgrade execs the `felis-daemon` installed beside the `felis` that asks for it, the same binary a spawn from that
command would start ("Where an auto-spawned daemon lands" below); the running daemon's own executable is the one being
replaced, so it cannot name its successor. Over `--host` that path belongs to the wrong machine, so a remote upgrade
runs the remote host's own `felis` through `ssh`. Nothing starts an upgrade implicitly: a newer client connecting to an
older daemon is ordinary minor skew, and quiescing every session on a connect is not a side effect a client earns.
`felis doctor` reports a running daemon built from other than the installed binary, and the user decides when to
upgrade.

Connected clients see one transport loss. The listen socket stays open across the exec, so a re-dial during the gap
waits in the kernel's accept queue instead of finding a cold socket and spawning a rival daemon, and the successor
restores the session registry before it accepts, so a window re-attaching its session never reads `UnknownSession`
([session-lifecycle.md](session-lifecycle.md) "Transport loss"). A stream client gets no such continuation:
`send --wait` waits for a command-end mark that is streamed live and never replayed
([control-surfaces.md](control-surfaces.md) "Waiting is `send --wait`"), so it reports the session ended, and a
notification stream ends. Telling those clients the close was an upgrade would take a new announcement on the wire and a
new outcome in every stream verb, to cover a case the update procedure already tells the user to avoid.

Windows has no exec to survive. A ConPTY pseudoconsole belongs to the process that created it, no supported API hands it
to another process, and closing it ends the console's children; so Windows keeps drain-and-restart for now. That is a
gap felis means to close, not a line it draws: an update that keeps the sessions is the same intent on every platform
([non-goals.md](../non-goals.md) "Cross-platform constraints"). The one route known today is a long-lived process that
holds the pseudoconsoles in front of the daemon, so that the daemon can be replaced while the consoles live on; it is a
third process with its own lifetime and protocol, and nothing in this design pays for it yet.

Running each upgrade as a new daemon generation on its own endpoint, with its predecessor serving its sessions until
they end, is rejected. It needs no dump, but it leaves exactly the long-lived sessions on the predecessor's binary for
as long as they live: the same user sees a fixed bug in one window and not in the next, every roster and session verb
has to span generations, and the predecessor never retires. It gives up the property an upgrade exists to deliver, that
every session runs the installed daemon.

Carrying the dump on the attach wire is rejected too. The rehydrate burst is a lossless picture of the visible screen,
not of the session: scrollback, the screen saved behind the alternate one, and parser state are not on it, and growing
the wire to carry them would make daemon-internal state a public compatibility surface. The dump reuses the wire's row
codec and image chunks as encodings, recorded with their versions inside the dump, without joining the wire's contract.

### Where an auto-spawned daemon lands

A daemon started by the client inherits the launching process's cgroup at fork, and keeps it for its whole life: cgroup
membership survives the reparenting to init that detaching the child performs. On a Linux desktop the launcher is
usually a unit of the systemd user manager, and a unit's `OOMPolicy` acts on every process in its cgroup. An OOM kill of
one shell child inside a compositor's `niri.service` therefore stops the compositor, ending the graphical session, and
which unit any daemon belongs to is decided by whichever window happened to find the socket cold first.

felis therefore hands the daemon to the user manager where one is reachable: a launcher whose own cgroup names
`user@<uid>.service` runs `systemd-run --user` on **its own `felis-daemon` binary and its own resolved socket path**.
The unit's shape is in [cli.md](../../reference/cli.md#auto-spawning). Where the hand-off succeeds, the placement is a
property of felis rather than of the window that dialed first, and the failure that motivated all of this cannot happen
to that daemon: its cgroup is its own.

#### Best effort, with the fork as the fallback

A hand-off that fails for any reason (a manager that refuses, one that stops answering inside the bound each call
carries, a start that reports ready but leaves the socket cold) forks the daemon, which is to say it lands in the
launching unit's cgroup and carries that unit's `OOMPolicy`: the fallback keeps the risk the hand-off removes, and the
logged reason is what says so. One failure is not a fallback: a managed start whose endpoint then refuses the dial for
any reason other than "nothing is listening" ends the dial with that error, because forking there would put a second
daemon beside whatever holds the endpoint.

The alternative, refusing to fork past a present manager, is rejected: it has to be right about every way a manager can
be present or absent (cgroup namespaces, sandboxes, masked units, paths D-Bus cannot carry), and each case it gets wrong
is a window that does not open. The fallback owes no such precision, because its worst outcome is the unmanaged
behavior.

What the hand-off does guarantee is the positive case, and `Type=notify` is what guarantees it: `systemd-run` exits `0`
only once that binary has bound that socket, which is why `serve` sends `READY=1` after its bind and treats a failed
send as a startup failure.

Each remaining property of the unit answers a failure the hand-off can produce. `TimeoutStartSec=15s` replaces the 90 s
default, because a `serve` that never reaches `READY=1` is broken and every concurrent launcher waits on that same start
job. `--collect` unloads a unit that ends inactive or failed, so a failed start leaves no name to reset before the next
window tries. `KillMode=process` is rejected, which is why the unit keeps systemd's default: it would leave a surviving
child holding the cgroup, and the next start would then fail on a name that is still loaded.

#### Launchers outside the user manager fork

A launcher that is _not_ under the manager forks, which is what a remote-attach host does: the relay is not under the
manager, so its daemon is a plain child with no `OOMPolicy=stop` unit to be killed by. The endpoint is the same for both
and outlives either, so what separates them is the process's own lifetime, and that lifetime is logind's policy and the
user manager's contract rather than anything felis holds ([cli.md](../../reference/cli.md#auto-spawning)). macOS,
Windows, and non-systemd Linux fork because nothing there couples a daemon's resource policy to the window that started
it.

#### Designs that reach further

Two designs that reach further are rejected:

- **Socket activation** starts a daemon on every connect, which the per-verb auto-spawn contract
  ([cli.md](../../reference/cli.md#auto-spawning)) cannot survive, and needs fd adoption and a bind-race guard felis
  would own forever.
- **A shipped unit file** pins `ExecStart` to one installed binary, losing the sibling-binary guarantee that makes
  `nix run`, a source build, and an SSH relay each start _their own_ daemon.

_Revisit if_ felis adopts idle-daemon exit and re-draws the auto-spawn table so every dial may start a daemon.

#### Deliberately not built

Each of these answers a need nobody has reported: a hand-off for the SSH relay, a `supervisor` field in `daemon status`
(diagnosis is `systemctl --user list-units 'felis-daemon-*'` plus the logged reason), `loginctl enable-linger` handling,
and cgroup v1.

_Revisit if_ a fallback fork is observed reproducing the OOM failure in a real deployment, or users cannot tell which
kind of daemon they have.

#### Sandboxed launchers

A process with a mount namespace of its own (`PrivateTmp=` services, bwrap or flatpak sandboxes) sees a different
`/tmp`, so its autospawn starts a daemon only it can reach, and its systemd hand-off asks the user manager, which runs
in the host namespace and therefore binds the host's endpoint the launcher cannot then dial. The result is a host-side
daemon the sandbox cannot see, visible to `felis doctor` from any ordinary shell.

### Crash recovery

A controlled stop and a crash do not share a recovery story. A crash loses every session, because the daemon holds no
on-disk session state. Persisting enough to recover is deferred. The upgrade dump ("In-place upgrade" above) lives in
memory for one exec; recovering from a crash would make it a durable on-disk format with every version a later daemon
might meet, and image bytes alone are a memory-vs-disk trade. The crash mode of a long-running daemon is mostly "process
killed by the OS for memory pressure", which is itself a sign of a bug.

Lost session _state_ does not mean every session _process_ dies, and the gap is what a daemon cannot close from the
outside ([session-lifecycle.md](session-lifecycle.md) "When the daemon ends"). A daemon running as a transient unit of
the systemd user manager gets one backstop: the manager kills the unit's whole control group, so a child that outlived
the hangup ends there rather than being orphaned. A forked daemon has none. Reclaiming such orphans at the _next_ daemon
start is rejected: recognizing felis's own orphans from ambient process-table state alone is guesswork whose wrong guess
signals a process that was never ours. Revisit together with the persistence above: on-disk session state would double
as the manifest a post-crash reclaim needs.

### The record is a file, not a console

GUI launch paths hand both binaries `/dev/null` for the console, so a console-only subscriber (and the default panic
hook, which also prints to the console) records nothing in exactly the runs users have. Logs therefore tee into a
per-user file and panics route through `tracing` (locations in [cli.md](../../reference/cli.md) "Log files"). Because
that file carries every line, the autospawned daemon nulls all three standard streams rather than inheriting the
caller's stderr. Inheriting is rejected: the daemon detaches and outlives its caller, so it would hold the write end of
a pipe whose reader has gone, and the next log event's `EPIPE` escalates to a whole-daemon abort. A developer running
`felis-daemon serve` directly keeps a live console, and a daemon the systemd user manager started writes its console to
the journal, which no client can close under it.

Rotating the file on every start is rejected twice over: concurrent clients share `client.log`, so each new window would
rename it out from under the running ones, and a crash-looping daemon would erase the previous run's evidence, the
record the file exists to keep. What bounds growth instead is a size gate at open ([cli.md](../../reference/cli.md) "Log
files"): a crash loop never trips it, since each run appends little on top of an un-renamed file, and the rare rename
under running windows diverts their subsequent lines into the rotated generation rather than losing them. Within one
long daemon run the file still grows unchecked: accepted, because a mid-run rotate would reintroduce the shared-writer
problem on every crossing, and restarts (upgrades included) are what paces the file back through the gate.

The crash recorder must not itself crash. Routing panics through `tracing` makes the recorder write to the same sinks
the running daemon does, including a stderr that may be a broken pipe. Two guards keep a sink failure from turning a
contained panic into a process abort. The subscriber runs with `log_internal_errors(false)`, so a failed write is
dropped instead of `eprintln!`d onto the same broken stderr (whose own failure would panic), and the file sink still
records the event first, since `Tee` writes both halves before it propagates the error.

The panic hook then wraps its `tracing::error!` in `catch_unwind`: a panic raised _while_ a panic is already unwinding
aborts unconditionally ([`std::panic`](https://doc.rust-lang.org/std/panic/index.html)), so the second panic is
swallowed and the first still reaches `prev` and unwinds into the watchdog that cleans the session up
([session-lifecycle.md](session-lifecycle.md) "Session-task panics").

## Why two processes

Three reasons, in priority order:

1. **Window death must not be PTY death.** A single-process design ties process lifetime to window lifetime; the daemon
   split decouples them.
2. **Mechanism vs. policy.** The daemon stores raw state; the client decides how to render it ("The daemon / client
   split" below is that boundary's record).
3. **Cross-host.** Because the boundary is a wire protocol, the daemon can run on a remote machine with the frames
   tunneled over SSH stdio ([ipc.md](ipc.md) "Cross-host attach: SSH stdio" carries the persistence argument).

## The daemon / client split

When a new feature lands, it must be assignable to exactly one side. If something seems to belong "in between," that is
a design smell: either it does not belong in felis at all, or the boundary is being drawn wrong.

### The principle

> Daemon = mechanism. Client = policy.

- The daemon stores the _truth_ about a session: the bytes that came out of the PTY, the parsed cells, the images, the
  cursor position.
- The client decides the _appearance_: which font, which colors, what cursor shape, how to shape ligatures.

A single daemon can be attached by multiple clients with different fonts and color schemes: in sequence or, for
same-user clients, simultaneously (see [session-lifecycle.md](session-lifecycle.md) "Same-user mirroring"). The state on
the wire must be font-and-color-agnostic either way; the same wire state drives several live renderers at once.

### Responsibility table

The principle assigns most concerns on sight: the PTY, the parser, the grid, the scrollback, the image store and the
session's own lifecycle are the daemon's; the window, the input devices, the fonts, the atlas, the draw calls and every
per-window animation are the client's. Seven assignments do not follow from it.

| Concern                        | Owner  | Notes                                                                                                                                                                                    |
| ------------------------------ | ------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Damage tracking                | daemon | The daemon's damage decides what ships; the shadow marks the rows it applies, and the renderer rebuilds only those ([damage-tracking.md](../rendering/damage-tracking.md)).              |
| Selection                      | client | An anchor/extent pair over the shadow screen; no selection state crosses the wire ([input.md](../input.md)).                                                                             |
| Keymap → action lookup         | client | An action reaches the daemon only when it needs a session-side effect ([input.md](../input.md) "Action mapping").                                                                        |
| Key encoding                   | daemon | The client captures the keystroke; the bytes depend on keyboard modes only the daemon has parsed ([input.md](../input.md) "Keyboard").                                                   |
| Clipboard                      | client | The OS clipboard is the window's; the daemon never touches it ([security-model.md](../security-model.md)).                                                                               |
| Default colors and OS scheme   | both   | Pixels are the client's, but it reports its resolved trio so the daemon can answer a program's query ([session-lifecycle.md](session-lifecycle.md) "Client-derived presentation state"). |
| Authentication / authorization | daemon | Local socket permissions, or the SSH carrier cross-host ([security-model.md](../security-model.md) "Daemon IPC").                                                                        |

Per-cell pixel buffers, client font selections, and configuration synchronization never cross the IPC wire. Each side
reads its own declarative configuration independently; there is no runtime settings replication message.

### Implications

#### No shared memory

The boundary is the IPC protocol, full stop. Even when the daemon and client run on the same host, they communicate over
the local carrier (a Unix socket, or a named pipe on Windows): never through `mmap`, never through a shared file. This
keeps cross-host attach (over SSH stdio) trivial: the same code path applies whether the daemon is local or remote.

The one shared-memory path in the system runs the other way and stops before the boundary: a producer hands the daemon
image pixels through a POSIX shm object (the Kitty `t=s` transfer mode), which the daemon copies into its image store
([data-model/image-store.md](../data-model/image-store.md)). Pixels leave the daemon for a client as protocol frames.

#### The daemon is color-blind

The daemon stores cell attributes as protocol-level values: SGR parameters, palette indices, and 24-bit RGB triples that
came out of the PTY. It does not interpret them as "the user's red" or "background color". The client maps
protocol-level color to actual pixels.

A config change on the client (theme reload, palette swap) does not invalidate any grid state in the daemon. The one
deliberate exception to "the daemon does not know": the client reports its _resolved_ default fg/bg/cursor on attach
(`SessionToDaemonMsg::ConfigureTheme`) and its OS light/dark preference (`InputMsg::ColorScheme`), because the daemon
owns the query surface: a program's `OSC 10/11/12 ; ?` or `DSR ? 996 n` must answer with the real surface colors, and
the program talks to the daemon, not the client. The daemon stores the reports opaquely (it still never interprets a
color) and **per window**, then resolves the one answer a query gets along a fixed candidate chain. Which mirror's
colors a program sees is therefore a property of the chain, not of whose attach frame landed last, and an empty chain
answers with the compiled-in fallback ([session-lifecycle.md](session-lifecycle.md) "Client-derived presentation
state").

#### The daemon is font-blind

The daemon does not know which font the client is using. The grid is indexed by _cell coordinates_, not pixels. The
client decides cell width in pixels by measuring its current font, then sends the grid size in cells back to the daemon
(via a resize message). The daemon asks the PTY to resize.

A client can attach to a daemon, change fonts, and trigger a soft resize at any time, without any other state moving.

#### The client is stateless across runs

When the user closes the client window, the next client process starts empty. It connects to the daemon, requests a
rehydration frame, and rebuilds its glyph atlas, image cache, and shaping cache from scratch. Nothing important lives
only on the client.

This is intentional: it forces the boundary to be honest. If something "feels like" it should be cached on the client
between runs, that caching can be re-derived on a fresh attach in milliseconds, or it should not exist.

### Boundary smells

If a feature design produces any of the following, the design is wrong:

- Daemon code that mentions a font or a color space.
- Client code that holds authoritative session state.
- A peer that _papers over_ version skew silently: a mismatched frame reinterpreted as something the reader does
  understand, a field read under one major's meaning and applied under another's, or an open-ended compatibility matrix
  carried to decode every past shape of the wire. Refusing a major the daemon does not serve is **not** the smell: the
  preface refuses it loudly before a frame is written ([ipc.md](../../reference/ipc.md) "Versioning"). Neither is a
  **declared** deprecation window, in which the daemon runs one decoder per major it still serves and each frame is
  decoded by the schema its connection agreed on: two decoders that never see each other's bytes are two honest paths,
  not a paper-over. The smell is skew handled where it cannot be seen: inside a decoder, per field, without the
  connection having agreed which major it is speaking.
- A "fast path" that bypasses the IPC layer for performance.

The right response to a smell is to redraw the boundary on paper before writing any more code.

## Prior art and alternatives considered

felis internalizes session persistence in its own daemon and delegates layout to the WM. That places it against a long
line of multiplexers and detach/attach tools, and one axis decides the whole design: what a client sees when it
attaches.

**Replay-based attach** (tmux, GNU Screen, zellij) re-emits synthetic VT sequences to repaint the attaching terminal.
That terminal is then a second emulator interpreting the multiplexer's rendition of a screen the multiplexer had already
interpreted once, and wherever the two interpretations differ the screen drifts. This emulation-of-emulation bug class
is the standing argument for internalizing persistence rather than composing with a multiplexer: felis ships a
structured grid over IPC and **rehydrates from a snapshot**, never a replay
([../data-model/scrollback.md](../data-model/scrollback.md), [session-lifecycle.md](session-lifecycle.md)). WezTerm's
mux, mosh, and zmx each arrive at the same snapshot conclusion from different starting points, which is the strongest
evidence available that the axis is real rather than a preference.

What felis borrows, by source: the detach/attach idea and Unix-socket IPC (tmux); the principle that detach should be
cheap (abduco/dtach); the length-prefixed frame shape (zellij's IPC is well-engineered); the existence proof that native
cross-host attach is viable in a Rust terminal (wezterm-mux); the rehydration burst, snapshot-ship-never-replay (zmx;
the internalize-vs-compose-with-zmx question resolves to internalize); and the lesson that network resilience and grid
fidelity can be solved together (mosh).

The scope felis refuses in this space (layout primitives, scripting, multi-user attach, mosh-style UDP roaming,
disk-backed session state) is argued in [non-goals.md](../non-goals.md). One refusal here is architectural rather than
scope: the **passthrough-client model** of abduco and dtach, where the daemon holds a raw byte stream and the client
parses it. felis's client owns the renderer, and a snapshot needs a screen to snapshot, so the daemon has to own a
_structured_ screen. Adopting the passthrough shape would move the parser back into the client and take the snapshot,
the multi-client fan-out, and every non-GUI consumer of the grid with it.

## Workspace: the crate-boundary decision record

The canonical crate map (per-crate contracts, the dependency diagram, extraction sets, reuse paths, Cargo configuration,
versioning) lives in [reference/workspace.md](../../reference/workspace.md). This section records why the boundaries sit
where they do.

Each crate is a seam: a boundary that a future repository split, or a non-Rust client, must be able to reuse in
isolation. The "MUST-NOT-depend-on" column of the crate map is the load-bearing one: it is the extraction criterion.
Some seams are aspirational (`felis-render-wgpu` only earns its keep when a non-wgpu renderer exists) and cost a small
amount of API plumbing while only one consumer exists; that is the accepted price of extractability.

### Rejected workspace shapes

- Flat layout (`src/{daemon,client,protocol}/` in one crate): cheap today, expensive to split: every internal `pub`
  boundary becomes a public API at extraction time, and the version graph has no place to record incremental client /
  protocol releases.
- Two crates (`felis-daemon`, `felis-client`) plus a shared `felis-common`: better than flat, worse than this layout,
  because a consumer of _just_ the parser or _just_ the grid still pulls in everything in `felis-common`.
- Daemon-as-library with no separate binary: useful for embedding, but the user runs `felis-daemon` from the shell.
  felis keeps both: the binary is a thin wrapper around the library entry points, so a test harness can still spawn the
  daemon in-process.
- Workspace member glob `crates/**/*` with nested workspaces: adds complexity for nothing (`fuzz/` already needs to be a
  separate workspace so the nightly-only libFuzzer dependency does not propagate); the crate members stay `crates/*`.
- A guard on a workspace-wide invariant hosted inside whichever crate is convenient: a test walking the workspace root
  behaves the same from any member, so the choice looks free. `crates/*` is the extraction set, though, and such a guard
  is precisely what a repo split has to un-pick: it fails on the first checkout of the extracted crate, having never
  tested anything about it. The guards live in `tests/` instead, a member outside the crate tree with no library or
  binary target, so their address matches their scope. Ordinary cross-crate tests are the opposite case and stay in
  their crates: they assert something about a seam a crate owns, over a dev-dependency edge that is already declared.
- A dev-only `felis-test-*` crate of shared test helpers: it ships nothing, so a fixture written once for every crate's
  tests looks free. Every crate under `crates/` is an extraction seam, though, and a helper crate their tests dev-depend
  on is an edge each extracted crate has to carry into its new repository or un-pick, for code that tests nothing about
  that crate. A crate's tests keep their helpers beside them instead, and a test that reaches above its own crate root
  belongs in `tests/` (the guard shape above). _Revisit if_ a fixture copied into several crates' tests diverges between
  the copies in a way that lets a defect through.
- A crate between `felis-protocol` and `felis-transport` to hold the shared connection driver: one module in a crate of
  its own, drawing a seam a repo split would have to un-draw. The driver lives in `felis-transport`, the first crate
  that already has both the protocol vocabulary and `tokio` ([ipc.md](ipc.md) "Why the driver lives in
  felis-transport").

### The felis-client-core / felis-client seam

The client side is two crates, split GPU-free / GPU. `felis-client-core` (`crates/felis-client-core/src`) holds the
portable session logic: the shadow screen, the daemon connector and retry policy, pull pacing, keymap and action
resolution, selection, the session roster, viewport state, daemon spawn, and config. `felis-client`
(`crates/felis-client/src`) is the winit/wgpu shell around it: the event loop and window (`event_handler`,
`macos_window`), the OS clipboard, and the render wiring. The seam exists because the session logic is reused by
consumers that have no window or GPU: `felis-cli`'s headless verbs, the `felis-web-component` wasm core, and `felis-tui`
all link the core and substitute their own front end. `connect_or_spawn_daemon` sits in the core for the same reason:
both front-door binaries share one autospawn policy without dragging the daemon backend into the client extraction set.

The same holds for the renderer state a grid message changes beyond the shadow (theme and palette overrides, reverse
video): `renderer_effect` maps each message to a `RendererEffect` value that every frontend applies to its own renderer.
It returns data rather than calling a renderer trait because the core cannot name a GPU renderer, and a trait defined in
the core could not be implemented for one in a frontend crate without a newtype in each. A copy of the mapping in each
frontend is the rejected alternative: the copies drift apart, and a message one of them misses renders differently.

### The ScreenBuffer seam inside felis-grid

`felis-grid` exposes two types where a terminal buffer might look like one. `ScreenBuffer` is the cell ring with its
geometry, cursor, and the interned tables a cell's payload indexes into. `Grid` wraps one and adds the VT parser's
state: modes, tab stops, charsets, saved cursors, scroll regions, title and working directory, palette and theme
overrides, notification and clipboard outboxes.

The line between them is the wire. A field belongs on `ScreenBuffer` when a `GridMsg` carries it to an attached window,
and on `Grid` when only the parser reaches it. That gives the daemon and the client one shared type for the surface they
both hold, instead of a mirror the client reimplements and the two drift on. It also deletes state from the client
rather than hiding it: a shadow that holds a `ScreenBuffer` has nowhere to put a title or a mode bit, so the parser
fields it never writes are absent rather than present and stale.

Two consequences fix the boundary in place. The cell-id tables travel with the cells, because a `Cell` resolves to
nothing without them: its style, link, and sizing handles and a cluster grapheme's id all index tables that have to
arrive alongside the cells referencing them (see [grid-and-cells.md](../data-model/grid-and-cells.md)). Damage travels
with them too, since it is indexed by ring row and resized in lockstep; leaving it on `Grid` would fork every cell write
into a marking and a non-marking variant, one for each side of the wire.

Three operations straddle the seam, and each keeps its parser half on `Grid`. A resize re-tracks margins and tab stops
after the ring moves. The style sweep and the sizing sweep re-establish the one interned handle the parser holds outside
any cell (the pen memo and the in-flight `OSC 66` run). `Grid` owning both halves of each is what lets `ScreenBuffer`
stay free of parser concepts.

Rejected: grouping `Grid`'s fields into sub-structs for organization. Field-level grouping buys nothing on its own,
because nearly every access site names one field rather than a group, and it would cost a rename at every one of them.
`ScreenBuffer` is not that refactor: it earns the churn by being a type two crates share across a wire, and by removing
state from the client instead of relabeling it.

On the client the marks are load-bearing: the renderer rebuilds only the rows the shadow marked since the last paint
([damage-tracking.md](../rendering/damage-tracking.md) "Client repaint"), so one write path marking for both sides is
what keeps a shadow write from ever skipping its repaint.

### Reuse paths for non-Rust clients

The concrete reuse-path table is in [reference/workspace.md](../../reference/workspace.md); what it records is a policy.
felis core ships no `examples/` bridge crates: each reuse path is realized by a sibling repo in the ecosystem, built
only because a real consumer exists (principle 1: add only what earns its place). A client whose language can generate a
protobuf reader speaks the wire directly; one whose language cannot (`felis.el`, and any script in the same position)
reaches the daemon through `felis bridge`, the CLI's JSONL stdio subcommand, instead of through a second wire encoding
([ipc.md](ipc.md) "One encoding on the socket, JSON at a bridge").

### Shared wire knowledge across the satellite clients

An ecosystem of downstream clients reads a felis session: `felis-web-component` (a wasm `felis-client-core` core plus
the TS `<felis-terminal>` view) with `felis-web-gateway` in front of it (a native attach re-exposing a session over a
WebSocket), `felis-fcast` (a `.fcast` recorder plus a `serve` pseudo-daemon), `felis-tui` (a host-terminal multiplexer),
and `felis.el`, which reaches the daemon through `felis bridge` rather than the socket. Four pieces of wire knowledge
recur across them; the sections below record where each belongs.

#### Structural session JSON: adopt as a named format, `felis-json` v1 in `felis-grid`

Two out-of-repo consumers mirror a whole session as JSON: `felis-web-gateway` decodes wire→JSON for a separately
deployed browser view, and `felis-fcast` writes frame payloads into `.fcast` files and encodes JSON→wire to replay them.
Both need the same two conversions, and both have to recode a `GridMsg`'s opaque row payload, since serde cannot
transcode bytes `felis-grid` pre-encoded. So the conversion is shared, in `felis-grid::json_v1` behind the `json`
feature, and it is a **named, versioned format** rather than a helper: an in-band `felis_json` version, dedicated DTOs,
a generated schema and golden frames ([ipc.md](../../reference/ipc.md) "Structural session JSON").

The DTOs are the point, and they reach all the way down to a dirty row's cells. Publishing the daemon's own Rust serde
shape would make every variant or field rename a silent downstream break, and leave validation and compatibility
inherited from implementation types; an explicit DTO makes a rename fail the conversion to compile instead. It is also
where the format refuses what it cannot represent: a reserved bit in a bitmap is an error rather than a bit the reader
drops, which would otherwise re-encode as a different frame.

It sits in `felis-grid`, not `felis-protocol`, because the row recode needs the row codec (`felis-grid::wire`) and
`felis-protocol` is deliberately opaque to the per-cell layout; every consumer already links `felis-grid`, so it adds no
dependency edge. Nothing in the module reaches a socket.

The alternative was to keep the conversion an internal helper and let each outer format own its own envelope and DTOs.
It is less code here, but it hands the same five message families to every satellite to re-describe, which is the drift
a shared engine exists to prevent, and the two satellites that exist already depend on this conversion by git rev.

The compatibility rule the format carries is stricter than the wire's additive minor (REQ-104b): a socket peer
negotiates an effective minor and can be told what the other side understands, while a `.fcast` file read two years
later, and a browser view deployed separately from the gateway feeding it, can be told nothing. So inside a version the
only permitted change is an optional field readers ignore, and everything wider is a version bump the reader refuses on
sight.

The families only `felis bridge` renders (`Ops`, `Region`, `Notify`, `Push`, `Search`) are not in the format: they live
in `felis-cli` under the CLI's own `v: 1` contract, which already publishes their schemas.

_Revisit if_ a consumer needs a family outside the five the format covers, or if a second format version is ever wanted
at the same time as a wire major, when whether the two track each other is worth deciding rather than inheriting.

#### Grid→ANSI re-encoding: adopt, into `felis-grid`

Turning stored cell state back into escape sequences recurs in three places: the daemon's pipe action,
`felis sessions capture --ansi`, and any client painting into a host terminal (`felis-tui`). All of it lives in
`felis-grid::ansi` (`row_ansi`, `row_ansi_with`, `sgr_set`), which sits there rather than in `felis-protocol` because it
reads `Cell`, `StyleTable`, and the cluster and hyperlink tables, the per-cell layout `felis-protocol` is deliberately
opaque to. The host-specific decisions (256-color down-map, clip and pad to a column count, blank the Kitty
Unicode-placeholder cells the caller covers with an image) are caller-supplied options, so the daemon still makes no
presentation choice (principle 3). A round-trip test driving the emitted bytes back through the real parser keeps it
honest in-repo.

Two alternatives are rejected:

- **A per-client emitter.** Emitters drift: one that never learns underline styles, underline color, conceal, or
  overline renders those dimensions in the GUI client and drops them through the TUI.
- **The feature gate its `json` sibling carries.** That gate exists to make `serde_json` optional, and ANSI emission
  adds no dependency to save.

_Revisit if_ a consumer needs an emission mode that would drag client policy (per-host quirk tables, terminal-specific
workarounds) into the shared module; that policy belongs in the client, which composes it around the encoder rather than
inside it.

#### Wire constants: the schema is the authority, prose covers the bytes around it

A client that speaks the wire without linking the Rust crates has to agree on more than message shapes, and
`felis.proto` (package `felis.v1`) is the single machine-readable authority for almost all of it: the families, the
`FrameKind` routing values, and the protocol major and minor, each pinned against the Rust constants by a test that
reads the file. A client generates its codec from it (the analogue of the `protoc-gen-prost` the Rust crates run).

Which arms that client may send, and in which mode and phase, rides the same descriptor: each oneof arm carries a
`(felis.v1.arm)` field option holding its routing row, so the matrix a peer must implement is read with the reflection
it already has rather than parsed out of comments ([reference/ipc.md](../../reference/ipc.md#the-arm-table)). Options
are invisible to `buf breaking`, so the Rust arm-table test is what holds the two in agreement.

What a schema structurally cannot hold is the bytes _around_ the protobuf bodies, because protobuf is not
self-delimiting on a byte stream (the same reason gRPC prefixes each message with its own 5-byte frame): the fixed-size
little-endian frame header, the big-endian preface layout, and the keyboard/capability bit values. Their normative home
is [`ipc.md`](../../reference/ipc.md); they are a tiny frozen set, so a client hard-codes the handful it needs.

Rejected: a hand-reflected manifest (`felis_protocol::contract` emitting a committed `contract.json`). Its shape
sections duplicate the proto and drift from the generated casing, and once shapes, kinds, and version constants all come
from the schema, its only remaining job is the byte layouts above.

_Revisit if_ a client appears that can neither run a `protoc-gen-*` nor reach the daemon through `felis bridge`; the
answer then is to ship the `FileDescriptorSet` the schema already compiles to, not a hand-reflected manifest that
drifts.

#### Server-side handshake reuse: adopt the functions, not a crate yet

`felis-fcast`'s `serve` hand-copies the daemon's opening sequence: the frozen preface, then
`Hello → Welcome → attach-ack` ([reference/ipc.md](../../reference/ipc.md) "Handshake"). That is the one surface where a
pseudo-daemon's drift stays invisible until a real client refuses to connect, so the handshake helper is shared rather
than copied.

**Rejected for now:** a full `felis-server-core` crate mirroring `felis-client-core`. There is one server-side consumer
today (`serve`); a whole crate for one consumer fails principle 1 ("if no real consumer needs it yet … the terminal does
not implement it").

_Revisit if a second server-side consumer appears_ (a second pseudo-daemon, or a record-and-forward proxy), at which
point the shared functions graduate into `felis-server-core`.

### Revisit triggers

The crate split is settled, but three developments would reopen it:

- Profiling shows compile-time pressure from too many crates.
- A second renderer (software / OpenGL / Metal-direct) is built: reconfirm `felis-render-wgpu` earns its keep as a
  separate crate.
- `felis-grid` reuse demands splitting cells / scrollback / image store into separate crates. The image store is folded
  into `felis-grid` because reflow, placement remapping, and placeholder resolution all read the grid and the store
  together; splitting later is a refactor, not a redesign.
