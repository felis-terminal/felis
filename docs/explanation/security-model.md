---
title: Security model
sidebar:
  order: 7
---

Terminal emulators sit on a high-trust seam: they render bytes from arbitrary processes and turn user keystrokes into
bytes those same processes execute. A bug in that seam is, by default, a code-execution bug. This document fixes the
threat model felis designs against and the rules every component must follow.

The standing audit checklists that enforce parts of this model (the `O_CLOEXEC` + `O_NOFOLLOW` open-site audit and the
`felis-protocol` crate-purity audit) live in the reference twin, [security-audits.md](../reference/security-audits.md).

## Threat model

### In scope

- **Hostile output written to the PTY.** Anything a remote shell, a malicious tarball's filename, a compromised log
  line, or a CTF-style payload can emit. The terminal must not let such output execute commands, exfiltrate data, or
  escalate privileges.
- **Resource exhaustion from output.** Output that is well-formed but pathological (multi-gigabyte sequences, animation
  storms, infinite DCS strings) must degrade felis, not the rest of the user's session.
- **Local same-host attackers.** Other UIDs on the machine must not be able to attach to the daemon, read scrollback, or
  hijack the PTY.
- **Parser bugs in graphics, VT, and IPC decoders.** All three accept attacker-shaped bytes; all three must fuzz
  cleanly.

### Out of scope

- **Multi-user trust.** A single daemon serves one UID. Sharing a session across users is a non-goal (see
  [non-goals.md](non-goals.md)).
- **Sandboxing the child shell.** felis runs whatever the user runs. If the shell launches a malicious binary, that is
  the OS's problem.
- **Defending against a compromised client config.** The client config file is trusted input; a user who edits it can
  already do anything the client can do.
- **Network adversaries on the SSH path.** Cross-host attach runs over SSH stdio: the client runs a felis stdio relay on
  the remote that bridges to the remote user's persistent per-UID daemon socket. SSH is the auth and confidentiality
  layer; felis adds no second one. The relay runs as the SSH-authenticated user and connects to that user's `0600`
  socket, so the remote daemon's ordinary peer-UID check covers the cross-host client too: there is no separate stdio
  auth path. See [architecture/ipc.md](architecture/ipc.md) "Cross-host attach: SSH stdio".

## Trust boundaries

```
hostile bytes ──► VT/Kitty parser ──► grid / image store ──► IPC ──► client ──► GPU
                  ▲                                                  │
                  └──────────── PTY input ◄──────────── keyboard ◄───┘
```

Three places enforce trust:

1. **Parser → state.** The parser is the only code that touches attacker-controlled bytes. It must be total: no panics,
   bounded memory, bounded time per byte.
2. **State → PTY input.** The only path that lets output influence input is the response to query sequences (DA, DSR,
   etc.). Every such response is a potential injection vector and is treated as privileged.
3. **IPC peer check.** Anything that crosses the daemon socket boundary must be authenticated as the same UID.

## Output-to-input injection

The classic terminal-emulator vulnerability class: hostile output provokes the terminal into writing bytes back into the
PTY, where the shell executes them.

Every reply felis writes back to the PTY falls into one of two classes, and the rule differs by class.

### Identity and capability replies are constants

DA1, DA2, DA3, XTVERSION, XTGETTCAP (apart from the `co` / `li` geometry capabilities), and the Kitty graphics probe
(`APC _G a=q`) answer from what this build _is_, never from what the session has done. XTGETTCAP `TN` is the one input
from outside the build: it answers the `TERM` stamped when the session was spawned, which no output can change. This is
the class fingerprinting and spoofing care about: a reply that varied with prior output would let one program read off
what ran before it, and would hand a hostile producer a way to choose the bytes felis emits.

### Reflective state replies answer with session-scoped state

The OSC 4 / 5 / 10 / 11 / 12 color queries, DECRQM, DECRQSS, cursor-position reports, the `CSI 18 t` / `CSI 19 t`
cell-size reports, the XTGETTCAP `co` / `li` geometry capabilities, the `CSI 20 t` / `CSI 21 t` icon-name and title
reports, the Kitty keyboard flags query (`CSI ? u`, which reports the flags the session pushed), and the OSC 52 mirror
readback all report state belonging to one session. That is what makes them useful and is why REQ-202, REQ-203, and
REQ-802 mandate them: a program that changed the palette restores it by reading back what it set.

Their reply is a function of prior output by construction, so the constant rule cannot cover them. What contains them is
that felis generates the reply, a decimal parameter, an `rgb:` triple, or an OSC frame formatted from parsed state
rather than a pass-through of producer bytes, so an injected `\x1b` or `;` cannot survive the round trip.

### The inputs a reply may carry are a closed, named set

Three sources reach the reflective class, and no fourth does: what a program on this session's own PTY wrote (palette,
modes, cursor position, title and icon name, the clipboard mirror), the cell geometry the session's attached clients
set, and the two presentation facets below. The title and icon name are the one place producer text is repeated rather
than reformatted, and the control bytes that could break out of the reply frame never enter that state, because an OSC
string value carrying one is rejected whole (REQ-901).

State that reached felis from the window manager or the OS has no query that reports it, which is what keeps the
reflective class from becoming an exfiltration channel. There is no font readback and no OS-clipboard read, and the
window-position and pixel-size queries answer a constant stub rather than a window's real numbers.

### One named carve-out: client-supplied presentation facets

Two replies answer with values a client supplied rather than the PTY: the `OSC 10/11/12 ; ?` fallback colors (the
attached window's configured surface colors, consulted only when no program has set a runtime override) and the OS
light/dark preference behind `DSR ? 996 n` and `DECSET 2031`. They earn the exception because the alternative misleads
every program that asks: a dark felis without them answers xterm white, and auto-detecting colorschemes inverts.

What contains them is that both sources are windows attached to _this_ session (never another session, never the window
manager), both values are a color or a two-valued preference the user configured rather than anything a producer wrote,
and the reply shape is generated like every other. Palette entries take no client fallback at all (an unset OSC 4 / 5
entry answers the compiled-in palette), so the one query with 256 slots to fill stays purely reflective. Which window
answers is [session-lifecycle.md](architecture/session-lifecycle.md) "Client-derived presentation state".

### Every answered `CSI t` readback is classified

Four `CSI t` readbacks are reflective. The text-cell size queries `CSI 18 t` / `CSI 19 t` answer in cells from this
session's own geometry, and `CSI 20 t` / `CSI 21 t` answer the icon name and window title a program on this PTY set
through OSC 0 / 1 / 2, in an `OSC L` / `OSC l` frame felis builds (an unset name answers an empty payload, so a probe
can tell "supported" from "unsupported").

The rest are constant: `CSI 11 t` always reports "shown", and the window-position and pixel-size queries
`CSI 13 / 14 / 15 / 16 t` reply a `0;0` stub, since a pixel extent is one window's fact and the grid is shared by every
mirror of the session. The per-arm replies are [vt-compliance.md](../reference/protocols/vt-compliance.md)'s. The
setters that exist are fire-and-forget.

### Clipboard, bracketed paste, and OSC control bytes

- **Clipboard is write-mostly and gated.** An OSC 52 write always lands in the session-local mirror, but reaches the
  _OS_ clipboard only with explicit user opt-in (`clipboard.osc_52 = "system"`, default off). A `?` query never reads
  the OS clipboard; it answers that mirror of the values the session itself wrote. The OS-clipboard read is not
  implemented, and the standing position is never: whether to support clipboard _read_ from the PTY at all, and under
  what confirmation, is a question felis answers no to until an argument reopens it.
- **Bracketed paste is never turned off silently.** The mode starts off and a program turns it on with `CSI ? 2004 h`. A
  DECRST that turns it back off raises an edge-triggered warning in the daemon's diagnostic log, so a program cannot
  strip the framing a shell relies on to tell pasted bytes from typed ones and leave no trace.
- **Control bytes in OSC payloads are rejected.** A `\n`, `\r`, or any other C0 byte inside a title, a hyperlink URI, or
  any other OSC string value drops the whole value rather than being normalized away, and `\x1b` ends the sequence.
  Silent normalization is how injections get smuggled in.

### The test every reply must pass

**Test:** for any identity or capability query Q the daemon answers, the response is a function of felis's static
identity alone, and is byte-identical whatever output preceded it. For a reflective state query, the response is a
felis-built frame whose only variable inputs are the three named sources: state written by the same session's PTY, that
session's cell geometry, and the two presentation facets supplied by windows attached to that same session. No reply
carries state from another session, from the window manager, or from the OS.

_Revisit if_ a reflective query is proposed whose state can reach the session from outside those three sources: an
OS-clipboard read, a palette shared across sessions, or a path that lets the window manager write the session title the
`CSI 21 t` readback reports. Reflection is safe while the writer is the session itself or the user's own window, and a
query that breaks that is exfiltration in the same shape.

## Parser robustness

The VT/CSI/OSC/DCS/APC state machine accepts unbounded attacker input.

- **Hard caps on every unbounded thing.** CSI parameter count, OSC string length, DCS payload size, APC payload size,
  intermediate-byte count: all bounded, and exceeding a cap is never silent. A CSI/DCS with overflowed params dispatches
  flagged ignore; an OSC/APC body truncates at the cap and dispatches with an overflow signal, so the consumer can
  distinguish a capped body from a complete one.
- **No allocation per byte.** Parser scratch buffers are reused; a hostile stream that emits a 4 GiB OSC string costs
  O(cap) memory, not O(stream).
- **DCS that felis does not implement is dropped at the dispatcher, not buffered.** Sixel is not supported; its DCS
  prefix is recognized and the body discarded byte-for-byte, never collected.
- **Every decoder has a `cargo fuzz` target.** A fuzz target for the VT parser, the Kitty graphics command parser, and
  each half of the IPC decode (the frame envelope and the protobuf/JSON body + wire→domain conversion) lives in the
  repo. CI runs a short fuzz pass on every PR; a longer corpus runs nightly.

**Test:** the parser passes a 24-hour fuzzer run with zero panics, zero OOMs, and bounded peak memory.

## Kitty graphics

The graphics protocol is a large attack surface: it accepts compressed image data, file paths, and chunked uploads.

- **Image decoders run with budgets.** The per-image decoded-byte budget is checked before a buffer is sized, so a zip
  bomb is refused rather than allocated; the decode aborts and the producer is answered with Kitty `ENOTSUP`, the status
  that tells it to retry smaller. The session byte cap and the per-image frame cap (REQ-1008) are the store's, applied
  when the decoded image is admitted: the byte cap first evicts the oldest images and their placements and refuses with
  `ENOTSUP` only when the image still does not fit, and the frame cap refuses without evicting anything
  ([image-store.md](data-model/image-store.md#limits)). No breach of any of the three ends the session.
- **`t=f` (file) and `t=t` (temp file) are path-restricted**
  ([the rules](../reference/protocols/kitty-graphics.md#file-transmission-safety-tf--tt)). Each rule answers one attack:
  `O_NOFOLLOW` on the final component defeats swapping the file for a symlink between the stat and the read, refusing
  anything but a regular file keeps a device or FIFO from stalling the read, and the size cap keeps the open from
  becoming an unbounded one. No root allowlist is enforced, because the daemon runs under the user's own UID and its
  filesystem reach already equals the producer's; the deferral and its revisit trigger are in the
  [Kitty graphics design](protocols/kitty-graphics.md).
- **`t=t` deletes after read, atomically.** The read and the unlink go through the same pinned parent dirfd, so a path
  swap between them cannot redirect the delete. The "temp file" contract is enforced by felis rather than trusted from
  the producer, and an unlink failure aborts the transmission.
- **Image IDs are session-scoped.** ID 42 in session A and ID 42 in session B are different objects. Cross-session
  reference is not expressible in the protocol.
- **A chunked upload is bounded by bytes, not by a clock.** One in-flight transmission reassembles into a capped buffer,
  so a producer that streams `m=1` and never terminates costs O(cap) memory rather than O(stream). A timeout is the
  rejected alternative: it would discard a slow but legitimate upload on a busy daemon to reclaim memory the cap already
  bounds, and a never-terminated stream is observable instead, as parked bytes in `felis daemon status`.

### Shared memory (`t=s`) is name-validated, copied, and retired at session teardown

A `t=s` transmission names its segment, and that name _is_ the handle, because the protocol offers no fd channel
(producers speak over the PTY). fd passing is not expressible in the Kitty protocol and would exclude mpv-class
producers.

The containment: the name is validated with the platform's shm-name rules (embedded `/`, `.`, `..` reject; traversal out
of the shm namespace is not expressible), opened read-only with `O_NOFOLLOW`, capped at the per-image decode budget
_before_ reading, and copied into a daemon-owned buffer. Name-squatting by another UID yields an open/permission error,
not data; a same-UID attacker is outside the threat model (same posture as `t=f`).

The regular-file guard is Linux-only. On **Linux** the segment lives in the `/dev/shm` tmpfs where a same-UID peer could
plant a FIFO at the name, so the open is additionally restricted to regular files (a FIFO cannot stall the read).
**macOS/BSD** keep shm in a kernel namespace isolated from the filesystem (`shm_open` can only ever return a real shm
object, never a FIFO), and the guard would otherwise reject every macOS segment, whose `fstat` reports no file-type
bits.

The object is `shm_unlink`ed at session teardown (`Session::drop`), not per read. mpv reuses one segment for every video
frame, and a per-read unlink forces its next reopen to allocate a fresh `ftruncate`-zeroed inode the terminal then reads
as a black frame. Deferring the unlink is not a containment hole: a same-UID producer can fill `/dev/shm` directly
regardless, and the teardown sweep bounds any leak to the session's lifetime (mpv also unlinks its own segment on clean
exit).

What the deferral must not become is a _ledger_. The names are producer-chosen, so holding one per transmission would
let any program in the session grow the daemon by a string per escape sequence, with no segment to show for it, since a
name that never opened was recorded too. The queue is therefore bounded and success-gated (`ShmDeferral`), which costs a
rotating producer only the per-read unlink kitty already does.

## Daemon IPC

The daemon socket is the second-largest attack surface after the parser. Rules in addition to those in
[architecture/ipc.md](architecture/ipc.md):

### The socket and its directory

- **The carrier admits only the daemon's own user, enforced by the OS.** On Unix that is a `0600` socket in a `0700`
  directory, created with an explicit `umask` rather than an inherited one; on Windows it is a named pipe whose DACL
  names the daemon's user SID alone, so a foreign user cannot open it. The endpoints and their modes are
  [the IPC reference's](../reference/ipc.md#stream-layer). What matters here is that both are OS-enforced before any
  felis code runs, so a peer that reaches the handshake has already passed a check felis cannot get wrong.
- **No abstract sockets.** Linux abstract namespace sockets bypass filesystem permissions. felis only uses pathname
  sockets inside the daemon's own `0700` directory.
- **The directory holds two felis-written entries**, `daemon.sock` and the `daemon.sock.agent` symlink
  ([reference/ipc.md](../reference/ipc.md#local-carrier)). Both are `0600`-equivalent by the directory's own mode:
  nothing outside the uid can traverse it.

#### The socket's parent is judged, never tightened

The daemon creates the directory `0700` under an explicit `umask` when it is absent, and otherwise judges it and refuses
to start if it fails. The default endpoint's name, `/tmp/felis.<uid>`, is predictable and `/tmp` is world-writable, so
another local uid can create that directory, or a symlink there, before this uid's first start.

The judgment reads the descriptor the daemon opened and locked (`O_DIRECTORY | O_NOFOLLOW`, then `fstat`): a directory,
not a symlink, owned by the uid, access bits exactly `0700`. Anything else fails the start naming what was found and the
recovery: the entry's owner removing it (`/tmp` is sticky, so the victim cannot), or `--socket` / `FELIS_SOCKET` naming
a dedicated directory. Judging the locked descriptor rather than the pathname means what is judged is what the bind
writes into.

The same rule applies to an explicit endpoint's parent, so `--socket /tmp/x.sock` is refused; felis judges the parent
only, and the parent's ancestors are the user's contract, as `~/.ssh`'s are for ssh. A directory felis did not create is
never `chmod`ed into shape: tightening it would hide whatever set it loose.

The judge runs in the daemon's bind and nowhere else. A client neither creates nor judges a parent, and is covered by
the peer-uid check below instead.

#### Binding never steals a live daemon's socket

Before unlinking anything at its path, `serve` classifies what is there without following it: a directory or a symlink
fails the start untouched (felis removes no directory, and a symlink there is the user's), a non-socket inode is
removed, and a socket is connected to first.

A connect that succeeds to a listener of this uid means a daemon is serving, so the second `serve` refuses ("another
felis-daemon is serving \<path\>"); a listener of another uid fails the start; and only `ENOENT` or `ECONNREFUSED`
proves nobody is listening and licenses the unlink (REQ-009c). Every other connect error (`EACCES`, `EMFILE`, `EPERM`)
fails the bind and touches nothing: a blind unlink-then-bind would silently hijack every future client of the live
daemon.

The probe→unlink→bind sequence runs under an exclusive `flock` on the socket _directory_. Two concurrent startups (both
front doors autospawn) could otherwise both judge the socket stale, and the slower unlink would remove the winner's
freshly bound inode: the same theft the probe prevents, reintroduced by the race. The lock is the directory rather than
a `<socket>.lock` file because a regular file is what a tmp cleaner ages, and an aged inode is how two starters end up
holding different locks.

After the bind, one check that the parent path still names the locked directory, because a daemon serving inside a
directory another uid swapped in could be fed a foreign `SSH_AUTH_SOCK` through `<socket>.agent`, which the peer check
on the daemon socket does not cover.

Nothing holds the lock for the daemon's lifetime, and exit unlinks nothing: an old daemon dying can never remove a newer
one's socket, at the cost of one stale socket file the next starter's probe removes. Windows named pipes get the same
refusal from `first_pipe_instance` at creation, which is atomic and needs no lock.

### Peer identity

#### Peer identity is verified on every connection, by both sides (REQ-106)

Peer identity is checked in code, not inferred from the carrier's permissions. The filesystem mode and the pipe DACL
already limit who can connect, but a permission set is configuration and a check is code, so felis runs both. A mismatch
closes the connection before `Hello` is read.

On Unix the check is the peer UID (`SO_PEERCRED` on Linux, `getpeereid` on macOS and the BSDs). The daemon checks it on
accept, and the dialer checks the listener's uid after the connect and before the first byte of the preface, so a
pathname connect is safe whatever a cleaner or another uid did to the path in between. That second direction is what
keeps the relay's carrier block (its whole environment) from ever reaching a foreign listener, and it is why felis needs
no equivalent of Linux's `fs.protected_symlinks` on macOS.

On Windows it is the client's user SID, resolved from the pipe's client process rather than through
`ImpersonateNamedPipeClient`, which is unreliable until the client has written bytes and the reject-before-`Hello`
contract means it has not.

The cross-host carrier changes nothing: the relay is an ordinary local client on the remote host, running as the
SSH-authenticated user, so SSH is the network auth boundary and does not bypass the local one.

#### The Windows pid lookup carries an accepted TOCTOU

Between reading the client's pid and opening the process, the client can exit and the pid be recycled, so the SID may
describe the recycled process. Exploiting it means winning a pid-reuse race from an account the DACL already admits (an
account the DACL excludes cannot open the pipe to start the race at all), so the window is recorded here rather than
chased.

The DACL and the same-user admit have runtime tests on the `x86_64-windows` runner (REQ-106a); the mismatch path is
pinned through the `admit` seam, since a single-account runner can never present a foreign SID to `accept`.

_Revisit if_ the pid lookup ever gates something the DACL does not already gate.

#### The `RetargetHost` push relays existing authority, not new

A same-UID caller on the `0600` socket can drive `OpsToDaemonMsg::Switch` with a `SwitchTarget::Carrier`, and the daemon
relays the descriptor (SSH destination, `ssh_args`, socket) to the window verbatim (see
[architecture/ipc.md](architecture/ipc.md) "Cross-host attach: SSH stdio"). That grants nothing new: a process on the
socket can already exec arbitrary code as the daemon's user, so handing the window an ssh destination to dial adds no
authority the caller lacked.

### Bounds on what a peer can make the other side hold

#### Frame length has a hard ceiling, both ways

A frame larger than the configured cap is a protocol error and tears down the connection rather than allocating. A
sender refuses the same body before the header is written, so a bug on our side cannot narrow a length into a
plausible-looking header and blame the peer for rejecting it.

#### Each operation carries its own limit beside the framing cap (REQ-105a)

Each surface names its own number, listed in [ipc.md](../reference/ipc.md) "Semantic limits". The 64 MiB frame cap says
what the framing layer will carry; it says nothing about what a search pattern, a retarget descriptor, or an argv is
allowed to be, and letting it stand in for all of them leaves ordinary inputs and descriptors free to grow to nearly the
whole ceiling.

A single cap per message family was rejected for the same reason: `SessionToDaemonMsg` alone spans a 4 KiB path and a 1
MiB argv, and one number for both is either useless on the path or wrong on the argv.

Both peers check the same constants, so a limit is a property of the protocol rather than of whichever side happens to
be newer: the numbers the first release ships are part of major 1, and narrowing one afterwards owes the old-peer answer
[ipc.md](architecture/ipc.md#schema-evolution-major-minor-feature-flag) describes rather than a quiet edit.

_Revisit if_ a real workflow hits one: the numbers are deliberately generous, and each carries its rationale in
`felis-protocol`'s `messages/limits.rs`.

#### Connections are admitted against a daemon-wide cap (REQ-916)

The daemon serves at most 1024 connections at once against an owned permit taken before the connection's task exists, so
the ceiling holds under a burst rather than after it; a dial past it is answered `Refused { AtCapacity }` and closed. A
same-UID process can open sockets as fast as the kernel accepts them, and every accepted one costs a task, a file
descriptor, and a read and write buffer.

Answering costs a task too, so the refusal path has its own small ceiling, past which the socket is dropped unanswered.
This is the one case a caller cannot tell from a dead daemon, and the price of not letting the refusal path become the
flood.

Per-session subscriber caps were the rejected alternative: with connections bounded, subscribers are bounded by the same
number, and a second gauge would bound nothing the first does not while adding a lock to the attach path.

_Revisit if_ a real deployment wants more mirrors per session than `MAX_CONNECTIONS / MAX_SESSIONS` (1024 / 256).

#### The descriptor table can bind before the cap does

1024 connections need more descriptors than the default soft `RLIMIT_NOFILE` grants on some hosts (256 on macOS), and
the daemon cannot raise its own limit because `setrlimit` is `unsafe`, which the workspace denies. So a flood can
exhaust descriptors while permits remain, and `accept` starts failing with `EMFILE` on a connection still sitting in the
backlog: retrying immediately fails on the same peer and spins the accept loop.

The loop therefore backs off for a fixed interval after an accept I/O error, which turns exhaustion into an idle wait
instead of a busy one. A peer refused for its credentials is not backed off: that accept consumed its backlog entry, so
the loop is already making progress, and delaying it would let a foreign-UID flood meter how fast legitimate dials are
admitted.

Sizing the cap for the smallest floor any supported host might impose was the rejected alternative: it would refuse
ordinary desks on every host without that limit, to buy a typed refusal in a case the caller cannot act on differently
from a retry.

_Revisit if_ the daemon acquires a reason to read its own rlimit at startup: deriving the cap from the observed limit is
strictly better than either fixed number.

#### Silence is timed only while it is unreadable

Before `Hello` a peer has told the daemon nothing, so a socket that stays quiet cannot be told from one that is wedged,
and the deadlines ([reference/ipc.md](../reference/ipc.md) "Handshake") close it. Once a mode is named the silence has a
meaning, and a mode whose work is to wait is left alone.

One blanket connection-lifetime deadline was the rejected alternative. It would cut the bridge anchor and the
notification observer (the two shapes whose whole job is to hold still), and it would not buy the bound it looks like,
because an `Ops` peer is exempt by design and can hold its slot in silence.

What the first-operation deadline does bind, it binds to the attach itself: a `Window` that asks a pre-attach verb and
stops is still cut, because a roster read is not the subject it owes. Against a hostile same-UID peer the count cap
above is the real bound; the deadlines only stop an accident from becoming permanent.

_Revisit if_ a wedged idle connection ever has to be told from a working one: that wants a keepalive the peer answers,
not a deadline it cannot see.

#### Client-side admission: a claim is checked, not trusted

The framing rules above (the frame ceiling and the per-operation limits) bound bytes that _arrive_. A scalar that tells
the receiver how much memory to reserve is not bounded by either: a twenty-byte `GridMsg::Size` announcing 65535 ×
65535, or an image header claiming `u64::MAX` decoded bytes, is a legal small frame that orders an allocation nothing on
the wire pays for.

Those scalars (announced geometry, an image header's byte count and frame index, and the running total a client's image
mirror holds) are admitted in `felis-protocol`'s decode against protocol-visible constants, on the client as on the
daemon, before any buffer is sized. The refusal ends the connection, like any other corrupt frame
([architecture/ipc.md](architecture/ipc.md) "Bounding announced quantities").

Trusting the daemon because it is same-UID was rejected: the client also mirrors a _remote_ daemon over the SSH relay,
and principle 3's "the daemon is the authority" is about screen content, not about how much memory the client is willing
to be told to hold.

#### Pending input is bounded per session (REQ-1011a)

Each session admits 16 MiB of unwritten input, the connection reserves against it before handing the bytes over, and the
reservation is released only when the PTY writer's `write_all` returns. A child that has stopped reading its stdin
(stopped, wedged in a syscall, or simply slow) is not a reason for the daemon to grow: a same-UID `felis sessions send`
loop into one would otherwise move arbitrary backlog into daemon memory, since nothing upstream could tell "queued" from
"written".

Making the session task itself block on the PTY was the rejected alternative: that task serves every subscriber and the
parser drain, so one stuck child would freeze the grid for every mirror of that session; the cost falls on the wrong
people. Bounding at the connection puts it on the one peer that is typing.

_Revisit if_ a single session ever legitimately needs more than 16 MiB of input in flight; the number is per session, so
the daemon-wide worst case is it times `max_sessions`.

### What stays out of a client's reach

#### The PTY master fd never crosses the socket

Clients receive grid/image events; they cannot obtain the underlying fd. This keeps client compromise from becoming PTY
hijack.

#### Per-client clipboard scope

OSC 52 writes initiated by client A do not appear in client B's clipboard view. Cross-client surprise is treated as a
security defect.

The daemon routes each write to the _active_ subscriber, the one whose keystrokes drove the emitting program: its best
attribution for "initiated by". With no active subscriber a sole attached window is unambiguous and receives it; with
several mirrors and no attribution the write is dropped, since any guess would land in a clipboard its user never
touched.

Broadcasting to every mirror was rejected: with `clipboard.osc_52 = "system"` on two mirrors, one program would
overwrite both hosts' system clipboards.

_Revisit if_ mirrors need a shared clipboard deliberately; that would be a config opt-in, not a default.

## Text rendering

Two well-known classes of attack target the human reading the screen, not the parser.

### Bidi override is visible

U+202A–U+202E and U+2066–U+2069 are rendered with a visible marker by default. The Trojan Source attack (CVE-2021-42574)
class is mitigated by making the trick obvious to the eye, not by stripping the codepoints (which would break legitimate
RTL content).

Each codepoint scores zero columns and folds into the cell before it like any other zero-width scalar, so the marker
recolors that cell and its character stays readable. An override with no character before it (at column 0, or after an
empty cell) is held and folds into the next character printed, so a line that opens with U+202E, the classic Trojan
Source shape, marks its first character.

It folds in after that character's own scalar, not before it, so copied text carries the override one character late: a
cluster that led with a zero-width control would break every consumer that reads a cluster's first scalar as its base,
from the renderer's fallback glyph to the Kitty image placeholder. Any other control or escape sequence arriving before
the character discards the held override, so the marker never lands on text drawn somewhere else; SGR is among the few
exemptions ([grid-and-cells.md](data-model/grid-and-cells.md#building-a-cluster) lists them) because a syntax
highlighter colors the token right after the override.

Giving each codepoint a cell of its own was rejected: a program that lays out a row with glibc `wcwidth` counts the
codepoint as zero columns, so the rest of the row lands one column right of where it drew it, per codepoint. A
Fluent-localized CLI wraps every placeholder in U+2068 / U+2069, which makes that drift common in ordinary output.

_Revisit if_ glibc `wcwidth` gives these codepoints a column.

### Grapheme cluster length is bounded

A "cell" accepts a finite number of combining marks; excess is dropped. This bounds shaper cost and prevents adversarial
inputs from monopolizing the shaper.

### Confusable URLs

OSC 8 hyperlinks display the target URL while the activation modifier is held over the link; the visible link text is
never trusted to represent the target. See "OSC 8 hyperlinks and OSC 7 CWD" below for the preview's own safety
guarantees.

## OSC 8 hyperlinks and OSC 7 CWD

- **URI scheme allowlist.** `http`, `https`, `mailto`, `file`. Other schemes are not opened. `javascript:`, `data:`,
  `vbscript:`, and custom schemes are explicit denies; adding a scheme is a recorded design decision, not a config knob.
- **Activation is explicit.** Hyperlinks are not auto-launched on emission, on hover, or on focus. They open only on
  user action.
- **Logs never carry the URI.** An activation logs only the outcome, the scheme class (`Http` / `Https` / `Mailto` /
  `File`), and the byte length of the target on success; a refusal logs only which `ActivationRejection` variant fired.
  `ActivationTarget`'s own `Debug` prints that same pair rather than the string, so a `?target` field added later cannot
  reopen the disclosure. Neither carries the string itself, so a hostile or sensitive path segment never reaches the
  log.
- **OSC 7 hostnames are display-only.** The `file://hostname/path` hostname field reported by remote shells is shown to
  the user but never drives trust decisions on the local side.

### A typed target is re-validated at the activation boundary, not trusted from the grid

The client re-parses the raw string into an `ActivationTarget` at the moment of activation, and only a successfully
parsed target ever reaches a platform launcher. The parser's own filter (dropping any OSC body with a C0 byte or DEL) is
not the only path a URI can take to the client: a non-felis daemon, or a future grid change, could hand the client a
`GridMsg::Hyperlink` the parser never saw.

The parse re-checks the same scheme allowlist and then refuses an interior NUL, any other C0/DEL/C1 control character, a
bidi-reordering codepoint (U+202A–U+202E, U+2066–U+2069, U+200E/F, U+061C), or a URI past the parser's own length cap.
Bidi codepoints are refused rather than stripped: a URI carrying one is hostile by construction (RFC 3986 URIs are ASCII
after IRI mapping), and stripping would activate a target the user never saw rendered.

### The URL reaches the OS handler as data, never as a command line

Linux and macOS spawn `xdg-open` / `open` with the target as a single `argv` element. Windows calls `ShellExecuteW`,
which takes it as one wide-string parameter.

The rejected shape is `cmd /c start "" <url>`: cmd.exe re-parses its command line _after_ the runtime's `argv` quoting,
so `&`, `|`, `<`, `>`, `^` or `%` inside a URI any remote program can print would run as a second command. No quoting
rule closes that gap; the fix is to involve no shell. The scheme allowlist above is a second, independent guard, not a
substitute: a `https://` URL is enough to carry every one of those characters.

### A control-safe, bidi-safe preview shows the real target before activation

Holding the Ctrl+Click activation modifier over a hyperlink cell shows the target in a bottom-anchored preview bar, so
the visible link text is never trusted to represent the destination. The preview is the validated `ActivationTarget`
string itself, clipped to the display width the row has: `parse` has already refused every control and bidi codepoint,
so nothing further needs stripping. A clip always ends in `…`, because a target shortened silently would read as a whole
one, which is the misreading this bar exists to prevent.

The row the bar holds takes nothing a producer controls: no Kitty image at any `z`, and no glyph or decoration reaching
down from the row above ([protocols/kitty-graphics.md](protocols/kitty-graphics.md) "Chrome rows are not drawable"): a
producer that could paint over the bar could show a URL other than the one the click activates.

### No preview, no activation

The bar shares its row with the search and confirmation bars and an IME composition, and whenever one of those holds the
row the gesture is disarmed with the bar (which bar wins the row is [input.md](input.md) "Link preview"). The test is
the frame that was painted, not the state that would paint one: a press arriving before the freed row has been repainted
activates nothing, because the anti-spoofing argument rests on a bar the user could actually read.

Rejected: **activating anyway** while the bar is hidden, which leaves the whole anti-spoofing argument off in exactly
the states a user is least attentive in (a search left open, a confirmation armed by a chord, a candidate window up),
and contradicts the affordance the arrow cursor is showing. The user's recourse is the one they already have for a bar
they opened: close it, and the preview comes back.

## Process and environment boundary

A spawned shell starts from an inherited environment with a small denylist applied plus a few forced overrides. Which
environment is inherited, how names are canonicalized before they are checked, and what replaces what are
[session-lifecycle.md](architecture/session-lifecycle.md#creation) "Creation"; the properties that hold against a
hostile caller are here.

- **The identity and addressing stamps are applied unconditionally**
  ([the variables and the order they resolve in](../reference/terminal-identity.md#the-default-identity)), so a felis
  shell identifies as felis whatever terminal the daemon was launched from. Explicit `SpawnArgs.env` pairs apply after
  them, because the stamp guards against _inherited_ leakage rather than a caller's stated request.
- **No environment content reaches a log, a diagnostic, or a refusal payload.** The refusal type carries counts and
  felis's own reserved key literals and nothing else, so the property holds by construction rather than by review; the
  caps (`MAX_ENV_BASE_ENTRIES`, `MAX_ENV_BASE_BYTES`, and the carrier block's frozen twins) are reported as numbers
  alone.
- **`O_CLOEXEC` everywhere.** Every fd the daemon opens (sockets, log files, image temp files) is opened with
  `O_CLOEXEC`. fork-exec must not leak the IPC socket into the child shell. The standing audit in
  [security-audits.md](../reference/security-audits.md#o_cloexec--o_nofollow-audit-standing) records the sites.
- **Signal forwarding is minimal.** SIGWINCH on resize. Nothing else is auto-forwarded from client to child.
- **utmp/wtmp via setuid helper, or not at all.** felis does not ship setuid binaries. If session accounting is added,
  it goes through the platform's existing helper (`utempter` on Linux). Direct utmp writes from a non-privileged daemon
  are out.

### Sanitization splits by what each side can see

The capturing side knows the platform representation, so `env_base` travels as raw bytes rather than `String`: a
non-UTF-8 `SSH_AUTH_SOCK` path is still the path the agent listens on, and a lossy conversion would cost the child its
agent while reporting nothing. The daemon stays authoritative for everything a peer could get wrong: the denylist, the
reserved keys, and platform validity (REQ-912a).

Validity is not a cosmetic rule, which is why it is checked before the environment block is built rather than while: the
Windows backend builds its `CreateProcessW` block directly from these entries, so an embedded NUL would split one entry
into two and smuggle a key past the reserved check.

### A base is scrubbed silently; an explicit pair is refused

Dropping a denylisted variable out of a captured environment is the policy working; nobody asserted it. Naming one in
`SpawnArgs.env` is a request the contract forbids, and the caller can act on the refusal.

The two reserved classes refuse the spawn for the same reason: a caller-supplied session id or daemon address would make
in-session addressing lie, sending a verb to a window or a daemon the caller chose rather than the one it is running in,
and a denylist override would undo the scrub the list exists for. The refusal names the reserved key felis owns, never
the caller's spelling of it, which is environment content.

### The denylist holds four kinds of key, for four reasons

- **A single-use focus-handoff token** (`XDG_ACTIVATION_TOKEN`, `DESKTOP_STARTUP_ID`) must not survive into a shell that
  outlives the handoff.
- **A host-terminal marker** (`VTE_VERSION`) mis-fires shell-integration scripts when the host is not that terminal.
- **A supervisor's readiness endpoint** (`NOTIFY_SOCKET`) lets whoever holds it report the daemon's own unit ready, or
  stopped, in its place.
- **The identity escape hatch** (`FELIS_TERM` / `FELIS_TERM_PROGRAM`) must configure the override without reaching the
  child as a live variable, which is why the hatch is read off the resolved base _before_ the scrub runs.

The scrub and the explicit-pair check consult one list, **derived** from the denylist plus the addressing stamps rather
than written out beside it: two lists would let a key be added to one and forgotten in the other, reaching the child
neither scrubbed nor refused with REQ-912 nominally satisfied.

### Any same-UID process that reaches the socket can repoint the stable agent link

The daemon hands relay-chain children one `SSH_AUTH_SOCK` symlink of its own
([session-lifecycle.md](architecture/session-lifecycle.md#creation) "Creation"), and a peer connecting with a carrier
block naming an agent socket points that link at it.

No privilege boundary is crossed: the peer UID is verified at accept (REQ-106), the socket and its directory are
0600/0700, and a process of that UID could already set `SSH_AUTH_SOCK` in any shell it starts, read the daemon's own
environment, or ptrace it. What such a process gains is reach over _future_ relay-chain children, which the link exists
to redirect.

The list mutation and the symlink write are one critical section, so racing connections cannot interleave into a link
naming a registration that has gone; the daemon clears a link left by a previous instance at startup, so a dead daemon's
target never outlives it.

## Resource exhaustion

Output that is well-formed but pathological must degrade felis, not the session behind it, so every unbounded quantity
is capped where it is owned: the scrollback ring at 10 000 rows (REQ-605, [scrollback.md](data-model/scrollback.md)
"Capacity and eviction"), the grapheme-cluster table in entry length and in entry count and the hyperlink table in bytes
([grid-and-cells.md](data-model/grid-and-cells.md) "Why the cluster table is capped", "Hyperlink interning", at the
values [ipc.md](../reference/ipc.md) "Semantic limits" carries), and the animation and title-update rates, which
coalesce what the display cannot show.

Both tables grow monotonically so that reattaching clients keep stable ids, which is what makes an uncapped entry
permanent for the session rather than merely large. The client's glyph caches are capped by entry as well as by pixels,
because the output picks their keys too, a blank glyph costs no pixels, and a glyph rasterized at a tiny OSC 66 size
costs almost none ([rendering/text-shaping.md](rendering/text-shaping.md#cache),
[rendering/pipeline.md](rendering/pipeline.md#atlases)).

The live image outbox queues markers naming an image rather than wire messages carrying its pixels, so a producer
transmitting faster than the daemon ships cannot decide how many copies of a decoded image accumulate
([kitty-graphics.md](protocols/kitty-graphics.md) "The live outbox carries markers, not pixels").

Where a cap would be the wrong answer, the answer is backpressure, applied to the parse and never to the renderer: when
the parse falls a full swap buffer behind, it stops draining the PTY and the child blocks on `write`, which curses
applications already handle. A client that cannot keep up does not slow the parse: its frames coalesce, and a subscriber
whose backlog crosses the cap is evicted ([rendering/pipeline.md](rendering/pipeline.md#demand-driven-emission),
[session-lifecycle.md](architecture/session-lifecycle.md#slow-subscribers-eviction-not-backpressure)).

## Logging and persistence

- **Scrollback is RAM-only.** felis writes no scrollback to disk (REQ-609): the ring lives in the daemon's address space
  and dies with the process, so a secret that scrolled past leaves no file to find, to back up, or to clean up after a
  crash. There is no persistence switch to turn on
  ([data-model/scrollback.md](data-model/scrollback.md#prior-art-and-alternatives-considered) "Prior art and
  alternatives considered").
- **No automatic password redaction.** Heuristic redaction would violate principle 4 (explicit over heuristic) and would
  leak some secrets while giving false confidence about others. Nothing in felis copies scrollback to disk on its own; a
  transcript exists only where the user's own `pipe` action put it, and its contents are theirs to judge.
- **Diagnostic logs never include payload bytes.** The daemon's `tracing` output carries event names, sizes, and counts,
  never the contents of pastes, OSC strings, or input. It reaches stderr and the per-binary log file; there is no wire
  family that streams it to a peer, so a client cannot subscribe to the daemon's log.
