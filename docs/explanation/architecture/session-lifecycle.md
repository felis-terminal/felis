---
title: Session lifecycle
sidebar:
  order: 2
---

A _session_ is the unit of work the daemon owns: one PTY, one running shell, one grid + scrollback, and one image store.

The page follows a session through its states: how it is created, what an attach ships, what detaching leaves running,
and how a session ends, first its shell and then the session itself. It then covers how one window moves between
sessions and between daemons, how several windows share one session, and what happens to sessions when the daemon ends.

## States

```
   spawn
     │
     ▼
  ┌───────┐  attach  ┌───────────┐ detach ┌──────┐  destroy  ┌──────┐
  │ Init  │─────────►│ Attached  │───────►│ Idle │──────────►│ Dead │
  └───────┘          │           │◄───────│      │           └──────┘
                     └───────────┘ attach └──────┘             ▲
                                            │                  │
                                            │ child has exited │
                                            └──────────────────┘
                                              and ~5 s grace lapses
```

- **Init:** PTY spawned, shell started, registered in the pool, but no subscription yet. Not observable to a `Create`'s
  caller, which is answered only once its own subscription landed; an `OpsToDaemonMsg::Spawn` passes straight through it
  into Idle.
- **Attached:** one or more same-user clients subscribe to grid updates and send input (see "Same-user mirroring").
- **Idle:** no subscriber. Output continues to land in the grid and scrollback.
- **Dead:** the session is out of the pool and its resources are freed.

Whether the child is still running is a flag orthogonal to those states, not a state of its own. A session whose shell
has exited keeps its grid, its scrollback, and its images; it can still be listed, attached to, searched, and captured
("Post-exit reaping"). Reaping requires both conditions at once, the child exited _and_ zero subscribers past the grace,
so neither an exited session someone is reading nor an idle session whose shell is alive is eligible.

Folding exit into `Dead` is the alternative, and it loses the case users hit most. A shell that exits while its window
is closed is exactly what the grace keeps readable, and a model where exit _is_ death has no name for what is being
kept. Orthogonality also states the reaper's condition in one line rather than as a qualified transition.

Every state is owned by one per-session task in the daemon (`serve::session_task`): it holds the parser, grid, image
stores, and PTY halves, and runs the PTY→grid pipeline continuously whether zero, one, or N clients subscribe.
Connections never take ownership of a session; they register as subscribers over the task's command channel.

## Creation

A session is created one of two ways, and which one a caller picks is decided by whether it wants a window.
`SessionToDaemonMsg::Create` creates _and_ attaches the requesting connection in one step; `OpsToDaemonMsg::Spawn`
creates a session nobody attaches to, which is what a scripted caller wants and what the GUI never does. The message
shapes, the argument record, and every refusal variant are in [reference/ipc.md](../../reference/ipc.md) "Message
families"; [ipc.md](ipc.md) "Creating a session" argues why the split runs this way. Session IDs are not human-named
(see [non-goals.md](../non-goals.md) on user-namespaced session catalogs).

Both take the same arguments and run the same sequence, differing only in whether a subscription follows it. The order
of that sequence is the part that is a decision.

### Admission comes before allocation

Two admissions run before anything is allocated. The requested geometry is judged first ("Geometry bounds" below), then
one session slot is taken against the daemon's aggregate cap (REQ-915); the connection carrying the request was already
admitted against the connection cap when it was accepted (REQ-916, [security-model.md](../security-model.md) "Daemon
IPC"). Refusing before an id exists is what makes an impossible create cost nothing but the frame it arrived in.

The slot is _taken_ rather than the pool merely counted, because the session registers only after the fork/exec. A check
that read the count and let go would admit every create in a concurrent burst, and the pool would settle at the cap plus
the burst width. The slot releases as the handle registers, under the same lock, and drops if the spawn fails.

### Registering and subscribing are one transaction

Registering the session and subscribing the creating connection are then one transaction. If the subscribe fails the
daemon removes the pool entry, shuts the session down, and waits for the child to be reaped before writing the refusal.
Otherwise a failed window launch leaves a live shell whose id nobody was ever told: unreachable, and still counted
against the cap.

### The refusal names which half refused

That transaction is why a create has two failure vocabularies rather than one, and why the refusal names the half that
refused. Its **spawn half** (admission and the fork/exec) can fail over a geometry, a cap, a drain, or a program that
will not start, and all four say the same thing about the request: nothing ran, and no session exists. Its **attach
half** can fail only by losing the race against a child that ended first. Every other attach-half reason (an id nothing
holds, a prefix matching none or several, a shell that has already exited) is about a session that was already there,
which is `SessionToDaemonMsg::Attach`'s business and not a create's.

The two sets are therefore disjoint, an `OpsToDaemonMsg::Spawn` has no attach half at all, and
`SessionToDaemonMsg::Attach` has no spawn half; so the wire carries one enum per half and the refusal names the arm
([reference/ipc.md](../../reference/ipc.md) "Session (kind = 4)").

Each request's admissible values then follow from its shape, which is what lets a client check them: an attach naming a
full id can be told the id is unknown, an attach naming a prefix can be told the prefix matched none or several, and
either can be told the shell has exited only if it asked to be. A refusal outside that set is a daemon this client is
not speaking the same protocol as, so it is reported as a protocol failure rather than passed on as a refusal: a merits
decision felis reports is one the daemon actually took.

Two other shapes are rejected:

- **One enum with the per-operation subsets documented.** It is the cheaper edit, but a note does not remove a value
  from the type a caller matches, so every reader of either reply still carries a nine-way match and decides by hand
  which half of it can arrive, which is the mistake the note exists to prevent.
- **A `SessionCreateFailed` family arm for the spawn half.** The success twin `SessionAttached` is shared, so a second
  failure arm would add a routing row to discriminate what the oneof on the existing reply already discriminates. Two
  reply messages with a second success message alongside them doubles the create/attach surface to fix one enum.

### Omitted arguments default; degenerate ones are refused

Every argument the caller omits falls back to a daemon default, an empty command meaning a plain `$SHELL`, and the
default-shell factory takes the directory as a parameter rather than choosing both together, since which shell to run
and where to run it are independent choices. Present-but-degenerate is not the same as absent: a geometry naming zero
rows is refused rather than defaulted, and an argv naming no program at all is refused before any spawn rather than
allowed to exec-fail on an empty program name.

### Working directory

The front doors fill `cwd` rather than send it empty, because an empty `cwd` inherits the daemon's own, which is a poor
front-door default. The daemon is long-lived and its cwd freezes at auto-spawn (on macOS at the filesystem root, since
the OS launches GUI apps there; at the user's home when the systemd user manager started it), so every fresh window
would open somewhere unrelated to the window that asked.

A GUI launch uses the client process's own cwd, falling back to `$HOME` when that is `/`: a Finder/Dock launch lands at
root with no shell intent behind it, whereas a terminal launch (`felis`, which inherits the shell's cwd) has a real
directory to carry. A chosen directory that is not valid UTF-8 fails the launch with a named error instead of falling
through to the next candidate: `cwd` is a wire `string` (REQ-105b), and moving on quietly would open the window
somewhere the user did not ask for.

`felis sessions spawn` inherits the caller's cwd for a local daemon (`cd dir && felis sessions spawn` lands the session
in `dir`, matching tmux `new-window`) and honors a deliberate `/` rather than rewriting it, since a shell sitting there
is a choice, not an accident. A `--host` spawn keeps the empty default so it never ships a local path the remote daemon
lacks; that holds for the GUI's `--host` launch and for the new-session chord of a window already attached over SSH,
both of which pick the launch form from the carrier they are on.

This is a front-door choice, not a protocol one: at the wire an empty `cwd` still means "the daemon's cwd" (see
[ipc.md](../../reference/ipc.md)); the front doors simply resolve a concrete value rather than leaning on that fallback.

Because a front door fills the field on the caller's behalf, the daemon treats an unusable one as advice rather than
instruction **when no command was named**: an unreachable directory retries in the daemon's own cwd instead of failing
the spawn. A bare launch's directory is inferred, not typed, and can vanish between the launch and the spawn (a removed
build tree, an unmounted volume, a folder the OS denies); failing it would leave the user with an `AttachFailed` and no
window, for a request they never made.

A named command keeps failing on an unusable cwd: `felis sessions spawn --cwd dir -- make` asks for that directory
specifically, so running it elsewhere would answer a different question. A relative `cwd` is refused outright at the
wire rather than resolved against the daemon's frozen directory; the CLI anchors a relative `--cwd` to the caller before
sending, since only the caller's process knows what it meant.

_Revisit if_ a window-opening chord wants the focused session's live `OSC 7` cwd rather than the launching process's,
the way the transient sessions below already do.

### Transient sessions start in the origin session's directory

The transient sessions `pipe` and `run` create take their directory from a third source: the origin session's live
`OSC 7` report, when it names a directory that exists on the _client's_ host ([input.md](../input.md) "Action mapping").
They are chorded from inside a session rather than launched, so the launching process's cwd (the front doors' source) is
the daemon's own and says nothing about where the user is. This is the "spawn in the focused session's directory" case
flagged above; it holds for transient sessions only, where the tool is by definition acting on what the session is
showing.

The client's host is what the report is judged against because the transient runs there, on the client's local daemon: a
report naming a remote directory leaves the field empty, and the transient starts in the local daemon's cwd with the
report passed through as `FELIS_CWD`. A window launch still uses the client's cwd: a new window is not necessarily a
continuation of an existing session.

### The environment a child starts from

A spawned shell starts from an inherited environment with a small denylist applied plus a few forced overrides. This
matches what every modern terminal does (kitty, alacritty, wezterm, ghostty, foot, st all use the same "inherit +
denylist + overrides" shape). An allowlist was rejected because the working set users actually need is open-ended
(`PATH`, `LANG`, `LC_*`, `SSH_AUTH_SOCK`, the full `XDG_*` family, language-runtime vars like `PYTHONPATH` / `GOPATH` /
`RUSTUP_HOME`, editor preference vars), and curating that list is a never-ending maintenance task that produces a
strictly worse default than inheritance.

#### Whose environment is inherited

Whose environment is inherited is the harder question. The honest source is the environment of the process that asks for
the session, so that process captures its own and sends it as `SpawnArgs.env_base`, but **only when its carrier to the
target daemon is the local socket**. Snapshotting the daemon's own is the obvious answer and the wrong one: the daemon
is persistent, so on a warm one that snapshot froze hours ago, and on a remote one it descends from an _earlier_ SSH
connection and names an `SSH_AUTH_SOCK` that died with it.

Over the SSH relay the dialer's environment describes the wrong host, and sending it would hand one host's credentials
to another; the relay supplies that case instead, prepending its own environment (sshd's remote-command environment,
which _is_ the remote login's) in the frozen preface layer's carrier block. The daemon resolves one chain, in order: the
request's snapshot, else the connection's relay block, else its own birth environment. A supplied base **replaces**
rather than overlays: leaving the daemon's entries underneath would keep exactly what the base was sent to displace.

Two things the daemon reads _out of_ that resolved base rather than out of its own environment, for the same reason: the
`$SHELL` a bare create runs, and the `FELIS_TERM` / `FELIS_TERM_PROGRAM` identity hatch. Reading either from the daemon
would run a warm daemon's stale login shell (or, over the relay chain, a shell path from the wrong host) in a session
whose every other variable is current. Each falls back to the daemon's own answer when the base is silent.

#### Names are canonicalized before they are checked

Windows compares environment names case-insensitively, so the checks run on the name folded under the _target
platform's_ semantics, before the reserved and denylist lookups, before dedup, and before overrides are applied. Folding
afterwards would let `felis_session_id` through a create and replace the identity stamp in-session addressing depends
on. Colliding explicit `env` pairs refuse the spawn; colliding `env_base` entries dedup with the later entry winning,
because an arbitrary winner would make a child's `PATH` depend on iteration order.

#### A stable agent path, because a snapshot cannot keep an agent alive

On Unix the daemon owns one symlink, points it at the newest live relay connection's forwarded socket, and hands
relay-chain children that path instead of the socket itself. A forwarded `SSH_AUTH_SOCK` dies with its SSH connection
and a child's environment is fixed at exec, so without more, every remote shell strands permanently the first time its
window detaches.

The link is derived from the daemon's own endpoint, sits beside its socket, and is absolute because children resolve the
path against their own working directory. A relay's disconnect restores the next-newest live target rather than clearing
the link, so a short-lived headless relay cannot take a window's agent down on its way out and a stale relay's teardown
cannot overwrite a newer registration. Windows is a no-op: the OpenSSH agent there is a named pipe at a fixed name. What
the same-UID reach over that link costs is [security-model.md](../security-model.md#process-and-environment-boundary).

Three alternatives are rejected:

- **A relay-maintained symlink.** Several writers race on connect and cleanup; the daemon is the only single writer
  available.
- **A daemon-owned agent _proxy_.** It is real socket-forwarding machinery for the same result.
- **Disclaiming persistent agent support.** The stale agent is the most common form of the very failure this exists to
  fix.

_Revisit if_ a platform appears where the agent endpoint is neither a filesystem path nor already stable.

#### `LANG` is filled rather than stamped, and only on macOS

felis fills `LANG` in the daemon when a macOS session would otherwise start with no locale. launchd hands a
`.app`-launched process no `LANG` and no `LC_*`, so a session created from the bundle runs under `LC_CTYPE=C` and every
child that encodes text for the OS falls back to the region's legacy charset. The failure hides from a terminal-only
check: `pbcopy` tags UTF-8 bytes as the region encoding and `pbpaste` reverses the same mistake, so only another
application sees the mojibake.

kitty, ghostty, wezterm and alacritty all fill the variable in their own process; felis fills it in the daemon instead,
because a base **replaces** the daemon's environment, so a client-side fill would reach only the creates that carry a
client snapshot and miss both the relay block and the daemon's birth environment. The daemon is the process on the
child's host for all three, so its host locale is the right answer in all three.

The guard is `LANG`, `LC_ALL` and `LC_CTYPE` together, read off whichever source the create resolved: an environment
naming any of the three has stated its encoding, and checking `LANG` alone would push one onto a user who deliberately
set only `LC_CTYPE`. Only `LANG` is set, the weakest of the three, so a child's own `LC_*` still outranks it;
alacritty's choice of `LC_ALL` overrides every category and is heavier than the problem.

The value comes from CoreFoundation's language and country codes rather than its locale identifier, which newer macOS
decorates with `@currency=…` and other collator metadata, and a candidate is accepted only when `/usr/share/locale`
holds it. That is the directory `setlocale` itself consults, checked as a directory because calling `setlocale` would be
FFI in a crate that denies `unsafe`.

_Revisit if_ a launcher on another platform is reported starting sessions with no locale at all, or a relay-chain child
needs the far host's locale rather than the locale of the host it runs on.

## Attach

Attach is additive ("Same-user mirroring" below owns that rule), and a plain attach is refused only for an unknown or
mid-teardown session id; the live-only form also refuses a session whose child has exited ("Picking the session a chord
lands on" below). What follows is a _rehydration burst_, bracketed by two markers and carrying the registry entries the
visible rows reference, those rows, the session meta, and the whole image store with its placements; scrollback is not
in it and arrives only through viewport requests afterwards. Its contents and their order are in
[reference/ipc.md](../../reference/ipc.md). Three properties of it are decisions rather than mechanics.

**It ships state, not history.** The daemon does not replay the PTY byte stream; it ships the grid it already holds, so
there is no room for the client and the daemon to disagree about what is on screen. This is the axis the daemon/client
split turns on ([overview.md](overview.md) "Prior art and alternatives considered").

**It is visible-first.** What a screenful of cells names is what the attach pays for before it can paint, and the image
bytes, the largest payload, close the burst rather than open it. What that ordering costs the registries, and the
rejected alternatives to it, are in [grid-and-cells.md](../data-model/grid-and-cells.md) "Referenced-before-use is
causal, not positional".

**Its middle is composed for the connection's mode, before anything is composed.** Filtering afterwards has already paid
for the row encodes and a read-back of an image store that runs to 256 MiB. A scripted `Ops` attach therefore gets the
two markers and the keyboard modes only: it reads the session through the `Region` or `Search` reply it asks for, and a
renderer-grade snapshot is content it would decode and drop. The markers themselves are mode-independent, because they
are the ordering barrier a client drains to before writing its first request.

The burst is per subscriber. A late joiner rehydrates from the current grid while the existing subscribers carry on with
incremental diffs, each at its own pull cadence ([pipeline.md](../rendering/pipeline.md) "Frame pacing").

### Client-derived presentation state

Right after attaching, the client sends `ConfigureTheme { fg, bg, cursor }` carrying its _configured_ default colors,
and reports the OS light/dark preference as `InputMsg::ColorScheme`. The daemon consults them when a program asks
(`OSC 10/11/12 ; ?` falls back to the trio, `DSR ? 996 n` and `DECSET 2031` answer from the preference), so a background
detector (neovim, modern CLIs) reads felis's real surface color instead of the xterm baseline (white). Without them a
dark felis is mistaken for a light terminal and auto-detected colorschemes invert. The reports ride every attach (and
every session switch), not the session's creation, because the same idle session can be re-attached by a client with a
different colorscheme.

Two layers meet here, and only one of them is a window's to move. Colors a running program _sets_ (`OSC 4/10/11/12`) are
session state and always win: they were installed on this PTY, and a query answers them whatever windows happen to be
attached. The client-derived layer underneath is exactly the `ConfigureTheme` trio and the OS preference: palette
entries have no client-derived fallback, so an unset `OSC 4` slot answers the compiled-in palette rather than a window's
idea of one.

#### Mirrors resolve along one candidate chain

Mirrors make that second layer ambiguous, and felis resolves the ambiguity with an order rather than a race. The daemon
stores each window's reports **per subscriber** and resolves them along one candidate chain: the last window input owner
first (the same marker "Attachment identity and the input-owner marker" below defines and a default-scope retarget
picks), then the remaining windows in reverse attach order. Scripted `Ops` subscribers are never candidates: a
`sessions send` drives the shell but owns no surface whose colors could be meant.

The two facets travel that chain **independently**: each answers from the first candidate that has reported it, and a
window that reported only its theme leaves the OS preference to the next one. A single `ConfigureTheme` is atomic,
though (one report, three channels), so a channel the client left unset means "no color", not "ask the next window", and
a query can never see a trio stitched from two mirrors.

The chain is re-resolved after **every** report, attach, detach, and ownership transfer, and which subscriber supplies a
facet is an output of that resolution rather than a gate on it: a window that never reported before takes the facet the
moment it becomes the owner and reports. What a program can observe changes only when the newly resolved, unmasked value
differs: a same-value re-report (which every client sends on attach) changes nothing, a losing mirror's report updates
only its own stored entry, and a channel a program's own `OSC 11` currently overrides shows nothing either way. Under
`DECSET 2031` the daemon notifies the program whenever a resolution moves the effective OS scheme, whoever supplied the
new value: an ownership transfer that hands the facet to a window on the other OS theme is as much a change as a fresh
report.

When the last window detaches the chain is empty, which is the whole of the lifecycle rule: the client-derived layer
resolves back to the compiled-in fallback, and a parked session never answers with a departed window's colors. Nothing
clears anything as a separate step.

#### Ownership transfer has a frozen order

The order is frozen because the event that transfers ownership is also an event the program can react to. On an input
event that moves the marker, the daemon applies the marker and the presentation state the chain now resolves to, _then_
size ownership and the PTY resize, _then_ the input bytes. Both later steps can make the child ask about colors before
the event finishes (the resize's `SIGWINCH` immediately, the keystroke as soon as the program reads it), so a query
triggered by the very keystroke that switched windows already answers with the new owner's colors.

#### Resolution outlives the shell; the notification does not

Attaching to a session whose child has exited is allowed ("Post-exit reaping" below), and that window's reports still
resolve: a query is answered from the grid, which is still there. What stops at end of file is the `DECSET 2031` write:
there is no program left to notify, and the PTY it would go to has no reader, so the write is at best discarded and on
the BSDs fails outright, which the daemon would then have to read as a dead session, ending the very corpse the user
attached to look at. The `DECSET 1004` focus edge stops there for the same reason, and the same attach triggers it: the
client reports focus on the window's first `Focused` event, right behind the `ColorScheme` report.

The rule is scoped to what the daemon writes on its own initiative. Input the user types is deliberately outside it: it
still reaches the PTY, and a write that fails there still ends the session: typing at something that has stopped running
has no better answer, and the corpse the rule protects is the one nobody is typing into.

#### Why not the last reporter, or a palette per window

_Last-reporter-wins_ (one session-global slot each report overwrites) is rejected. It ties the answer to which client's
attach frame lands last, so the same two windows in the same state can answer either way, and a background mirror
re-reporting on an OS theme change repaints a program the user is reading in the other window. Same user, same config
file hides the difference in the common case, which is what makes it worth refusing: nondeterminism nobody can reproduce
on the day it does surface.

_Per-window virtual palettes_ (giving each mirror its own view of the full color state) is rejected too: the querier is
the _program_, it has one PTY, and there is no answer to "which window's palette" to give it; that is also why palette
entries stay producer-only.

_Revisit if_ a single session ever has mirrors that genuinely differ in theme by design (a shared session across two
users' machines, say) rather than by transient OS-theme skew. The chain answers with one window's colors by
construction; a case where two answers are simultaneously right needs a different model, not a different order.

### Focus across mirrors

Focus is per-window state too, reduced differently: each window reports its own OS focus (`InputMsg::FocusChange`), and
the session is focused iff _any_ window subscriber is focused. The daemon writes `CSI I` / `CSI O` (under `DECSET 1004`)
only on that session-level edge, so a background mirror blurring cannot un-focus a session another window still shows
focused, and the focused window detaching un-focuses the program even though the window sent no parting event.

Two other reductions are rejected:

- **Last-writer.** It lets stale blur events from a background window race the foreground's state.
- **Active-window-only.** It ties an OS fact to the input-driven ownership marker, which a freshly focused window does
  not hold until the user types.

That second rejection is about focus alone: two windows can be focused at once and the OR is true of both, whereas a
color query has exactly one answer to give and therefore needs the chain to pick a window.

## Detach

A client may detach voluntarily (`SessionToDaemonMsg::Detach`) or involuntarily (the client process dies, the socket
closes, the client window is killed). All of these surface to the daemon as one subscriber deregistering; **the
underlying session is not destroyed by the disconnect itself**, and other subscribers of the same session are untouched.
Wire-level failures (transport / codec / handshake errors mid-stream) are non-destructive _by construction_ under the
subscriber model: the session lives in its owner task, so a failed connection merely unsubscribes. Only a PTY-level
failure (`ConnError::PtyIo` inside the task's own pipeline) drops the session, because that is the one signal that the
shell is gone.

When the last subscriber leaves, the owner task:

1. Stops composing grid updates, but the parser keeps running.
2. Continues to update the grid, scrollback, and image store from PTY output.
3. Continues to advance animation frames.

A live (still-running) shell is never reaped while detached: only the post-exit grace below ever ends a session on its
own.

Point 1's "parser keeps running" is literal: the parse rides the PTY **parse thread**, not the owner's async task
([pipeline.md](../rendering/pipeline.md) "Demand-driven emission" owns that topology), so it never stops because the
last client left; what the owner task stops doing is composing and shipping wire diffs. Point 2 therefore needs no
separate drain loop: the grid is already current, and the owner only replays the queued grid effects (query responses,
graphics) on the sink's dirty signal.

The parked pace lives in the sink instead: with zero subscribers it runs a token-bucket pacer (~10 MiB/s sustained after
a 1 MiB burst). Because the pacer _sleeps in the sink_, a slow window stops the reader thread, the bounded PTY read
buffer fills, and the child blocks on `write`: real backpressure on a runaway producer (`yes`) at near-zero daemon CPU,
not a drain-and-discard treadmill. It is a pace limit, not a volume limit: memory is already bounded by the fixed grid
plus the capped scrollback regardless of throughput.

## Post-exit reaping

There is no idle timer on a live shell: a running session, attached or detached, is preserved indefinitely. The only
automatic transition to **Dead** is the post-exit grace.

Each session's owner task polls its own child on the parked tick. Once the shell has _exited_ and the session has been
idle (zero subscribers) past the post-exit grace (default 5 s, measured from the last detach), the task removes itself
from the pool and the session transitions to **Dead**. A session with a running shell (or any subscriber) is left
untouched.

The grace exists so that "I closed the window by accident" is recoverable for the brief window after a shell exits; it
is restamped whenever the subscriber count drops to zero.

### Telling the attached window the shell exited

An attached window learns its shell exited through a typed signal, not an inference. When the owner task sees PTY EOF it
ships `PushMsg::SessionExited { id }` to every _window_ subscriber and _then_ drops the subscription (the wire shapes
are in [ipc.md](../../reference/ipc.md) "Push (kind = 8)"). This matters because these endings otherwise reach the
client as the same silent channel close, and the client must react differently to each:

| Event                             | On the wire                               | Session after                   | Window                                  |
| --------------------------------- | ----------------------------------------- | ------------------------------- | --------------------------------------- |
| Shell exits (ctrl-D / `exit`)     | `SessionExited` then close                | ends (grace, then Dead)         | **takes the exit ladder**               |
| Eviction (`felis sessions evict`) | `Evicted { reason }` then close           | survives (re-pooled)            | closes                                  |
| Daemon dies, or the carrier drops | bare close                                | survives only if the daemon did | **re-dials, then exit ladder or close** |
| Window switches away              | bare close on the _superseded_ connection | survives (re-pooled)            | **stays open on the new session**       |

The last two rows share a wire shape, so a bare close carries no verdict on its own. Every switch detaches its
predecessor, which means the daemon closing that stream _is_ the switch completing. The client tells the two apart by
which connection closed rather than by what arrived on it: it numbers each connection it opens, and a close on any
number but the live one says nothing about the daemon. Canceling the superseded connection's reader cannot carry this by
itself (a reader that has already reached EOF is past the point where cancellation reaches it), and back-to-back
switches retire two connections inside a millisecond, which is exactly when that race is lost.

_Revisit if_ the daemon grows a typed close reason: the connection number would then corroborate a verdict the wire
already states instead of being the only source of one.

### The exit ladder

The daemon owns the _fact_ of the exit; what the window does with it is client policy (principles 3 and 4). Given the
fact, the window looks for another shell to hold, and closes only when that bounded search is spent: a window holds
exactly one shell (principle 1), never zero. That search is the **exit ladder**:

1. A retarget or reattach the window had already parked while busy runs first: the daemon already told the issuing
   process its push was delivered, so the window honors that intent before falling back to anywhere else.
2. The window unwinds its **trail**, newest place first ("The trail: places this window has been", below).
3. Failing the trail, the window dials its current daemon and attaches the nearest live neighbor of the exited session
   in the session ring ("Picking the session a chord lands on").
4. Failing that too, the window closes, exit code `0`.

`SessionExited` still goes to the `Window` subscribers `Reattach` does; a scripted `Ops` attach has no window to move,
and the channel close it sees already ends the verb. A carrier drop that the reconnect ladder resolves to
`ReconnectError::SessionGone` ("Transport loss" below) enters this same exit ladder instead of closing: the shell is
confirmed gone, so the window looks for somewhere else to land exactly as it would on a direct `SessionExited`. A
protocol or auth refusal, or exhausted transport retries, stay terminal.

Ordering is what the ending owes the user, not a final paint. The owner task pushes `SessionExited` _behind_ the grid
events already queued and the pump flushes in order, so the shadow holds the shell's last screen by the time the client
handles the exit; the client requests a redraw and takes the ladder on the next event-loop turn. Promising a _painted_
final frame instead would promise something nobody can observe, since the window is being destroyed or repainted onto
another session in the same breath.

The session itself is unaffected by any of this: it still lingers for the post-exit grace above (a closing window is
just another detach), so a mistaken exit stays recoverable for that grace. A cross-host window dials in the same
`Window` mode a local one does ([ipc.md](ipc.md) "Cross-host attach: SSH stdio"), so it reacts to its shell exiting the
same way: it unwinds its trail, else picks on the daemon it is attached to, else closes.

### The trail: places this window has been

A window's **trail** is a stack of the places the user themself has taken it: each entry is the exact daemon and session
a landing left. A directional chord, `felis sessions switch`, the `new_session` chord from this window, a cross-daemon
retarget, and the switch into a pipe/run transient each push the place being left. Each place appears once: revisiting a
place moves it to the top rather than stacking a second copy, so bouncing between two shells never evicts older history.
Capped at 16 entries; overflow evicts the oldest, never the newest, so the trail holds the window's most recent moves. A
landing the ladder itself makes (a trail return, the ring pick, a reconnect) pushes nothing, so unwinding is monotone:
every rung consumes a trail entry it can never re-add.

Unwinding pops newest first. Each attempt dials the entry's daemon and attaches its exact session with the same
live-only attach a chord uses; a refusal (the session has exited, or is unknown, or is reaping) or a dial failure
discards the entry and the ladder tries the next one. The entry names one session, not a roster to search, so a dead
entry costs one connect and one refused attach rather than a roster round trip on top.

A `pipe`/`run` transient's switch-in pushes the place it left like any other landing, and the transient's own exit pops
that entry back, restoring the viewport the transient's launch saved, but only when the landing is that exact place. The
saved viewport stays parked while the window sits on a transient, and is cleared without being applied once the window
settles anywhere that is not the place it belongs to, since a return to an older place must not inherit a transient's
viewport. Leaving a transient pushes nothing: a transient session is never a place worth returning to, so a chain of
transient hops (a local script re-entering a remote one) still collapses into the one return [input.md](../input.md)
"Action mapping" promises, with no transient stack of its own.

A re-point the chain makes (the landing an intent asks for from inside a visit) carries the parked viewport past itself,
so the far end of the chain still has one to restore, but it does not leave the window marked as sitting on a transient:
what an intent named is an ordinary session as far as the window can tell, and a window that kept the mark would stop
recording the user's own moves out of that session for the rest of its life.

### Every ladder attempt is time-bounded

The ladder records one deadline, 30 seconds from the moment it actually starts: immediately on a direct `SessionExited`,
a bare-close transient end, or a reconnect verdict of `SessionGone`, and, when the exit arrived while a user-initiated
landing was still in flight, only once that landing settles and fails. Rung 1 and every trail entry are wrapped
connect-to-attach in whatever is left of that budget; a timeout consumes the entry like a refusal, and once the deadline
passes the remaining trail entries are skipped. The ring rung sits outside that budget: its one guarded attempt gets a
full `RECONNECT_ATTEMPT_TIMEOUT` (10 s) of its own, so the ladder's own attempts end within 30 s + 10 s of starting.

A create-form parked retarget (`felis window retarget … -- <cmd>`) bounds only its connect step even at rung 1:
cancelling after the create request went out could orphan a session the target daemon already made, so the
create-and-attach itself waits unbounded, as `retarget` does outside the ladder.

A retarget or reattach delivered while a ladder attempt is already in flight is parked rather than dropped, and runs
ahead of the ladder's next attempt (the next trail entry, the ring rung, or the close) under the same deadline. The
daemon already reported that push delivered to whichever process sent it, so a ladder that closed without running it
would contradict a result the caller already saw.

### An automatic landing never lands on a corpse

An automatic landing must never land on a session that is _itself_ lingering in the post-exit grace. A corpse's shell
cannot exit again, so no `SessionExited` would ever push the window off it, and its subscription blocks the reap, so
window and corpse keep each other alive indefinitely (two fast `felis -- cmd` runs against one daemon are enough to park
the second window on the first one's ended session).

Two mechanisms hold that line, because neither is sufficient alone. The roster carries the exit fact
(`SessionInfo::exited`, stamped from the owner task's PTY-EOF / reap-poll observations) and a chord's pick skips flagged
entries; and because the roster is only a snapshot, the landing itself goes out live-only, so a session that exits after
the listing refuses the attach rather than accepting a subscriber it can never release ("Picking the session a chord
lands on").

A _deliberate_ switch or `felis attach` sends neither the skip nor the refusal and may still land on a corpse: viewing
the final screen is what the grace is for. Pushing `SessionExited` to a late attacher instead is rejected: it would
bounce the deliberate viewer off the corpse instantly, deleting that recovery use.

### Transport loss

A bare close with nothing in flight is the transport dropping, not a verdict about the session: in the table above, a
daemon's death and a superseded switch share that wire shape. So the window does not treat it as an ending. It keeps the
shadow it last applied on screen, appends a disconnected marker to its title, stops the cursor blink (a blinking caret
would read as a shell waiting for input), and re-dials _the same carrier and the same session_.

The re-dial is six attempts backing off from one second to eight, roughly 23 s of waiting
(`RetryPolicy::WINDOW_RECONNECT`, sized so each attempt can spawn a fresh `ssh` without hammering the host), and each
attempt itself bounded to ten seconds (`RECONNECT_ATTEMPT_TIMEOUT`), so the ladder is under a minute and a half even
against a route that accepts the connection and then answers nothing. Nothing below that bound would ever return: an
`ssh` child on a half-open path can wait as long as the kernel lets it.

The re-attach goes out live-only, so a session that ended while the transport was down refuses rather than parking the
window on a corpse. The indicator and the stopped blink are client-local presentation (principle 3); no daemon message
says a window is offline.

Only the SSH carrier can really answer. Over `--host` the relay dies with the connection while the session survives on
the remote daemon, so retrying the same id reaches that daemon: the endpoint is derived from the uid alone and lives
until the remote host reboots, so nothing between two attaches moves it. On the local socket a close is the daemon's own
death, and the daemon holds the PTY masters, so the session died with it. The first attempt still auto-spawns a
replacement daemon, exactly as a window launch does, because that is what turns a dead socket into an answer: the fresh
daemon has never heard of the session, the attach comes back `UnknownSession`, and the ladder stops there. One ladder
serves both carriers because the local case terminates itself on the first attempt.

A verdict that the session is gone hands the window to the exit ladder above, since the shell ending is the same fact
whether the window heard it from `SessionExited` or from a re-dial. The other two endings, the daemon refusing the
window (auth, or a protocol major it does not serve) or the budget running out, close it, with the reason in the log and
the process status ([cli.md](../../reference/cli.md) "Window launches"). A window parked on no session is what principle
1 forbids, and felis draws no dialog to explain itself in, so the log line names the remedy instead:
`felis sessions list` for what is left, `felis attach <id>` once the daemon is back.

## Revisit: ephemeral sessions and idle-daemon exit

Two behaviors are deliberately _not_ built yet, recorded here so the cross-host design ([ipc.md](ipc.md) "Cross-host
attach: SSH stdio") can point at them instead of growing a second transport mode:

- **Ephemeral session.** A session that opts out of the "a detached live shell is never reaped" guarantee above:
  destroyed on the **last detach**, not after the post-exit grace. This is a per-session property (a `SpawnArgs` flag),
  orthogonal to transport: it means the same thing for a local window and a cross-host attach.
- **Idle-daemon exit.** Only _sessions_ are reaped on their own; the daemon _process_ exits when it is asked to and not
  otherwise, so an empty pool keeps it alive. `felis daemon stop --when-empty` already supplies the exit half of this:
  the daemon drains and then exits. What an idle-daemon exit adds is the _trigger_, an empty pool entering the drain by
  itself, on a timer nobody asked for.

They compose: an ephemeral session plus idle-daemon exit reproduces a _leave-no-trace_ attach (SSH in, work, disconnect,
and nothing lingers on the host) as two general primitives, each useful locally, rather than a bespoke
throwaway-over-SSH mode. Deferred under Principle 1: build them when a real workflow needs them, not on the chance a
transit host wants stateless attach.

## Destruction

Destroying a session hangs its child up explicitly: `SIGHUP` to the process group, then `SIGKILL` for whatever is still
there two seconds later, and a reap. The signal is explicit rather than a consequence of closing the PTY master, because
that close is never the last one while the child runs: the reader thread holds its own dup of the master and unblocks
only at EOF, which is the child exiting. Left to the kernel, the two wait on each other and a destroyed session's shell
runs forever: off every listing, unreachable, still holding a PTY.

The daemon reaps as part of the same step: it stays the child's parent for the process's whole life, so an unreaped exit
sits in the process table until the daemon itself dies. `SIGKILL` follows the grace because past it the session is
already gone from the pool, so anything still running is a process the user has no way to see or reach.

## Session switching from a single client

There is no "active session" in the daemon: that idea lives in whichever client is attached, so switching is a client
operation the daemon sees only as a detach and an attach. A live client may detach from its current session and attach
to a different existing session (or to a freshly-created one) **without exiting**. The IPC protocol expresses this
directly: a client opens a new connection and sends `SessionToDaemonMsg::Attach { id }` for an existing session, or
`SessionToDaemonMsg::Create { args }` for a new one (which attaches in the same step; see "Creation"), receives a fresh
rehydration burst, and only then sends `SessionToDaemonMsg::Detach` on the connection it is leaving. See
[ipc.md](../../reference/ipc.md) "Message families" for the message shapes.

**The switch is attach-then-detach, a bounded transactional overlap.** For the interval between the new attach and the
old detach the window holds two live subscriber connections. This is deliberate: detaching first would leave the window
with no session at all if the dial or attach fails (target reaped in the race, daemon unreachable), forcing a blank-grid
recovery state the client would have to own. Attach is additive ("Same-user mirroring"), so the overlap needs no daemon
cooperation, and the client retires the existing connection's generation before the swap so a late frame from it cannot
reach the shadow. On a failed dial the window simply stays where it was. Detach-before-dial with an explicit
failure/recovery state is the rejected alternative: it trades a harmless bounded overlap for a client-owned error state
the daemon cannot see.

This is permitted because at no instant does the window _display_ more than one PTY, which is principle 1's test in
[principles.md](../principles.md). Sequential switching never displays two at once: the transactional overlap above is a
wire fact, not a display fact, and the shadow swaps atomically at install. The switching _surface_ is what principle 1
and [non-goals.md](../non-goals.md) "Multiplexer features (delegated to the WM)" rule out, so nothing inside the
window's render surface selects a session: no list, no tab bar, no selector widget above the grid.

Switching is therefore driven from outside it, either by a separate CLI invocation ([cli.md](../../reference/cli.md)) or
by any process authorized on the daemon socket (same UID; [security-model.md](../security-model.md) "Daemon IPC").
Neither path is privileged: both are the same IPC a fresh client speaks.

The client's keymap carries only the _directional_ form, stepping the session ring ([input.md](../input.md) "Action
mapping"); naming a session by id is the CLI's, and it makes the daemon push one of the from-session's windows (the last
input owner by default, or the one `--attachment` names) through the same detach, attach, rehydrate path a chord would.
Why the direction/id split falls where it does is argued in [control-surfaces.md](control-surfaces.md) "The keymap
switches by direction, the CLI by id".

### Picking the session a chord lands on

Every pick the _client_ makes, a directional chord and the exit ladder's ring rung alike, fetches a fresh roster
(`OpsToDaemonMsg::List`) immediately before it chooses. The window caches none. A cached roster is stale by however long
the user has been working, and each way it goes wrong is invisible until the chord fires: a landing on a session killed
an hour ago, or a session created since that the ring never reaches.

The cost is that a chord is not instantaneous. It is a round trip on the connection the window already holds (the daemon
admits `Ops` post-attach precisely so a window can re-list without dialing a second one), which is local IPC in the
common case, and over SSH the carrier's latency that every other interactive operation on that window already pays.

The ring rung is the one pick that cannot use that connection: the shell exiting is what tears it down, so the fetch and
the attach both ride the fresh connection the rung dials, and a roster query that fails there ends the rung rather than
reading as an empty daemon. A trail entry dials a fresh connection too, for the same reason, but names its session
exactly and so needs no roster fetch at all: it costs one connect and one attach, live-only, nothing else.

The fetch is bounded by a deadline as well as by its connection. The failure that needs it is a daemon healthy enough to
keep streaming the grid that never answers this one verb: with only the connection bounding the wait, a chord would
leave the window unable to switch for the rest of that connection. Past the deadline the chord stays where it is and
surfaces the error, the same outcome a refused fetch already has.

Four properties make the fresh fetch well-defined.

#### Ring order is creation order

Each session carries a daemon-assigned creation sequence (`SessionInfo::sequence`): monotonic over the daemon's
lifetime, stamped once at creation and never reassigned, so an attach does not move a session and a reap leaves a gap
rather than handing its place to the next creation. The ring is that order, which makes `next` and `previous` inverses
whatever the pool did between two chords.

The two orders already on hand cannot make them inverses: session ids are random `u128`s, so a ring keyed on them
reshuffles every time a session is created, and the daemon's recency listing reorders itself every time one is parked.

`OpsToClientMsg::Listed` keeps that recency order (it is what `felis sessions list` prints) and the client sorts by
sequence itself, so neither consumer has to accept the other's order. Every row carries a sequence, since a `0` (which
is what an omitted field writes) is refused at decode, so the ring has one order and never a fallback; the id is the
second key only so the order is total by construction rather than by the daemon's promise never to repeat a sequence,
because a total order is what leaves `next` and `previous` an answer at all.

#### The anchor is a value, not a position

The window remembers the ring key of the session it is on and picks relative to that _value_: the nearest live entry
above it (`next`) or below it (`previous`), wrapping at the ends. Nothing requires the anchor to appear in the roster
that comes back: the shell can exit and the session be reaped while the fetch is still in flight, which over SSH is an
ordinary outcome rather than a corner case. A pick that needed to find its own row would have no answer there; a pick
relative to the value returns the neighbor adjacent to the reaped session.

#### Automatic landings refuse a corpse

Skipping `SessionInfo::exited` rows in the roster is necessary but cannot close the race by itself: the roster is a
snapshot, and a session that exits after it would be attached silently. That window is not merely cosmetic: the late
attacher never receives `PushMsg::SessionExited`, so nothing ever pushes it off, and its subscription blocks the
corpse's reap indefinitely.

So every system-selected landing sends the live-only form of `SessionToDaemonMsg::Attach`, which the session task
answers against its own view of the exit and refuses with `AttachFailure::SessionExited`. That covers the directional
chords, every rung of the exit ladder (each trail entry, the transient return, and the ring pick), where a plain attach
would re-open the same race whenever a trail entry's session exits between two ladder attempts, and the reconnect
ladder's re-attach after transport loss.

A landing the _user_ named sends the plain attach and may land on a corpse: reading a just-exited session's final screen
is what the grace is for. That covers a retarget or reattach the exit ladder runs ahead of its own rungs too: the intent
is user-named, so it keeps its plain attach and may land on a corpse the way `felis sessions switch` may.

#### Newest intent wins, and a landing re-picks once

At most one roster fetch is in flight per window. A chord pressed while one is out is queued, replacing whatever was
queued before, and the older fetch's roster is dropped when it lands rather than acted on, so a burst of chords moves
the window once, in the direction the user pressed last, without any mid-flight cancellation machinery.

`SessionExited` supersedes whatever is in flight or queued: the pending chord's fetch is riding the connection the exit
closes, and the ladder picks on its own dial instead. Past the pick, the chosen session can still exit before the attach
reaches it; on `UnknownSession`, `SessionEnding`, or the live-only refusal the window fetches once more and re-picks. A
second failure is terminal, which is what stops a session that keeps vanishing from looping the window: a chord then
stays where it is and surfaces the error, while a rung of the exit ladder, having no session left to stay on, continues
to the next rung. Each rung is spent once for the same reason, so a ring pick whose own landing fails reaches the close
instead of picking again. Chords never close the window.

#### No pushed roster stream

A revisioned catalog stream, the daemon pushing roster deltas to each window, is rejected as machinery without a
consumer: nothing inside the window displays a session list (there is no in-window picker, principle 1), so its only
reader would be a pick that happens a few times a day.

_Revisit if_ a session-list UI ever needs live updates.

## Cross-carrier re-dial

Session switching moves a window between sessions on _one_ daemon. A window can also be re-pointed at a _different_
daemon without closing: it detaches from its current carrier, dials another, and attaches there. This is the `felis ssh`
/ `felis window retarget` pair (the CLI surface and its flags are in [cli.md](../../reference/cli.md)); on the wire
either is `OpsToDaemonMsg::Switch` carrying a `SwitchTarget::Carrier`, which drives a `PushMsg::RetargetHost`
([ipc.md](../../reference/ipc.md) "Ops (kind = 5)", "Push (kind = 8)"). A carrier is one of three: an SSH destination
(`felis ssh`), an explicit local socket, or the default local socket (`felis window retarget`). The window holds exactly
one at a time.

**Re-dial is sequential, never simultaneous.** The window replaces its single connection; it never fans one window out
across several daemons at once. Simultaneous multi-daemon attach from one window is rejected on four counts:

- **Partial compatibility.** The protocol major, the effective minor, the connection mode, and any feature flag a peer
  opted into are all settled per connection, in the preface and the handshake that follows it
  ([ipc.md](../../reference/ipc.md) "Versioning"), so N live connections carry N separate states into one window, and an
  optimization present on one link and absent on another has no coherent window-wide answer.
- **Partial failure.** One dead link of N leaks a reconnect/retry policy into the GUI: the client would own failure
  state the daemon owns for a single connection.
- **Cross-daemon roster.** Session ids are per-daemon namespaces (a daemon serves one UID, REQ-005), so a unified roster
  across daemons needs client-side authoritative session state, which principle 3 forbids (the daemon owns session
  state, the client owns pixels).
- **In-window selector.** Choosing among hosts inside the window is a multiplexer surface, the shape principle 1
  rejects, whose test is "one window shows exactly one PTY at a time".

Sequential re-dial has none of these: the window _shows_ one PTY and _settles on_ one daemon, so it passes principle 1's
test on the same argument the sequential session-switch does ("Session switching from a single client" above). It reuses
that machinery (the same dial → attach → detach transaction the switch path runs, with the carrier descriptor swapped
for the session id), including the bounded two-connection overlap: during a retarget the existing carrier stays
connected until the new one has attached, so an unreachable target leaves the window on its current daemon instead of
blank. The rejected shape is _steady-state_ fan-out across daemons, not the transactional interval.

_Revisit if_ a design answers all of partial compatibility, partial failure, and the cross-daemon roster at once without
moving authoritative session state onto the client (principle 3); only then is simultaneous multi-daemon attach
reconsiderable.

### Crossing carriers to unwind the trail

A trail entry from a different daemon is a `Reconnector`, so a landing on it adopts that carrier exactly as a
`window retarget` does: the window detaches its current connection, dials the entry's daemon, and attaches there. This
is distinct from an ordinary retarget in one way: unwinding the trail never pushes a new entry, so a window that hopped
desktop → devbox → prod and takes ctrl-D twice returns to devbox, then desktop, without prod re-entering the trail on
either landing ("The trail: places this window has been" above).

This is the ssh feel: `exit` (ctrl-D) on a remote shell drops the window back where it came from rather than onto
another shell on the remote host. A carrier-less `felis window retarget` is the explicit counterpart. It detaches and
returns to the local carrier while the remote session **survives** on its daemon, where ctrl-D ends the remote shell;
reach for the carrier-less retarget to park remote work, ctrl-D to finish it.

## Same-user mirroring

Attach is **additive**, not exclusive: the subscriber model described throughout this document. A session is owned by an
always-running owner task (`serve::session_task`, which fans grid state out to whatever subscribers exist: zero, one, or
many), and `SessionToDaemonMsg::Attach` _subscribes_ a client, registering it for a per-subscriber rehydration burst
plus per-subscriber diff fan-out at that client's own pull cadence (see [pipeline.md](../rendering/pipeline.md) "Frame
pacing"), rather than removing the session from the pool to own it. Input from every subscriber fans into the single PTY
writer, interleaving at PTY granularity: two people typing at one keyboard. (Interleaved input from concurrent typists
can garble a command line; that is inherent to shared-shell semantics, true of tmux and screen too, and accepted.)

The subscriber model exists because exclusive attach (one connection owns the session by value; a second
`SessionToDaemonMsg::Attach` is refused) produces a **lockout**: a felis window left open on the desktop blocks the
laptop from continuing the same session, locally or over SSH (see [ipc.md](../../reference/ipc.md) "Stream layer"), and
the only recourse is eviction, which hands the session over rather than mirroring it.

The model is also the natural shape of machinery the design already demands elsewhere: frame pacing is per-connection
(one daemon rate cannot be correct for displays of different refresh rates), the PTY→grid parse runs with zero clients
watching (it rides the PTY parse thread; [pipeline.md](../rendering/pipeline.md) "Demand-driven emission"), and
rehydration is a per-connection snapshot of the current grid. Zero subscribers is the parked (sink-paced) case, one is
the classic attached stream, N is mirroring: one mechanism. Eviction stays an explicit hygiene op, not the
move-between-devices answer.

Rejected shapes for the same workflow:

- **Read-only mirror.** Too narrow: the motivating workflow is to _continue working_ from the second device, not to
  watch; input fan-in is nearly free given a single PTY writer. (A read-only subscriber remains expressible as one that
  never sends input.)
- **Client-side fan-out** (one client relays to others). The client is a wgpu renderer, not a session host; state lives
  on the daemon (principle 3), and a relay topology breaks cross-host attach.
- **Synchronized viewport** (the tmux/screen model, all clients at one scroll position). Contradicts principle 3 (the
  viewport is client-local policy) and is worse for the multi-device case: the laptop would yank the desktop's scroll.

Three per-client axes split differently, and each is argued where it is decided: the scroll position is per client,
because the shared grid is mechanism and the offset is client-local policy (principle 3); the PTY size belongs to the
active client ("Window-size negotiation" below); and the presentation state a query answers with is resolved along a
chain rather than held per client ("Client-derived presentation state" above).

### Attachment identity and the input-owner marker

The size marker answers "whose geometry does the PTY use". A separate question, "which window does a relay operation
move", cannot borrow it, and the split is why there are two markers rather than one.

Every window attach gets an **attachment id**: allocated by the daemon, unique for its whole lifetime, never reused,
published on the roster as `SessionInfo.attachments`. It is the same number the daemon's own subscriber id carries; a
second parallel identifier is rejected because every log line, command, and roster row would then have to say which of
the two it meant. Daemon-global rather than per-session, so a caller that reads the roster and then addresses one
attachment cannot have its id mean a different window on a different session.

The id is **not** exported into the child environment. A `FELIS_ATTACHMENT_ID` variable fails structurally: a PTY
environment is fixed at spawn and shared by every mirror and every later re-attach, so an environment variable cannot
name "this window". `FELIS_SESSION_ID` works precisely because a session _is_ the thing the environment is fixed to.

The **last window input owner** marker is a sibling of the size marker, updated only by PTY-reaching input from a
_window_ subscriber. The two must not be one marker: size ownership is promoted by any subscriber's input, a scripted
`sessions send` over an `Ops` attachment included, because that script really is the one driving the shell, but an `Ops`
subscriber has no window to retarget, so letting it claim the switch target would move a window the script never
addressed. Size ownership keeps its shared semantics.

What the marker buys is a default that means something: the command the user typed traveled through that window, so it
is the window they mean. When the marker is clear (nothing has typed yet, or the owner left), a session with exactly one
attached window resolves to it, which is a deduction rather than a guess: it is the only _possible_ window target. Two
or more windows with no marker is a genuine ambiguity and answers a typed `NoInputOwner` error; there is no silent
fallback to a neighboring window.

Two things read this marker, and both for the same reason. A default-scope retarget picks the window it names. And it
heads the candidate chain that resolves client-derived presentation state ("Client-derived presentation state" above),
which is why an input event that moves the marker installs the new owner's colors before it does anything else the child
can react to. The difference is what happens when the marker is clear: a retarget refuses once there is more than one
candidate, while the presentation chain falls through to the remaining windows in reverse attach order: a query has to
be answered, and any attached window's configured colors beat the xterm baseline.

The owning window detaching **clears** the marker. A tombstone (hold the dead owner and refuse until new window input
arrives) is rejected: it makes the common laptop-closes case error on a sole surviving window for no safety gain, and
the cleared state is exactly the pre-first-input state the sole-window rule already covers.

One race is accepted rather than closed. The marker is read at resolve time in the daemon, so a mirror that sends input
between the shell launching `felis` and the request resolving takes the target with it. That is the same user typing in
two windows at once: deterministic, self-inflicted, and not worth a token protocol. What _is_ closed is the narrower
one: the daemon resolves the target and enqueues the push in one step inside the session actor, so no window can detach
between "this is the target" and "pushed to it", and no reply can name a target that was never reached.

### Window-size negotiation

One PTY has one `rows × cols`. Every surveyed multi-attach tool (tmux, zmx, zellij, GNU screen, dtach/abduco, the
WezTerm mux) reconciles to one PTY size and forwards one grid; none gives each device an independently-reflowed view,
and the two projects that wanted to (zellij Discussion #5066, WezTerm issue #2133) found it blocked by a single core
grid object. The real choice is therefore _whose_ size wins and what the others do with the difference.

**Active-client-owns** is chosen: the client whose input most recently reached the session owns the PTY size, which fits
the move-between-devices workflow: type on the laptop and the size follows you. Its cost, a reflow when you switch
devices, is acceptable because the user _is_ switching devices.

It is the model zmx ships (`neurosnap/zmx` `src/main.zig`: the "leader" is the last client to send user input, and only
the leader's resize reaches the PTY) and the input-driven form of tmux's default `window-size latest`
(<https://man.openbsd.org/tmux#window-size>). The rule is principle-6-clean: "active = last input" is determined
daemon-side from input arrival, never from a client-side presentation hint.

Two other owners are rejected:

- **Smallest-common-size** (zellij; tmux `smallest`). The large desktop is shrunk to the laptop's size the moment the
  laptop attaches, even while the desktop is the one in use; it penalizes the active device.
- **Primary-client-owns.** It needs a primary-designation surface and a hand-off on primary-leave, and is the option
  most at risk of smuggling client policy into the daemon.

Non-active mirrors **letterbox**: render the authoritative grid as-is and pad the unused margin. felis can letterbox
cleanly because it owns a structured grid; the raw-stream tools (dtach, abduco) structurally cannot.

Letterboxing is chosen over two alternatives:

- **Panning an oversized window to follow the cursor** (tmux's alternative). tmux's own `CHANGES` flags it as costly and
  jittery on slow links.
- **Live per-client reflow.** It is the case blocked everywhere (above).

Snapshot rehydration (felis ships current grid state, never a byte-stream replay) also sidesteps the prompt-redraw
corruption other snapshot daemons hit on size change: zmx rewrites `OSC 133;A` to `redraw=0` on rehydrate for exactly
this (zmx issue #111).

### Geometry bounds

A session's geometry is the multiplier on almost everything it costs. The primary screen reserves its whole retention
window at construction, `(rows + scrollback) × cols × size_of::<Cell>()` of address space, so that the ring never
reallocates ([scrollback.md](../data-model/scrollback.md)), and one viewport rides beside it: on the alternate screen
the primary's ring is the parked snapshot and the live buffer is the bare alt viewport, and back on the primary a
`?47`-preserved alt viewport is the snapshot. Only ever one, because entering the alt screen _moves_ the ring into the
snapshot slot rather than copying it. At 24 × 80 the pair is 12 MiB; at the 65535 × 65535 the wire can spell, it is past
what an address space has. So the geometry a peer may ask for is bounded, on both the create path and the live-resize
path.

The bound is derived rather than picked. Holding what one session keeps held (ring plus that one viewport) under 512
MiB, with the 16-byte cell ([grid-and-cells.md](../data-model/grid-and-cells.md) "Style interning") and the 10 000-row
default scrollback REQ-605 fixes, makes 2048 × 2048 the largest power-of-two square that fits: 440.5 MiB of the 512,
against 1137 MiB for the next square up. `felis-grid`'s `the_geometry_bounds_fit_the_held_cell_budget` is the
derivation, run against the real `size_of::<Cell>()` and against the buffers a grid actually holds through a screen
switch rather than against a number quoted here, and it asserts the doubled geometry does _not_ fit, so the maximum
reads as the largest that fits and not as a round number someone liked.

2048 columns is roughly an 8K display at a 4-pixel cell: the bound excludes bugs, not windows. The floor is 1, which is
what `Grid` already clamps to; a higher one would refuse a legitimately tiny window and force the daemon to invent a
geometry the client is not showing.

The budget is on that steady state, and deliberately not on a reflow's peak. Re-wrapping the primary at a new width is
copy-based: `reflow` materializes the combined scrollback-plus-live surface, then the rewrapped result, and
`install_ring` lays the replacement ring while the existing ring is still alive: several times the ring's own footprint,
briefly, all of it freed before the resize returns. Pricing that multiple into the _admission_ bound would drag the
maxima down to 512 × 512, which is inside the range real windows reach (a maximized 8K window at an 8-pixel cell is 960
columns), and the bound would start refusing windows instead of bugs.

It would also mis-aim: the transient is proportional to the content a session has already accumulated, not to what a
request may ask for, and admission's job is to stop a cheap request from buying an expensive reservation. A session that
reaches the reflow peak filled 10 000 × 2048 cells to get there, which no `spawn --rows` can do. What the transient does
argue for is that reflow stays copy-bounded per resize and never runs concurrently with itself on one session (the owner
task serializes it), so the peak is one session's, once.

The two paths answer differently on purpose:

- A **create** outside the bounds is **refused**, with a `CreateFailure` variant of its own. A caller asking for a 65535
  × 65535 grid has a bug, and a daemon that silently substituted 24 × 80 would leave it debugging a window that is the
  wrong size for no visible reason. The variant is distinct from `SpawnFailed` because the two demand different fixes
  (one is "your geometry is impossible", the other "your command did not exec"), and a scripted caller has to tell them
  apart without parsing prose.
- A **live resize** is **clamped**. The user dragging a window edge, or a compositor reporting a surface mid-animation,
  has not made a request that can fail: refusing it would end the connection under REQ-114 and take the shell's window
  with it. Clamping degrades the view; refusing destroys the session.

Normalization happens **once, at admission**, and the clamped tuple is the only geometry the daemon then holds: the PTY
size, the grid, the subscriber's remembered `desired_size` (so a later promotion replays the clamped size, not the raw
request), the roster row, and the `GridMsg::Size` every mirror reads. The raw request is not observable anywhere
afterward. The alternative (store the request, clamp at each use) is how a mirror ends up rendering one geometry while
the PTY runs another.

Zero survives on the pixel axes alone. Pixel `0` means "unknown" (the TIOCGWINSZ convention) on both paths and is never
raised to the pixel minimum, because fabricating a window size for a producer that reported none is worse than reporting
none. Rows and columns have no such case: a create that wants the daemon's default asks by leaving `SpawnArgs.dims`
absent, so `0` rows is refused on a create exactly as 1 000 000 rows is. Spelling the default as a value drawn from the
geometry's own domain would cost one admission rule per verb over a single wire type. The argument, and why presence is
cheap enough to take instead, is in [ipc.md](ipc.md) "Optional fields instead of in-band sentinels".

_Revisit if_ a display generation arrives whose cell counts approach 2048: the bound is a budget, and the budget is a
number this argument can re-derive against a larger one.

### Slow subscribers: eviction, not backpressure

The owner task never blocks on one subscriber's wire: a subscriber whose unwritten backlog crosses the cap is evicted
instead, on the shape and the constants [reference/ipc.md](../../reference/ipc.md) "Backpressure" specifies. Applying
socket backpressure to the PTY, the natural answer with exactly one client, is rejected: with N subscribers it would let
one stalled mirror freeze the session for everyone, including the typist.

A per-session subscriber cap was the other rejected answer to "how much can these queues hold in total". It is
unnecessary: a subscriber is a connection, and connections are capped daemon-wide (REQ-916), so the number of outboxes
is already bounded without a second gauge on the attach path. That is a bound on the count, not on the bytes; what keeps
the aggregate down is the eviction, not the product of the two numbers.

### Slow children: backpressure, not eviction

The other end of the same session takes the opposite answer, and the asymmetry is the decision worth recording. A
session has N subscribers but exactly **one** child. Blocking on a mirror punishes everyone for one peer's stall, which
is why the outbox is cut instead; blocking on the child punishes exactly the connection whose bytes the child is not
taking, which is the connection that produced them. So client→daemon input is bounded and parks, where daemon→client
output is unbounded and cut ([reference/ipc.md](../../reference/ipc.md) "Backpressure").

What parks is the connection's reading loop, never the owner task: the reservation is acquired on the connection before
the bytes enter the session's command channel, so a stuck child leaves the task free to keep composing diffs for every
mirror. That ordering is the whole design. Reserving inside the task instead (the shape a bounded PTY writer invites,
since `write_pty` is called from the task's own select loop) would reintroduce exactly the freeze the eviction rule
above exists to prevent.

A parked reading loop still watches for its peer hanging up. Watching only the reservation leaks: the wait ends when the
child drains, a wedged child never does, and until then a peer that is already gone keeps its connection permit and its
subscription, so the session still counts a window that closed. The watch cannot be exact, because EOF on a socket sits
behind whatever the peer pipelined and draining that is the backpressure itself; it reads at most one read-ahead window
past what the loop had already taken, which covers the peer that hung up with nothing left in flight and leaves the rest
to the session's own end.

Dropping the input instead is rejected on the same ground the eviction rule accepts for output: a lost frame of a
mirror's screen redraws on the next diff, while a lost keystroke is gone and the user cannot tell.

Bytes the daemon generates _itself_ (such as a `DA` answer or a focus report) are the exception the reference's two
gauges carry: a child not reading its stdin cannot observe the reply either way, and waiting to deliver it would park
the task.

That exception is narrow on purpose. A mouse report is _not_ one of these: it is a window's input, so it takes a
reservation like a keystroke (the widest form the encoders produce, since the actor chooses the encoding after
admission). Dropping one would deliver a press whose release never lands, and a TUI left in a drag that no further input
clears is exactly the "lost keystroke the user cannot tell about" this rule refuses.

Fusing the two gauges is rejected too, because a reservation is held until `write_all` returns: a single large paste
would fill one shared gauge for the length of its own delivery and silence every query answer the session owed a child
that was reading perfectly well, turning a `CSI 6 n` into a hang. It also makes the exception's premise false, since a
child merely behind a paste is still reading.

_Revisit if_ a legitimate producer needs more than the budget in flight toward a child that is genuinely slow rather
than stopped; the remedy would be a larger per-session number, not a return to an unbounded queue.

### Scope and revisit triggers

Mirroring is strictly the **same user, multiple devices** case (one UID, REQ-005); sharing across distinct users remains
a hard non-goal: it would need a permission model and an audit log, and the same-user carve-out deliberately does not
open that door.

Revisit if:

- **The active-owns-the-size policy proves wrong in daily use.** It is the most likely knob to need tuning; tmux's
  four-way `window-size` option exists precisely because no single policy satisfies everyone, so a config knob is the
  obvious escape hatch.
- **Per-client independent _sizing_ becomes wanted**: each device reflowing the grid to its own dimensions. felis _can_
  build it where dtach/abduco/mosh structurally cannot, by maintaining a per-client reflowed projection of the daemon
  grid; deferred because no surveyed tool ships it and live per-client reflow is the hard case, where letterbox is the
  predictable answer.
- **Per-subscriber daemon state grows unexpectedly** (a client count high enough that diff fan-out shows in a profile):
  revisit the fan-out representation.
- **A non-Rust subscriber appears** that cannot drive the rehydrate / pull path: revisit the subscriber contract.
- **Interleaved input from concurrent typists proves harmful** beyond the accepted shared-shell semantics: consider an
  opt-in input lock (one writer at a time), at the cost of reintroducing a coordination surface.

## When the daemon ends

A daemon restart ends every session: the stop closes the PTY masters, which hangs up the child shells, and all RAM-only
state drops with the process. The update procedure that carries the drain, and the in-place upgrade that would avoid it,
are in [overview.md](overview.md) "Daemon process lifecycle". A crash loses the same session state without ending every
session _process_: the crashing daemon's fds close, so the kernel hangs up each terminal and an ordinary shell exits,
but a child that ignores or traps `SIGHUP` survives it, reparented to init, still holding its terminal, listed by no
daemon. The controlled path closes exactly that hole with the `SIGKILL` behind the grace ("Destruction" above), and the
crash path cannot: the process that knew the pids is the one that just died.

### Session-task panics

A panic inside one session's task is a smaller crash with its own containment. The task normally removes its own pool
entry and ends its child on every exit path, but an unwind skips that tail, and since the task's `JoinHandle` is
otherwise dropped, nothing observes the panic. The result is a ghost that outlives its own window twice over: `list`
keeps advertising the session, every attach is refused (the command receiver died with the task), the attached window
closes with no record anywhere of why, and the shell itself runs on.

A per-task watchdog owns that cleanup, because trusting the task's own tail is the hole. A second task awaits the
`JoinHandle` and, on `JoinError`, logs the panic payload, removes the pool entry, and puts the child through the same
hangup → grace → `SIGKILL` → reap sequence a destroyed session gets ("Destruction" above).

Two alternatives are rejected:

- **`catch_unwind` inside the task.** It adds unwind-safety assertions to the future's whole type for nothing: the
  `JoinHandle` already carries the payload across the boundary
  (<https://docs.rs/tokio/latest/tokio/task/struct.JoinError.html>).
- **Leaving the child to the unwind**, rejected for a sharper reason. Dropping the session _can_ get a hangup out, but a
  `Drop` cannot await, so nothing follows it. A program that traps `SIGHUP` never meets the `SIGKILL` behind the grace
  and runs forever holding a PTY, and one that leaves immediately is still never reaped: the daemon stays its parent, so
  it holds a process-table slot for the daemon's life.

The watchdog therefore holds its own reference to the child from before the task started: it is the one piece of the
session that has to outlive the unwind.

_Revisit if_ the daemon grows a general supervision layer; the watchdog is per-task and ad hoc.
