---
title: Glossary
sidebar:
  order: 11
---

- **APC** — Application Program Command. ANSI escape sequence (`ESC _ ... ESC \`) carrying application-specific
  payloads, such as Kitty graphics protocol commands.

- **Atlas** — A GPU texture holding multiple glyphs or image regions. The client maintains a glyph atlas and an image
  atlas referenced by cells during rendering.

- **Attach** — A client connection to an existing daemon session, triggering a state rehydration burst.

- **Bridge** — `felis bridge`, the persistent JSON-lines stdio subcommand allowing non-Rust clients to interact with the
  daemon over standard input and output ([ipc.md](ipc.md)).

- **Cell** — The smallest addressable rendering unit on the grid. A cell stores one grapheme plus attributes; multi-cell
  glyphs span a primary cell and continuation cells ([grid-and-cells.md](../explanation/data-model/grid-and-cells.md)).

- **Connection driver** — The shared typed state machine in `felis-transport` validating connection phases, directional
  message legality, connection modes, and correlation for a connection.

- **ConnectionMode** — The role of a connection declared by the client in `ConnToDaemonMsg::Hello` (`Window`, `Ops`,
  `Observer`), shaping daemon message admission and frame routing.

- **Continuation cell** — A secondary cell covered by a multi-cell glyph or wide character, forwarding attribute and
  glyph lookups to its primary cell.

- **Correlation envelope** — Envelope field 100 in an IPC family wrapper, carrying a `request_id` or `stream_id` to
  multiplex concurrent requests and streams on a single connection ([ipc.md](ipc.md)).

- **Cross-host** — Running the daemon on a remote host and the client locally, transporting IPC frames over an SSH-stdio
  carrier ([ipc.md](ipc.md)).

- **CSI** — Control Sequence Introducer. ANSI escape prefix (`ESC [ ...`).

- **Damage** — The set of grid rows changed since the last emission (in the daemon, which ships them to clients that
  cycle) or since the last paint (in the client's shadow, whose renderer rebuilds them)
  ([damage-tracking.md](../explanation/rendering/damage-tracking.md)).

- **Daemon** — The persistent background felis process managing PTYs, screen grids, scrollback storage, image stores,
  and sessions.

- **DCS** — Device Control String. ANSI escape prefix (`ESC P ... ESC \`). felis answers DECRQSS and XTGETTCAP; any
  other DCS body is dropped at the dispatcher.

- **Detach** — Disconnecting a client from a session while leaving the daemon session and child process alive.

- **Detached session** — A session with no attached client windows, created headless via `OpsToDaemonMsg::Spawn` or
  retained after all clients detach ([session-lifecycle.md](../explanation/architecture/session-lifecycle.md)).

- **Effective minor** — The minimum protocol minor version negotiated between client and daemon
  (`min(client_minor, daemon_minor)`) in the frozen preface, determining mutually supported wire features.

- **Eviction** — Forcefully detaching all attached clients from a session via `felis sessions evict`
  (`OpsToDaemonMsg::ForceDetach`), returning the session to an unattached state.

- **Frozen preface** — The fixed-size handshake block exchanged before IPC framing begins: 8 bytes from the client and
  10 bytes from the daemon, encoding magic bytes and protocol version numbers ([ipc.md](ipc.md)).

- **Grid** — The two-dimensional cell matrix maintained by the daemon representing the active screen state.

- **IPC** — Inter-Process Communication. The frame-based protocol spoken between felis clients and the daemon over Unix
  sockets, Windows named pipes, or stdio transports ([ipc.md](ipc.md)).

- **Kitty graphics protocol** — Terminal graphics protocol transmitting and displaying inline images
  ([kitty-graphics.md](protocols/kitty-graphics.md)).

- **Kitty keyboard protocol** — Progressive enhancement protocol for unambiguous keyboard event encoding, enabled via
  `CSI > flags u` and queried via `CSI ? u` ([key-encoding.md](protocols/key-encoding.md#keyboard-encoding)).

- **Kitty text sizing protocol** — Multi-cell text scaling protocol supporting proportional font metrics
  ([kitty-text-sizing.md](protocols/kitty-text-sizing.md)).

- **Marginalia model** — The architecture where the daemon stores session tags as opaque strings and derives the
  roster's other annotations itself, delegating presentation and selection to external tools
  ([control-surfaces.md](../explanation/architecture/control-surfaces.md)).

- **Minor ledger** — The documented ledger in [ipc.md](ipc.md) tracking additions, wire compatibilities, and fallback
  behaviors across protocol minor versions.

- **OSC** — Operating System Command. ANSI escape prefix (`ESC ] ... BEL` or `ESC ] ... ST`), used for window titles
  (OSC 0/2), hyperlinks (OSC 8), and color palette queries (OSC 4/10/11).

- **Primary cell** — The origin cell of a multi-cell glyph or character run, storing font styling, rendering attributes,
  and dimension metadata.

- **Protocol major / minor** — The protocol versions declared in the frozen preface. Major bumps represent incompatible
  wire schema changes; minor bumps represent backwards-compatible additive capabilities.

- **PTY** — Pseudoterminal. The operating system kernel abstraction providing bidirectional terminal I/O for child
  processes.

- **Rehydration** — Restoring terminal state on an attaching client by transmitting the current grid and image store
  rather than replaying historical terminal byte streams
  ([session-lifecycle.md](../explanation/architecture/session-lifecycle.md)). **Visible-first rehydration** is the order
  that burst uses ([ipc.md](ipc.md)).

- **Run (shaping run)** — A span of cells shaped as one string: when `font.features` names features to apply, the
  longest span of a row that is ASCII at default sizing in one style and covered by that style's primary face; every
  other cell is shaped on its own ([text-shaping.md](../explanation/rendering/text-shaping.md)).

- **Session** — A daemon-owned instance encapsulating a child PTY, active screen grid, scrollback ring buffer, and image
  store. A session survives client disconnections.

- **SGR** — Select Graphic Rendition. The CSI sequence family (`CSI ... m`) controlling text styling, foreground colors,
  background colors, and text decorations.

- **Shadow screen** — The client-side mirror of the daemon's screen buffer, updated via IPC notifications and consumed
  directly by the rendering pipeline.

- **Sized run** — A sequence of cells formatted with custom scale factors under the Kitty text sizing protocol.

- **SSH stdio attach** — Cross-host transport launching `ssh user@host felis-daemon relay` and streaming IPC frames over
  child stdin and stdout pipes.

- **Stream (IPC)** — A multi-message correlated response sequence opened by a client request, terminated explicitly with
  `ConnToClientMsg::End` or `ConnToClientMsg::Error` ([ipc.md](ipc.md)).

- **Stream id / request id** — Independent, monotonically increasing connection-scoped identifiers assigned by clients
  to multiplex parallel operations ([ipc.md](ipc.md)).

- **Synchronized output** — Private mode sequence (`CSI ? 2026 h` / `l`) allowing applications to batch terminal
  mutations and prevent partial-frame tearing.

- **Transient session** — A temporary session spawned on the local daemon for piped external commands or runner
  keybindings, restoring the parent session upon exit ([scrollback.md](../explanation/data-model/scrollback.md)).

- **Transport (carrier)** — A bidirectional byte stream abstraction (Unix domain socket, Windows named pipe, or standard
  I/O pipes) supporting framed IPC communication.

- **VT parser** — The daemon-side state machine interpreting incoming PTY byte streams and translating escape sequences
  into grid mutations.

- **wgpu** — The portable graphics library and WebGPU implementation used by felis for hardware-accelerated terminal
  rendering across Vulkan, Metal, and DirectX 12.

- **Window manager (WM)** — The host system compositor or window manager managing window positioning, dimensions, tabs,
  and tiling.
