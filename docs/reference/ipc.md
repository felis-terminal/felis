---
title: IPC wire specification
sidebar:
  order: 5
---

The daemon and client communicate over a single bidirectional byte stream across local Unix sockets, Windows named
pipes, or SSH stdio transports.

Two companion specifications define related encoding layers:

- `crates/felis-protocol/proto/felis.proto`: Machine-readable authority for the schema layer (messages, enums, and frame
  kinds).
- [Row codec](row-codec.md): Byte encoding for the opaque `packed_cells` field.

For high-level surface mappings, see [control-surfaces.md](control-surfaces.md). For protocol design goals, tradeoffs,
and rejected alternatives, see [ipc.md](../explanation/architecture/ipc.md).

## Layering

```
Application messages   (protobuf bodies + correlation envelope)
        ↑
Frame layer            (length-prefixed, kind-tagged)
        ↑
Version preface        (frozen fixed layout, exchanged once)
        ↑
Stream layer           (binary stream: Unix socket / named pipe /
                        SSH stdio)
```

### Stream layer

A reliable, in-order, bidirectional byte stream. The frame layer operates on `AsyncRead + AsyncWrite`, and
`Connection<R, W>` is generic over its halves; that generic bound is the seam every carrier plugs into. The stream
carries no seek, no metadata, no peer credentials: peer authentication belongs to the carrier. A future carrier (TLS,
in-memory, a WebSocket tunnel for a browser-hosted client) hooks the same seam.

#### Local carrier

The `local` module in `felis-transport` is the single place the OS-specific local carrier is named; the binaries import
the carrier surface (`connect`, `Listener`, `server_split` re-exported at the crate root; `ServerStream`, `ReadHalf`,
`WriteHalf` from `local`) and never an OS stream type, so the carrier leaks from exactly one module: the extraction seam
the workspace layout wants ([overview.md](../explanation/architecture/overview.md)).

- **Unix:** `tokio::net::UnixStream`, split via `into_split`. The default endpoint is `/tmp/felis.<uid>/daemon.sock`,
  derived from the uid alone; `--socket` and `FELIS_SOCKET` are the only overrides, and both require a parent that is a
  `0700` directory the uid owns. The daemon creates that directory `0700`, binds the socket `0600` under an explicit
  `umask`, and holds an exclusive `flock` on the directory for the probe-unlink-bind window only. It leaves the socket
  file in place when it exits; the next start's probe classifies and replaces it. Beside the socket the daemon keeps
  `daemon.sock.agent`, the stable `SSH_AUTH_SOCK` symlink ("Cross-host carrier" below).
- **Windows:** a named pipe via `tokio::net::windows::named_pipe`
  (<https://docs.rs/tokio/latest/tokio/net/windows/named_pipe/index.html>). Pipes have no owned-half split, so
  `local::connect` uses `tokio::io::split`; callers receive a uniform `(ReadHalf, WriteHalf)` pair on both platforms.
  Pipes live in `\\.\pipe\`, not the filesystem (<https://learn.microsoft.com/en-us/windows/win32/ipc/pipe-names>), so
  the Unix `0700`-dir and stale-socket cleanup do not apply: a pipe vanishes when its last instance handle closes. The
  default endpoint is `\\.\pipe\felis.<sid>.daemon`, disambiguated per user by SID string (stable, unlike a volatile
  session id). Peer authentication is a per-instance DACL plus an active client-SID check: the named-pipe analogue of
  the Unix `0600` + peer-UID pair; see [security-model.md](../explanation/security-model.md) "Daemon IPC".

`Endpoint` is the platform-neutral address at the carrier boundary: a Unix path on Unix, a pipe name on Windows. The
binaries keep passing the address as the `PathBuf` they already carry and convert at the boundary; they never do
filesystem operations on the Windows value, so the "no filesystem model for pipes" rule holds without rewriting every
`&Path` call site.

For Windows carrier design rationale and rejected alternatives, see
[ipc.md](../explanation/architecture/ipc.md#carrier-choices).

#### Cross-host carrier: SSH stdio

The client runs an `ssh user@host …` sub-process and treats the child's stdin / stdout as the IPC byte stream
(`ChildStdout` / `ChildStdin` through the same `AsyncRead + AsyncWrite` seam, per ssh(1)'s stdio-forwarding semantics;
<https://man.openbsd.org/ssh.1>). On the remote the SSH command is `felis-daemon relay`, a felis **stdio relay**: it
connects to the remote user's **persistent per-UID daemon socket** and pumps frames between the SSH pipes and that
socket. A cold socket starts a daemon by intent, exactly as it does locally: a window launch's relay autospawns the
daemon (the same connect-or-spawn-the-daemon policy as a local launch, forked here since the relay runs under the remote
login's session scope), while a headless read/drive verb dials `relay --no-spawn`, which fails with a "no daemon on this
host" diagnostic on the caller's stderr instead of resurrecting one.

The relay carries bytes only; the client and the remote daemon handshake and converse end-to-end through it, so local
and remote attach share `FrameReader` / `FrameWriter`, the handshake, the rehydrate path, and the input pipeline
unchanged.

Nothing listens on the network on either end, so existing SSH firewall configurations, jump hosts, and `~/.ssh/config`
entries work unchanged, and felis inherits keys, agents, and 2FA prompts. felis puts no connection deadline on the `ssh`
child; timeouts follow the user's SSH config.

The `FRLY` **carrier block** ("Relay carrier block" below), holding the relay's own environment, is the only thing the
relay can write of its own, once, before the splicing begins; it is omitted when that environment exceeds either frozen
cap, and the relay then warns and splices the client's bare `FLIS` stream. Running as sshd's remote command, that
environment is the remote login's (including the `SSH_AUTH_SOCK` sshd has just forwarded), and it is what the remote
daemon falls back to when it creates a session for a client that could not supply one. It has to come from the relay,
because the persistent daemon's own environment descends from whatever SSH connection started it, and on a warm daemon
that names an agent socket that died with that login. The relay decodes nothing else: after the block it is a byte pump,
with no frame awareness at all.

The relay resolves its endpoint the way every local process of that uid does
([cli.md](cli.md#carrier-and-connection-lifetime) "Carrier and connection lifetime"): `/tmp/felis.<uid>/daemon.sock`,
derived from the uid alone. No environment variable takes part, so an SSH login carrying no `XDG_RUNTIME_DIR` or
`TMPDIR` reaches the daemon a desktop login started, and a login carrying either reaches the same one. Only `--socket`
or a `FELIS_SOCKET` the caller set names a different daemon.

Sessions live in the remote daemon, not in the SSH link: an SSH disconnect tears down only the relay and this client's
subscription; the per-UID daemon keeps every PTY running, and a reconnecting client reattaches to the still-live session
until the host reboots ([attach-over-ssh.md](../how-to/attach-over-ssh.md) "Operational notes"). On Unix the daemon also
keeps a stable `SSH_AUTH_SOCK` path of its own (`<socket path>.agent`, a symlink beside its socket, pointed at the
newest live relay connection's forwarded socket) and hands children that path instead of the forwarded one, so existing
shells regain a working agent on the next SSH attach rather than stranding on a dead socket forever. Windows needs no
such indirection: the OpenSSH agent uses a named pipe at a fixed name. For cross-host carrier rationale and rejected
alternatives, see [ipc.md](../explanation/architecture/ipc.md#carrier-choices).

SSH is the auth and confidentiality boundary; felis adds no second password / token layer. The relay needs no
special-cased auth: it runs as the SSH-authenticated user and connects to that user's `0600` socket, so the remote
daemon's ordinary local peer-UID check ([security-model.md](../explanation/security-model.md) "Daemon IPC") already
covers the cross-host client. There is no separate "skip the check over stdio" path to get wrong. The cross-host client
speaks the same protobuf binary a local one does, preface included: the relay moves bytes and negotiates nothing, so the
link's speed changes no part of the contract. The `ipc_throughput` bench (`crates/felis-protocol/benches/`) measures
that codec over the message shapes the daemon emits.

### Version preface

Before any frame, each side writes one fixed-layout block. The layout is frozen for all time (REQ-104a): version
selection cannot depend on the schema being selected, so it happens in bytes both a future and a past felis can read.
The Rust implementation is `crates/felis-protocol/src/preface.rs`.

Client → daemon, 8 bytes:

| Offset | Width | Field                                |
| ------ | ----- | ------------------------------------ |
| 0      | 4     | magic `FLIS` (`0x46 0x4C 0x49 0x53`) |
| 4      | u16   | protocol major the client speaks     |
| 6      | u16   | protocol minor the client speaks     |

Daemon → client, 10 bytes, always written:

| Offset | Width | Field                          |
| ------ | ----- | ------------------------------ |
| 0      | 4     | magic `FLIS`                   |
| 4      | u16   | status                         |
| 6      | u16   | first word (status-dependent)  |
| 8      | u16   | second word (status-dependent) |

All preface integers are **big-endian**. The frame header below is little-endian. For endianness design rationale, see
[ipc.md](../explanation/architecture/ipc.md#handshake-bootstrap).

The **status word alone** discriminates the reply. A decoder never infers the trailing words' meaning by comparing them
against what it sent:

| Status    | Meaning                                                                | Words                                     |
| --------- | ---------------------------------------------------------------------- | ----------------------------------------- |
| `0`       | Accept. Frames follow, protobuf binary, under the agreed major.        | agreed major, then the daemon's own minor |
| `1`       | Refuse. The daemon closes.                                             | the majors it does serve: min, then max   |
| any other | Refuse for a reason this reader is too old to name. The reader closes. | uninterpreted                             |

Decoding is lenient; negotiation is exact (REQ-104d). An accept's first word equals the major the client offered. The
client checks this before it reads any frame: on a mismatch it closes with no frame written and reports its own error
class, distinct from a refusal (`felis version` and `felis doctor` name both majors). On the daemon, the accepted major
and the minor advertised with it are one selection: a build that serves two majors during a deprecation window
advertises each major's own minor. The decoder does not compare; the check sits above it, in the connector.

A first four bytes that are not the magic end the connection with no reply: an unknown protocol gets no answer in ours.

The major names the schema. A build serves the range `SUPPORTED_MAJOR_MIN..=SUPPORTED_MAJOR_MAX`, which is one value
until a major bump opens a deprecation window in which the daemon serves both. The minor names an additive revision of
that schema, and the **effective minor**, `min(client, daemon)`, is what both peers may use. It is a send-side contract:
a peer never sends a frame kind, oneof variant, or row-codec version the effective minor does not define ("Versioning"
below).

#### Relay carrier block

One optional block may precede the client preface: the **relay carrier block**, which `felis-daemon relay` prepends to
hand the remote daemon the environment of the sshd remote command it runs as ("Cross-host carrier: SSH stdio" above). A
bare `FLIS` stream simply omits it, and the two forms are told apart on the first four bytes.

The envelope is frozen with the rest of this layer, not versioned as a minor addition: it is written before either peer
has said what it speaks, so a daemon has to read it without negotiating anything. The payload names its own format in
its first word, because the protocol major that would otherwise select a decoder arrives only after this block.

| Offset | Width | Field                                |
| ------ | ----- | ------------------------------------ |
| 0      | 4     | magic `FRLY` (`0x46 0x52 0x4C 0x59`) |
| 4      | u32   | payload length, in bytes             |
| 8      | …     | payload                              |

The payload, format version 1:

| Width | Field                        |
| ----- | ---------------------------- |
| u16   | payload format version (`1`) |
| u32   | entry count                  |
| u32   | name length, in bytes        |
| …     | name bytes                   |
| u32   | value length, in bytes       |
| …     | value bytes                  |

The last four rows repeat once per entry. Every integer is **big-endian**, like the rest of the preface. Names and
values are raw platform bytes in the _capturing host's_ representation, exactly as `SpawnArgs.env_base` carries them
("Session (kind = 4)" below); the relay and the daemon are the same host, so the two ends always agree on it.

The payload is consumed **exactly**: the client preface begins at the byte after it, so trailing bytes are a malformed
block rather than slack. A truncated block, an inner length that overruns or underruns the payload, a payload format
version of `0`, and a block over either limit below all close the connection like a wrong magic: the envelope is frozen,
so a peer that gets it wrong is not one this build can bootstrap with, and there is nothing to negotiate.

A payload format version other than `0` and the ones the reader decodes is **not** malformed. The reader has already
consumed the payload by its length word, so it warns, takes the no-block fallback below, and reads the client preface
from the next byte. `0` is the one value held apart because the version word is what tells a writer that predates it
from one that has it: such a writer puts its entry count where the version belongs, and that count's high half is zero.

`MAX_CARRIER_PAYLOAD_BYTES` is frozen for all time, with the envelope: it bounds the one allocation a length word buys,
in every format version including the ones this build cannot decode. `MAX_CARRIER_ENTRIES` is frozen with format version
1, the entry encoding it counts. Neither is per minor or per daemon: the block precedes version negotiation, so a limit
either side picked for itself would reintroduce the skew the frozen layer exists to remove. Every relay checks them
before sending and every daemon checks them on read; the daemon checks the declared payload length _before_ reading the
payload, so a bogus length word costs a close rather than an allocation.

| Limit                       | Value | Applies to              | Sized for                                                                    |
| --------------------------- | ----- | ----------------------- | ---------------------------------------------------------------------------- |
| `MAX_CARRIER_ENTRIES`       | 4096  | entries in one block    | the per-entry length words, which are read before any entry body             |
| `MAX_CARRIER_PAYLOAD_BYTES` | 1 MiB | the payload length word | the one allocation the declared length buys before any byte of it is trusted |

Both values are set against a measured login environment, which runs to tens of KiB (53 KiB in a login shell, 59 KiB
inside this repo's `nix develop` shell). At 1 MiB the payload cap clears the environments a relay actually carries by
more than an order of magnitude, and still bounds what an unverified length word can ask the daemon to allocate.

An environment over either limit is **not sent**: the relay logs a warning on its stderr, which is the SSH session's
stderr and therefore the caller's terminal, and splices the client's bytes without a block. The daemon then takes the
documented fallback and gives that connection's creates its own environment ("Session (kind = 4)" below). The connection
itself is unaffected: the caller loses agent freshness, not the session.

Every released v1 daemon reads both forms, so relay ↔ daemon skew never needs negotiation: a relay that prepends nothing
leaves the daemon on its own environment, and a relay that prepends a format version 1 block works against any released
v1 daemon. A relay that prepends a format version the daemon does not decode lands on the same fallback: the daemon
warns, skips the payload by its length, and serves the connection off its own environment.

### Frame layer

Each frame:

```
+---------+---------+----------+
| len:u32 | kind:u16| body:[u8]|
+---------+---------+----------+
```

- All header integers are little-endian.
- `len` covers the rest of the frame (`kind` + `body`), so the body it admits is `len` minus the two kind bytes. That
  body is capped in both directions at `DEFAULT_MAX_BODY` (64 MiB) (REQ-105). Inbound, a `len` describing a larger body
  is fatal: the frame layer rejects it before allocating anything and the connection is torn down. Outbound,
  `Frame::encode`, `encode_to` and `FrameWriter::write_frame` refuse a larger body before the `u32` narrowing and before
  any header byte is written, so a refused frame leaves the stream consistent and the connection usable. Image producers
  keep each `ImageMsg::Chunk` payload at or below `MAX_IMAGE_CHUNK_PAYLOAD` (256 KiB), far under that ceiling: the cap
  is sized as a scheduling quantum, not as ceiling headroom ("Backpressure" below).

#### Semantic limits

The 64 MiB frame cap is a **framing backstop**, not any operation's policy. Each surface below carries its own limit
(REQ-105a); the backstop only bounds what the framing layer will carry at all. Every limit on a value that becomes a
frame body sits far under it. The bridge line is the one entry that does not: it is a JSON envelope on a pipe rather
than a body, and the payload inside it is bounded by `MAX_PASTE_BYTES` on the way to the wire.

A limit the `Checked` column marks `sender and receiver` is checked twice with the same number: by the sender before the
body is encoded (`FrameWriter::send*`, and the CLI and bridge before they dial), and by the receiver after the body
decodes (`codec::decode`), where a breach is `WireError::OverLimit`, naming the field, the size, the cap, and the unit
the cap counts (bytes, entries, or frames). Every other marking is a departure from that and says which half is missing:
a cap the sender alone holds, one a handler reads instead of the decode, and one a table or constructor enforces each
answer in their own terms, which `Outcome` states, rather than by closing the connection.

The receive-side values are part of major `1` itself, not rows of the minor ledger. Lowering one for every peer narrows
what a field already accepts, which is a change of meaning (REQ-104b): a released peer sending a value under the old
bound would have its connection torn down.

| Limit                                     | Value                 | Applies to                                                                                                                                                                                                                 | Checked                                                                       | Outcome                                                                                                                                                                                                          |
| ----------------------------------------- | --------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `MAX_RAW_INPUT_BYTES`                     | 16 MiB                | `InputMsg::KeyBytes` payload (`sessions send --raw`'s escape stream, not only a key event)                                                                                                                                 | sender and receiver                                                           | sender refuses; receiver closes the connection                                                                                                                                                                   |
| `MAX_PASTE_BYTES`                         | 16 MiB − 64           | `InputMsg::Paste` payload (the shortfall is the room a paste's bracketing needs inside the PTY input budget)                                                                                                               | sender and receiver                                                           | sender refuses; receiver closes the connection                                                                                                                                                                   |
| `MAX_KEY_CHARACTER_BYTES`                 | 32 B                  | `InputMsg::Key`'s character (one keystroke's own text; anything longer is a paste)                                                                                                                                         | sender and receiver                                                           | sender refuses; receiver closes the connection                                                                                                                                                                   |
| `MAX_KEY_TEXT_BYTES`                      | 32 B                  | `InputMsg::Key`'s composed text                                                                                                                                                                                            | sender and receiver                                                           | sender refuses; receiver closes the connection                                                                                                                                                                   |
| `MAX_SEARCH_PATTERN_BYTES`                | 4 KiB                 | `SearchToDaemonMsg::Query.query`                                                                                                                                                                                           | sender and receiver                                                           | sender refuses; receiver closes the connection                                                                                                                                                                   |
| `MAX_SPAWN_ARGV_ENTRIES`                  | 4096                  | `SpawnArgs.args` entries                                                                                                                                                                                                   | sender and receiver                                                           | sender refuses; receiver closes the connection                                                                                                                                                                   |
| `MAX_SPAWN_ARGV_BYTES`                    | 1 MiB                 | `SpawnArgs.args` total bytes                                                                                                                                                                                               | sender and receiver                                                           | as above                                                                                                                                                                                                         |
| `MAX_SPAWN_PATH_BYTES`                    | 4 KiB                 | `SpawnArgs.command`, `SpawnArgs.cwd`                                                                                                                                                                                       | sender and receiver                                                           | as above                                                                                                                                                                                                         |
| `MAX_ENV_BASE_ENTRIES`                    | 4096                  | `SpawnArgs.env` entries                                                                                                                                                                                                    | sender and receiver                                                           | as above ("Session (kind = 4)")                                                                                                                                                                                  |
| `MAX_ENV_BASE_BYTES`                      | 1 MiB                 | `SpawnArgs.env` key + value bytes                                                                                                                                                                                          | sender and receiver                                                           | as above                                                                                                                                                                                                         |
| the same two caps on `SpawnArgs.env_base` |                       | entries and key + value bytes of a captured base                                                                                                                                                                           | dialer before it sends, daemon in the spawn path — deliberately not at decode | the one spawn fails as `CreateFailure::SpawnFailed`; the connection stays up ("Session (kind = 4)")                                                                                                              |
| `MAX_SESSION_TAGS` / `MAX_TAG_BYTES`      | 32 / 128 B            | `SpawnArgs.tags`                                                                                                                                                                                                           | sender and receiver                                                           | as above ("Ops (kind = 5)")                                                                                                                                                                                      |
| `MAX_SESSION_TAGS` / `MAX_TAG_BYTES`      | 32 / 128 B            | `OpsToDaemonMsg::Tag`'s delta                                                                                                                                                                                              | receiver                                                                      | not an `OverLimit`: the daemon answers `OpsToClientMsg::TagsUpdated` with `denied` set, so a caller that asked for too many tags reads why instead of losing the connection under a codec error                  |
| `MAX_RETARGET_DESCRIPTOR_BYTES`           | 64 KiB                | `RetargetTarget` carrier strings, on `OpsToDaemonMsg::Switch` and `PushMsg::RetargetHost`                                                                                                                                  | sender and receiver                                                           | as above                                                                                                                                                                                                         |
| `MAX_BRIDGE_LINE_BYTES`                   | 8 × `MAX_PASTE_BYTES` | one request line of the CLI stdio bridge — eight times `MAX_PASTE_BYTES`: JSON escaping expands a control byte sixfold, and the remaining two multiples are the envelope around that worst case                            | sender and receiver                                                           | the bridge answers `invalid_request` on `id: null`, since reading the id means parsing the payload being refused; the line is refused on its length as it is read, never buffered whole                          |
| `MAX_REGION_REPLY_BYTES`                  | 32 MiB                | `RegionToClientMsg::Reply.data`, the one stitched region body                                                                                                                                                              | sender only                                                                   | the daemon trims to the youngest resumable boundary — a line start, or a scalar start outside any escape sequence when the window holds no line boundary — and drops the viewport position with the head it lost |
| `MAX_GRID_ROWS` / `MAX_GRID_COLS`         | 2048 / 2048           | announced geometry: `GridMsg::Size`, `SessionInfo.dims`                                                                                                                                                                    | receiver                                                                      | `WireError::OutOfRange`; the client closes the connection                                                                                                                                                        |
| the same two                              |                       | the row index and body width of a `GridMsg::RowDelta`, which auto-grow the client's mirror to fit a row that outran a resize                                                                                               | receiver                                                                      | inside the bound the shadow grows; past it the row is a `ShadowError` and the client closes the attachment ("Grid admission" below)                                                                              |
| `MAX_GRID_PIXELS`                         | 32768                 | announced `GridDims.pixel_w` / `pixel_h`, `0` meaning unknown                                                                                                                                                              | receiver                                                                      | as above                                                                                                                                                                                                         |
| `MAX_IMAGE_BYTES`                         | 64 MiB                | the `width × height × bytes_per_pixel` an `ImageMsg::Header`'s `New` target implies, and the daemon's own decode budget                                                                                                    | receiver (both ends)                                                          | `WireError::OverLimit`; the client closes the connection                                                                                                                                                         |
| `MAX_IMAGE_FRAMES`                        | 4096                  | `ImageMsg::Header`'s `Frame` number, and the daemon's frame store per image                                                                                                                                                | receiver and daemon                                                           | over the cap the daemon answers Kitty `ENOTSUP`; a header past it closes the client's connection                                                                                                                 |
| `MAX_SESSION_IMAGE_BYTES`                 | 256 MiB               | all decoded image bytes one session retains, the daemon's store and the client's mirror alike; both charge the entry and frame records alongside the pixels, so an appended frame pays for its record as well as its bytes | receiver (both ends)                                                          | the daemon evicts; the client closes the connection                                                                                                                                                              |
| `ClusterText::CAP`                        | 128 B                 | one interned grapheme cluster's text, on the grid and on a `GridMsg::Cluster` a shadow installs                                                                                                                            | constructor precondition (both ends)                                          | daemon-side the fold stops and the cell keeps the cluster that fit; an over-cap entry off the wire closes the client's attachment                                                                                |
| `CLUSTER_TABLE_CAP`                       | 131 072               | entries in one grid's cluster table, daemon and shadow alike; also the highest id a shadow will install                                                                                                                    | the table itself (both ends)                                                  | `intern` refuses a new entry after the dedup lookup; an id past the cap closes the client's attachment                                                                                                           |
| `LinkText::CAP`                           | 8 KiB                 | one hyperlink entry's `id` and `uri`, from the parser or off a `GridMsg::Hyperlink` (equals `OSC_BUFFER_LIMIT`)                                                                                                            | constructor precondition (both ends)                                          | daemon-side the pen is not set and the covered text prints without a link target; an over-cap entry off the wire closes the client's attachment                                                                  |
| `LINK_TABLE_BYTE_CAP`                     | 8 MiB                 | the charged footprint of one grid's hyperlink table (entry struct, both string payloads, dedup slot)                                                                                                                       | the table itself (both ends)                                                  | the entry is refused before the slot vector grows, leaving no charge and no gap; a client that cannot hold it closes the attachment                                                                              |

A CLI verb or bridge operation that would breach one refuses before it dials, reporting the `invalid_request` machine
error kind ([cli.md](cli.md) "Machine output"); the connection is never involved. The GUI client refuses an over-limit
paste rather than truncating it: this applies to clipboard pastes, file drops, and `pipe` chord `paste` sinks. It also
refuses over-limit search patterns before opening a stream. Refusals surface explicitly: paste size appears on the
confirmation bar, and pattern length appears in the search bar.

The region reply is the one row checked on the sending side alone: it bounds a body the daemon builds from its own
scrollback rather than anything a peer asked for by size, and an over-budget region is trimmed rather than refused.

The hyperlink table's entry _count_ needs no constant of its own: the `NonZeroU16` handle is the bound (65 535), which
is why only its byte budget is listed.

The geometry and image rows bound a **claim** rather than a length: a number that tells the receiver how much memory to
reserve for bytes that have not arrived and need never arrive. They are therefore admitted in the `TryFrom<v1::*>`
conversion, where the number is read, rather than on both sides of the send
([the IPC design explanation](../explanation/architecture/ipc.md#bounding-announced-quantities)).

- The header carries no sequence number: ordering is the stream's own guarantee, and the one thing a peer does act on
  rides a typed body instead: `InputMsg::NextGridFrame`, the client's grid-frame pull
  ([the IPC design explanation](../explanation/architecture/ipc.md#the-frozen-frame-header)).
- `kind` selects the message family and is what the daemon routes on: `Conn` (0), `Input` (1), `Grid` (2), `Image` (3),
  `Session` (4), `Ops` (5), `Region` (6), `Notify` (7), `Push` (8), `Search` (9). These are the schema's `FrameKind`
  values, which is where a non-Rust implementation reads them.
- The header carries no correlation id. Correlation rides inside the body, in the envelope specified under "Correlation,
  requests, and streams" below; the message type is the family oneof's field number inside that body.

`kind` is a routing tag, not a channel: a connection carries one ordered frame stream per direction, and every family
shares it. What keeps a backed-up image stream from stalling input is not per-kind flow control but the two directions
being serviced independently ("Backpressure" below).

### Application messages

Every frame body is **Protocol Buffers binary**: one encoding, on every connection, in both directions. There is no
per-connection encoding statement and nothing to negotiate. A client whose language has no protobuf runtime reaches the
daemon through the CLI's stdio bridge instead ("CLI clients" below). Why protobuf, and why the second JSON wire was
rejected, is recorded in [the IPC design explanation](../explanation/architecture/ipc.md#why-protobuf) and its "One
encoding on the socket, JSON at a bridge" section.

The wire schema is `crates/felis-protocol/proto/felis.proto` (package `felis.v1`), and it is the **single
machine-readable authority** for the schema layer: the message families, their supporting structs and enums, the
`FrameKind` values, and the preface constants it documents for a generator to read. The generated Rust types are
committed under `felis-protocol/src/generated/` and regenerated by `just proto`, so a build needs no protoc; the
hand-written types in `felis-protocol/src/messages/` are internal domain types with validated constructors over the same
shapes, and `convert/` is the validator between them, checking what proto3 cannot state (a required oneof, an enum with
no `UNSPECIFIED` counterpart, an integer that must narrow). Where the schema and the Rust could disagree, the schema
wins, and tests that read `felis.proto` are what keep them from drifting apart.

The `felis.v1` package name is a schema namespace, not a copy of the protocol major: it moves only when a daemon has to
hold two schemas at once, which is what a post-freeze major's deprecation window needs.

## Correlation, requests, and streams

A connection is multiplexed: several requests and several streams may be in flight on it at once, and every frame that
belongs to one says which. The carrier of that statement is the **correlation envelope**, field **100** of every
wrapper. The field number is the same wire-wide, so a reader lifts the envelope off any body without a per-family table:

```
Correlation { oneof id { uint64 request_id = 1; uint64 stream_id = 2; } }
```

The envelope names **exactly one** id. A frame belongs to a point request or to a stream, never to both, so the schema
is a `oneof` and the domain type an exclusive `Correlation::Request(RequestId) | Correlation::Stream(StreamId)`. Three
encodings are malformed and end the connection: an unset `id`, a set arm holding `0` (`0` is how the wire spells absent,
which is why both sequences start at 1), and a body setting both tags, which the schema cannot express. The both-tags
refusal reads tag _presence_, not the values that survive: a body that writes `request_id`, then `stream_id`, then
`request_id` again as `0` names both, however a last-field-wins reader would resolve it.

Which id an arm must carry is its `correlation` class ("The arm table" below), and the driver checks the envelope
against the class before it delivers anything:

| Class            | Envelope                                                                                                        |
| ---------------- | --------------------------------------------------------------------------------------------------------------- |
| `uncorrelated`   | None. An envelope present on such an arm is malformed.                                                          |
| `request_opener` | A `request_id` equal to the receiver's next unissued request id, which then advances.                           |
| `request_reply`  | The `request_id` of an outstanding request, which it retires. A duplicate or unattributable reply is malformed. |
| `stream_opener`  | A `stream_id` equal to the receiver's next unopened stream id, which then advances.                             |
| `stream_item`    | The `stream_id` of a live stream.                                                                               |

The class half of that check runs on the sending side too. The encoder every send path goes through
(`CheckedFrame::encode` and `encode_correlated`, and so `FrameWriter::send*`, the client's outgoing queue, and the
daemon's pre-encoded fan-out) refuses a correlated arm handed no envelope, an `uncorrelated` arm handed one, and an id
of the wrong kind for the class. The refusal is local and recoverable: nothing is written and the connection stays
usable. It names the arm and the envelope the class wants, in the sentence the receiving driver would have used. The
sender checks the class only; the identity (the next unissued id, an outstanding request, a live stream) stays the
receiving driver's, since only it holds the sequences. The unchecked test paths (`CheckedFrame::raw`,
`FrameWriter::write_frame_unchecked`) are the sole way to write a frame past this, and exist so a test can play a
malformed peer.

Which families carry the envelope, and on which arms:

| Family                           | Envelope                                                                                                                                                                                                                                                                                 |
| -------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `Session`                        | `InputFence` / `InputAccepted` correlate by `request_id`, so a caller may hold several barriers at once. The openers do not: `Attach` / `Create` happen once per connection, in the phase the driver already sequences, so `Attached` / `Created` / `AttachFailed` answer without an id. |
| `Ops`                            | Every verb is a request whose reply echoes its `request_id`.                                                                                                                                                                                                                             |
| `Region`                         | `Request` / `Reply` correlate by `request_id`; `Rows` opens a stream whose `Row` and `RowsDone` items carry its `stream_id`.                                                                                                                                                             |
| `Notify`                         | `Subscribe` opens a stream; `Subscribed`, `Event`, and `Lagged` are its items.                                                                                                                                                                                                           |
| `Search`                         | `Query` opens a stream; each `Match` carries its `stream_id`.                                                                                                                                                                                                                            |
| `Conn`                           | None. `Cancel` / `End` / `Error` name their subject inline.                                                                                                                                                                                                                              |
| `Input`, `Grid`, `Image`, `Push` | None. Connection-scoped pushes: no reply to match, no stream to close.                                                                                                                                                                                                                   |

**Id ownership.** The client allocates both ids, on **two independent sequences** that start at 1, count strictly
upward, and never reuse a value for the life of the connection. Exhausting a sequence ends the connection rather than
wrapping; the client re-dials.

The daemon holds both counters and checks both: a request id, like a stream id, must be the next unissued value, so a
skipped or reused id ends the connection where it was sent rather than at whatever reply it would later have made
ambiguous. The counter advances even for a request the daemon then refuses through the envelope, so a typed refusal
cannot desync the two ends.

**Opening a stream.** The opening request carries the `stream_id` itself, so a client can cancel a stream that stalls
before its first item without waiting for a daemon-assigned id. That id must equal the daemon's `next_stream_id`, which
then advances. The daemon therefore classifies any `stream_id` against the active set plus that counter:

| Classification | Condition                                        |
| -------------- | ------------------------------------------------ |
| active         | in the table, producing                          |
| canceled       | in the table, cancel seen, terminal not yet sent |
| terminated     | not in the table and `< next_stream_id`          |
| never opened   | `>= next_stream_id`                              |

Client-side, the allocation and the write are one call: `felis-client-core`'s `open_stream` writes the opening request
under the id `begin_stream` issues, so the id, the `stream_id` envelope, and the `Setup` → `Observing` transition an
observer's `Notify::Subscribe` needs cannot come apart. `Connection::open_stream` and
`Connection::subscribe_notifications` are the attached and observer entry points; a caller that must register a receiver
between the allocation and the write (the `felis bridge` link) hands that registration to the same helper.

**Terminals.** Every stream ends in exactly one `ConnToClientMsg::End { stream_id, count }` or
`ConnToClientMsg::Error { subject, reason, detail }` (REQ-113). The terminal lives on the control family rather than
once per streaming family, so one terminal per stream is enforced in one place. An item or a second terminal after a
stream's terminal is corruption ("Corruption" below), as is an item or a `Cancel` naming a never-opened id.

**Cancel.** `ConnToDaemonMsg::Cancel { stream_id }` asks the producer to stop. It is best-effort and races the terminal
in both directions: after sending it the client keeps accepting items for that stream until the terminal arrives
(in-flight items are expected, and dropped), and the daemon treats a cancel for a terminated stream as an idempotent
no-op. A canceled producer still owes its terminal: "stop producing" is not "stop existing", and a client waiting on a
terminal that never came would hang.

**Producers are chunked.** A stream producer emits a bounded slice and yields: the session lock is released between
slices, so a cancel, a keystroke, and the PTY drain all get a turn mid-walk. No producer materializes its whole result
before emitting.

**Bounded concurrency.** A connection may hold `MAX_OUTSTANDING_STREAMS` (32) streams open at once. A stream-opening
request past the bound draws a typed refusal, `ConnToClientMsg::Error { stream_id, reason: TooManyStreams }`, which is
also that stream's one terminal. The connection stays usable: the peer asked for something reasonable at a bad moment,
which is not corruption.

`StreamErrorReason` is a closed set, so a client picks its remedy from the reason rather than from prose:
`InvalidRequest` (malformed or unservable as asked; re-issuing fails the same way), `TooManyStreams` (retry when an
earlier stream terminates), `Unavailable` (the subject went away mid-flight), `Internal` (the daemon abandoned the
stream on its own fault). `detail` says which value provoked it and is never parsed.

For design rationale regarding body envelopes and stream cancellation, see the design record in
[ipc.md](../explanation/architecture/ipc.md). The state machine behind all of this is one shared typed connection
driver, `crates/felis-transport/src/driver.rs`, run by the daemon and by every client: phase ("Connection phases"
below), per-direction message legality, connection mode, correlation, and the cancel/terminal races are validated in a
single place rather than once per call site.

## Corruption

The failure unit is **one connection** (REQ-114). An unexpected frame kind, a wrong-direction message, an undecodable
body, or a correlation violation ends that connection with a typed error naming what was expected against what arrived.
The daemon session and every other connection to that daemon are untouched: a corrupt peer can only kill its own
attachment. There is no skip-and-continue anywhere.

"Unexpected" is what the driver's state machine says it is, which the correlation rules above parameterize: an item
racing a `Cancel` is expected; an item on a never-opened or already-terminated stream is not. Unknown frame kinds and
unknown oneof variants are likewise fatal: the effective minor is a send-side contract ("Versioning" below).

Validation falls in two tiers, and a limit's tier is the one its `Checked` and `Outcome` rows in "Semantic limits" above
give it, not one inferred from the kind of value it bounds. **Wire-level validation** is what `codec::decode` performs
on a single body: the protobuf structure, the conversion into the domain type, which refuses a oneof arm this build does
not define, and the caps that table records as read by the receiver at decode, whose breach is a fatal `WireError`. It
is the driver's, and it runs on every path a frame can take, the drain path included: a reader that discards a family
decodes its bodies before dropping them, so a malformed `Grid` body ends the connection of a role that never draws a
grid exactly as it ends a window client's.

**Semantic validation** is every other check a receiver makes, and a check lands there for either of two reasons. Some
need state the decode does not have: a session's image total against `MAX_SESSION_IMAGE_BYTES`, a chunk's place in an
open transfer, a scroll region's, a cursor's, or a viewport's coordinates against the geometry the daemon announced, a
cell's registry id against the table the client has received. Others are body-local and still deliberately the handler's
to answer rather than the connection's: a tag delta over the tag caps comes back as `TagsUpdated { denied }`, a
`SpawnArgs.env_base` over the env caps as `CreateFailure::SpawnFailed`, and an overlong cluster or hyperlink is degraded
by the constructor or table that stores it. A breach on this tier is therefore not automatically fatal.

`packed_cells` is opaque to `felis-protocol` besides, so a row whose packed cells no row codec produced is a well-formed
frame on the wire and a `ShadowError` in the shadow that unpacks it. A `GridMsg::RowDelta`'s row index and body width
sit on this tier as well, read against the protocol-wide `MAX_GRID_ROWS` / `MAX_GRID_COLS`; their row in the table above
states what a breach costs.

Two entries in the table sit outside both tiers. `MAX_REGION_REPLY_BYTES` is the sender's alone: the daemon trims a
region body rather than send one past it, and no receiver checks it. `MAX_BRIDGE_LINE_BYTES` bounds a request line on
the CLI bridge's pipe rather than a frame body, so it is refused as the line is read.

For rationale regarding connection teardown on corruption, see the design record in
[ipc.md](../explanation/architecture/ipc.md).

### Grid admission

A `GridMsg` coordinate, count, or registry value the daemon could not have produced is corruption as well. The client's
shadow refuses it as a `ShadowError` before it touches a cell and closes that attachment; reconnect and rehydrate
rebuild the mirror, and the session and its other subscribers keep running. Each value has one rule:

| Value                                          | Admitted                                                               |
| ---------------------------------------------- | ---------------------------------------------------------------------- |
| `RowDelta` row index                           | below `MAX_GRID_ROWS`                                                  |
| `RowDelta` body width (the row codec's `cols`) | at most `MAX_GRID_COLS`, checked before the decoder reserves its cells |
| a row's sized-cell column                      | inside that row's decoded columns                                      |
| a row's `Cluster` / `Hyperlink` handle         | already installed ("Registry delivery" below)                          |
| `Scrolled.region_top` / `region_bottom`        | `region_top <= region_bottom`, both inside the announced grid          |
| `Scrolled.n_rows`                              | at least 1 and at most the named region's height                       |
| `CursorState.row` / `col`                      | inside the announced grid, whether or not the cursor is visible        |
| `ViewportState.lines_from_bottom`              | at most `max`                                                          |
| `Cluster.id` / `Hyperlink.id`                  | nonzero, within the table caps, and not already installed              |
| `Cluster.text`, `Hyperlink.anchor` / `uri`     | within `ClusterText::CAP` / `LinkText::CAP`                            |

The **announced grid** is the geometry the daemon last stated (`SessionToClientMsg::Attached`,
`SessionToClientMsg::Created`, or `GridMsg::Size`), which it states before composing anything at a new geometry. A
client that resized its own shadow ahead of the daemon honoring the resize measures against that announcement rather
than its own dimensions, and holds the cursor the daemon stated until a geometry can carry it: the correction that
overrules an optimistic resize repeats the rows, not the cursor, whose position the daemon sends only when it moves.

The daemon owes the matching send-side rules. A queued `Scrolled` names the geometry it was recorded under, so a resize
retires every directive not yet shipped and the rows it would have shifted ride the replay instead; and a resize
composes for every subscriber behind its `Size`, since a mirror whose next frame is already requested has nothing else
to answer that request.

A directive is also dropped for any subscriber whose last frame replayed every row of the grid as of that directive:
those rows already hold the shift, and shipping it as well would move them twice. Shifts folded into one directive from
both sides of such a frame replay every row instead, since the frame holds an unknown part of the shift.

Each composition takes the core lock for itself and settles its own scroll accounting under it, against rows whose
geometry and scroll order are the ones it read: a composition that finds the grid scrolled past what its subscriber was
offered replays the rows at that reading instead, since the cells it would encode already hold a shift no directive
accounts for.

A `RowDelta` inside the two bounds applies even when it does not fit the shadow's current dimensions: a lower or wider
row grows the mirror, a narrower one pads it. A resize race gives both an honest reading, which is why the row index and
the body width are the only grid values bounded protocol-wide rather than by the announced geometry.

## Message families

The variant names below mirror the `*Msg` enums in the protocol crate
([`messages.rs`](../../crates/felis-protocol/src/messages.rs)); those modules are the workspace's domain vocabulary,
mapped to the generated `felis.v1` wire types through the anti-corruption layer. Each family is a protobuf oneof whose
field numbers are the ones `felis.proto` declares, frozen for the life of a protocol major; a new arm takes the next
free number. A number the release baseline (the newest published final release, "Wire compatibility gates" in
[testing.md](testing.md#wire-compatibility-gates)) carries is `reserved` on retirement and never recycled; a number
retired while no baseline exists is free again. New variants land at the tail under a minor bump, recorded in the ledger
("Versioning" below).

The families are split by purpose, not pooled into a catch-all: kind 0 (`Conn`) carries the handshake and the stream
lifecycle, and the session, ops, region, notify, push, and search surfaces each ride their own frame kind (`Session`,
`Ops`, `Region`, `Notify`, `Push`, `Search`). `Search` and `Region` are bidirectional conversations on the _attached_
connection: each carries its request and reply halves on one kind, so a connection phase can refuse the conversation
whole.

A family whose arms all travel one way is one wrapper message, `<Family>Msg` (`InputMsg`, `GridMsg`, `ImageMsg`,
`PushMsg`). A family with arms in both directions is two, `<Family>ToDaemonMsg` and `<Family>ToClientMsg`, under its one
kind (`Conn`, `Session`, `Ops`, `Region`, `Notify`, `Search`): a peer decodes a frame of such a kind as the wrapper for
its own receiving direction.

The two wrappers of a family number their `oneof` arms from one sequence, so no field number names an arm in both; a
body naming only an arm of the other wrapper decodes as no arm at all, which is a wrong-direction message ("Corruption"
above). So is a body naming an arm of each wrapper, in either order, although the receiving wrapper alone would read it
as its own arm and skip the other as an unknown field: a receiver checks every top-level field number of such a body
against the other wrapper's arms. The daemon routes on `kind` first, before the body is decoded, and on the arm second.
The design rationale is in [ipc.md](../explanation/architecture/ipc.md).

The family sections below run in conversation order rather than kind order: `Conn` first, then the control families
(`Session`, `Ops`, `Region`, `Notify`, `Push`), then an attached session's own data path (`Input`, `Grid`, `Image`) and
`Search`. Each heading names its kind, and the arm table above is the lookup keyed by kind.

### The arm table

A frame kind is a conversation surface; an operation inside an existing surface is another oneof arm of that surface's
family. **The arm, not the kind, is the routing unit**, and each arm's row is normative:

| Column      | Meaning                                                                                                                                                                        |
| ----------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| direction   | `to_daemon` or `to_client`; the one way this arm may travel, and the direction of the wrapper that carries it.                                                                 |
| correlation | `uncorrelated`, `request_opener`, `request_reply`, `stream_opener`, or `stream_item`; the envelope each demands is tabulated under "Correlation, requests, and streams" above. |
| modes       | The `ConnectionMode`s this arm is legal on, receive side ("Connection modes" below).                                                                                           |
| phases      | A subset of `handshake`, `setup`, `attached`, `observing` ("Connection phases" below). No arm is legal before the preface completes.                                           |
| since       | The protocol minor that introduced the arm, written only where it is not `0`, which is this major's base schema ("Versioning" below).                                          |

The driver enforces the first four columns on every frame it admits. `since` is declarative: the driver holds no minor
to judge it by, and what a receiver owes each addition is the ledger's old-peer-behavior column, not a refusal.

Every family arm declares its row as a `(felis.v1.arm)` field option in `felis.proto`, a `felis.v1.ArmRouting` message
whose entries are the five columns above; `since` is omitted here, as on every base arm, because `0` is the proto3
default and never reaches the descriptor:

```proto
ConnHello hello = 1 [(felis.v1.arm) = {
  direction: DIRECTION_TO_DAEMON
  correlation: CORRELATION_UNCORRELATED
  modes: MODE_WINDOW
  modes: MODE_OPS
  modes: MODE_OBSERVER
  phases: PHASE_HANDSHAKE
}];
```

A non-Rust peer reads the whole matrix out of the descriptor the schema compiles to (`buf build`, or protoc reflection),
with no comment and no Rust source to parse. `felis-protocol`'s `ArmMeta` table asserts against the same options
(`the_schema_declares_the_same_arm_table`), so an arm carrying no option, or one disagreeing with the driver's table,
fails the build. That test is the mechanical check, and the only one: `buf breaking` compares field numbers, names, and
types, and an option is none of those, so the compat gate ("Wire compatibility gates" in [testing.md](testing.md)) is
blind to a routing edit.

The extension number, 50001, lies in protobuf's private range (50000-99999), which no registry allocates: it identifies
the option inside a descriptor pool and nowhere else. A consumer that compiles `felis.proto` together with another
schema extending `google.protobuf.FieldOptions` at the same number keeps the two in separate pools, or renumbers its own
copy; felis claims no global ownership of it.

The kind-level gate the driver applies **before** decoding a body is the fold of that table (a kind is admitted wherever
any arm of it is), so it is deliberately coarser than the arm's own row. `Ops` is admitted on both attach-capable modes
because `Ops::List` is; the mutating `Ops` arms are still refused, after the decode, on a `Window` connection. Reading
the kind gate as the whole rule is the mistake the arm table exists to prevent.

Every received frame is judged by the table, whatever the receiver means to do with it: one it drains unread and one it
holds back until an outstanding request is answered are refused on the same rows as one it consumes: REQ-114 makes a
routing violation end the connection rather than become a dropped frame.

Where every arm of a family declares the same row the fold is exact rather than coarse (`MessageKind::arms_are_uniform`,
true today of `Input`, `Grid` and `Image`). A reader that drains such a family without reading it, as the `felis bridge`
link does with the grid burst and a one-shot verb does with another family's frame, judges it on the kind columns
instead of the arm's row. The envelope, which the frame carries rather than the arm, and the body, which decides nothing
here but is corruption when it does not decode ("Corruption" above), are still checked per frame.

Whether a change earns a new kind or a new arm turns on which surface owns it: **new kind** iff the messages address a
surface no existing kind owns, or iff the refusal must land before the body is decoded, which only the kind fold can do;
**new arm** iff the message is another request, reply, or item shape on a surface that already exists, whatever
correlation class it needs. One surface may own several openers: `Region::Rows` opens a row stream of its own on the
region surface `Region::Request` also serves, and is an arm. Worked examples and the rejected alternative are in
[the IPC design explanation](../explanation/architecture/ipc.md#kind-or-arm).

Wherever a message announces a session's geometry it carries the shared `GridDims { rows, cols, pixel_w, pixel_h }`
struct (pixel dims are `0` where the producer does not know them). Announced geometry is always **admitted** geometry:
inside the REQ-605a bounds, resolved and clamped once at admission, and a receiver **re-admits** it on decode against
the same bounds rather than trusting the announcement. A `GridMsg::Size` or a `SessionInfo` is what sizes a client's
shadow grid, so an out-of-range announcement is `WireError::OutOfRange` and the connection ends ("Semantic limits"
above).

Which values a `GridDims` may hold is a property of the field carrying it, not of the type:

| Field                                                      | Zero rows/cols                                                            | Out of band                                             |
| ---------------------------------------------------------- | ------------------------------------------------------------------------- | ------------------------------------------------------- |
| `SpawnArgs.dims` (optional)                                | refused; the _absent_ field is what asks for the daemon default (24 × 80) | refused, `CreateFailure::GEOMETRY_OUT_OF_RANGE`         |
| `InputResize.dims`                                         | clamped up to the minimum                                                 | clamped into the band                                   |
| every other `GridDims` (`GridSize`, `SessionInfo.dims`, …) | refused                                                                   | refused, `WireError::OutOfRange`, ending the connection |

`pixel_w` / `pixel_h` `0` means "unknown" on all of them, and stays legal everywhere. Each carrying message is described
under its own family below.

### Conn (kind = 0)

Bidirectional. Kind 0 carries the application handshake and the stream lifecycle: a `Cancel` and a terminal name their
subject by id and need no family context, so they live here once instead of once per streaming family.

- `Hello { mode, pull_paced }`: client → daemon, first message on a connection. Version selection already happened in
  the preface, so nothing here negotiates one. `mode` is a `ConnectionMode` saying what the connection is for, which
  decides the arms the daemon admits ("Connection modes" below); it is minor-gated like any other addition, so the
  sender names only a mode the effective minor defines. `pull_paced` states whether the client paces grid emission with
  per-vsync pulls.
- `Welcome { identity }`: daemon → client reply. It echoes nothing back, and it carries **no** session roster: listing
  is `OpsToDaemonMsg::List` (below), so an attach never pays for the marginalia payload and a live connection can
  re-list at any time. `identity` is a `BuildIdentity` (`version`, a semver; `revision`, full lowercase hex or
  `unknown`; `dirty`) naming the build the running daemon is, distinct from the on-disk binary, for `felis version` to
  show. It is informational only; the version gate is the preface.
- `Refused { reason: RefusalReason, detail }`: daemon → client, sent in place of the answer the peer asked for, after
  which the daemon closes. `reason` has three arms:

  - `Role`: the frame the peer sent belongs to a family its stated mode does not admit.
  - `AtCapacity`: the daemon is already serving the connection cap (`MAX_CONNECTIONS`, 1024) and admitted no further
    connections. The peer did nothing wrong; the remedy is to retry once a connection frees, and `detail` names the
    admitted count and the limit.
  - `UnknownMode`: the `Hello` named a `ConnectionMode` this daemon does not define, so the peer is the newer half and
    the remedy is a newer daemon; `detail` names the mode number.

  `detail` provides human-readable specifics for logs and display and is never parsed. Without this frame, a refusal
  would be a bare close that a client could confuse with a transient EOF and retry indefinitely. This is the pre-attach
  refusal shape. Once attached, the daemon answers invalid requests with
  `Error { subject: Request(id), reason: InvalidRequest }` and the connection survives ("Ops (kind = 5)" below).

- `Cancel { stream_id }`: client → daemon, best-effort, racing the terminal in both directions ("Correlation, requests,
  and streams" above).
- `End { stream_id, count }`: daemon → client, a stream's clean terminal. `count` is the results that preceded it (rows,
  matches, notification events), never the acks, markers, and family trailers riding the same stream
  (`NotifyToClientMsg::Subscribed`, `NotifyToClientMsg::Lagged`, `RegionToClientMsg::RowsDone`). What sorts a new frame
  is the request: only the values it asked for are results. An acknowledgement, a loss marker, or a family trailer
  escorts those values without joining them, a trailer included when it carries data `End` has no field for. `0` is
  normal (no hits, an empty region), and the terminal is what says the stream ended cleanly.
- `Error { subject, reason: StreamErrorReason, detail }`: daemon → client. A request that produced no reply, or a
  stream's terminal when it ends badly. `subject` is required and is either a `request_id` or a `stream_id`: an error
  nobody can attribute is worse than none.

#### Connection phases

A connection's phase is how far it has progressed **and** what it has become; it is the `phases` column of the arm
table, and both ends run the same ladder in the shared driver.

| Phase       | The connection is                             | Admits                                                                                                                                                       |
| ----------- | --------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `Preface`   | exchanging version bytes                      | no frame at all                                                                                                                                              |
| `Handshake` | between `Hello` and `Welcome`                 | `Conn::Hello` / `Conn::Welcome`                                                                                                                              |
| `Setup`     | welcomed, and has not yet said what it is for | the `Session` openers and their acks, every `Ops` verb, `Notify::Subscribe`                                                                                  |
| `Attached`  | subscribed to a session                       | `Session::Detach` / `ConfigureTheme` / `InputFence` / `InputAccepted`, `Input`, `Grid`, `Image`, `Push`, `Region`, `Search`, and every `Ops` verb but `Stop` |
| `Observing` | serving a notification stream                 | `Notify::Subscribed` / `Event` / `Lagged`                                                                                                                    |

`Conn::Refused` is legal in every phase, and `Conn::Cancel` / `End` / `Error` in every phase a stream can be live in
(`Setup`, `Attached`, `Observing`): they are stream control, not session traffic.

The transitions:

| From → to               | On                                                                                                                          |
| ----------------------- | --------------------------------------------------------------------------------------------------------------------------- |
| `Preface` → `Handshake` | the preface accepted                                                                                                        |
| `Handshake` → `Setup`   | `Conn::Welcome`                                                                                                             |
| `Setup` → `Attached`    | the subscription landed: the daemon moves before it writes `Session::Attached` / `Created`, the client on decoding that ack |
| `Setup` → `Observing`   | `Notify::Subscribe`: the daemon moves before it writes `Notify::Subscribed`, the client before it writes the subscribe      |

There is no phase for teardown and no transition out of `Attached` or `Observing`. `Session::Detach` and a stream's
terminal end the conversation, and the connection then ends: closing it _is_ the teardown. A window that wants another
session dials a new connection.

Every inbound frame passes the ladder exactly once, before any handler side effect: the kind fold is checked before the
body is decoded, the arm's own row after. A frame parked while a correlated request is outstanding is classified when it
is consumed, never when it is parked.

#### Connection modes

`Hello.mode` is the connection's purpose, and the daemon admits only the arms that purpose needs: the kind first as a
pre-decode filter, the arm after the body decodes ("The arm table" above). The modes are not a ladder: none is a
superset of another, and a connection can never reach a surface it never asked for.

A mode is **routing and output shaping, never authorization**; the trust boundary is the carrier socket
("Authentication" below). For authorization architecture and mode design rationale, see
[ipc.md](../explanation/architecture/ipc.md).

The admitted rows below are the `modes` column of the arm table ("The arm table" above), projected onto the three modes;
where a family's arms disagree the row names the arms, not the kind, which `Ops` and `Push` are the two families to do.
The `is sent` rows underneath are a different fact: what the daemon _chooses_ to write to a subscriber, which is
narrower than what the mode admits on receipt, and which the daemon may narrow further without a wire break.

| Admitted message                                                                               | `Window` | `Ops` | `Observer` |
| ---------------------------------------------------------------------------------------------- | -------- | ----- | ---------- |
| `Conn` handshake and stream lifecycle (every arm)                                              | ✅       | ✅    | ✅         |
| `Ops::List` / `Ops::Listed` (roster query)                                                     | ✅       | ✅    | ❌         |
| `Ops::Info` / `Ops::InfoReply` (one session's row by prefix)                                   | ✅       | ✅    | ❌         |
| `Ops::Status` / `Ops::StatusReply` (daemon report)                                             | ✅       | ✅    | ❌         |
| `Ops` mutations and their replies (`Spawn`, `Destroy`, `ForceDetach`, `Switch`, `Tag`, `Stop`) | ❌       | ✅    | ❌         |
| `Input`, `Grid`, `Image`, `Session`, `Region`, `Search` (every arm)                            | ✅       | ✅    | ❌         |
| `Push::Evicted`                                                                                | ✅       | ✅    | ❌         |
| `Push::Reattach` / `SessionExited` / `RetargetHost`                                            | ✅       | ❌    | ❌         |
| `Notify` (every arm)                                                                           | ❌       | ❌    | ✅         |

| Sent payload                                               | `Window` | `Ops` | `Observer` |
| ---------------------------------------------------------- | -------- | ----- | ---------- |
| the notification `Attention`                               | ✅       | ❌    | ❌         |
| the rehydrate markers with `KittyKbdFlags` and `ModeFlags` | ✅       | ✅    | ❌         |
| the full attach burst (rows, cursor, meta, images)         | ✅       | ❌    | ❌         |
| the live grid stream (rows, cursor, viewport, scrolls)     | ✅       | ❌    | ❌         |
| live image events and the session-meta facets              | ✅       | ❌    | ❌         |

- **`Window`** is an interactive window: it attaches to one session and streams it, and reads the roster to populate its
  session picker. The window-management pushes above are its alone, since each one moves or alerts a window and no other
  mode has one.
- **`Ops`** is a scripted operator: every `felis sessions` verb, and the stdio bridge's own connections. It owns the
  mutating verbs, and attaches when a verb has to read or drive a session (`capture`, `search`, `send`). Those
  window-management pushes do not reach it, since a verb mid-read owns no window to move and dropping its subscription
  would break the read. `Evicted` is the exception, and reaches every subscriber ("Push (kind = 8)" below).

  The grid is not streamed to it either: an `Ops` attach reads the session through the `Region` / `Search` reply it asks
  for, so no rows, cursor, viewport state, scroll directives, image bytes or placements, session-meta facets, or id
  registry entries ("Registry delivery" below) reach it. The one image event that does is an animation's `ShowFrame`
  tick, which the session broadcasts to every subscriber rather than composing per attach; that is why the `Image` arms
  are legal on both attach-capable modes, and an `Ops` verb, holding no placement to advance, ignores it. What does
  arrive on the grid stream is what a verb acts on without a grid: the two burst markers, the keyboard modes
  `send --key` encodes its chords against, and the prompt marks `send --wait` blocks on ("Grid (kind = 2)" below).

- **`Observer`** is `felis notifications subscribe`. It subscribes to the notification fan-out and nothing else: it
  neither attaches nor mutates, and even its `--session` filter resolves daemon-side
  (`Notify::Subscribe.session_prefix`), so it never reads the roster.

The three pure queries, `Ops::List`, `Ops::Info` and `Ops::Status`, are open to both attach-capable modes. Every verb
that _mutates_ names its session by prefix in the request itself, so no mode needs `List` just to build an id.

**A mode added later is an additive minor.** Each mode carries a row in the minor ledger ("The minor ledger" below), and
a client sends only the modes the effective minor defines: naming one a daemon predates would cost the connection at the
daemon's decode, so the client refuses before the `Hello` goes out. The daemon holds the other end of the same rule: a
`Hello` whose mode it cannot read is answered `Refused { UnknownMode }` and then closed, rather than dropped as a
corrupt body. Every mode this build ships belongs to the base schema, so the rule binds nothing today and is what the
first added mode lands on.

The `pull_paced` field of `Hello` is the one optional _optimization_, a feature flag rather than a mode: when true, the
client's `InputMsg::NextGridFrame` pulls pace grid emission; when false the daemon eager-pushes ("Versioning" below).

A non-Rust implementation needs no contract manifest beside this page: `felis.proto` carries the message and enum
shapes, the `FrameKind` values, and the preface constants, and what remains outside it is the frame-header layout, the
preface byte layout, and the row codec's `packed_cells` payload, the first two specified above and the last in
[Row codec](row-codec.md).

### Session (kind = 4)

Bidirectional, _this connection's own_ session: the attach / create requests and their replies before attach, plus
detach, theme configuration, and the input barrier while attached. Operating on _other named_ sessions is the `Ops`
family. Variant names drop the `Session` prefix; the family carries it. Open to the `Window` and `Ops` modes alike: the
GUI window and `felis sessions spawn` both create sessions. In wire-tag order:

- `Attach { target, live_only }`: client → daemon. Subscribe to the named idle session and start streaming. `target` is
  a union of `id` (a full 16-byte session id) and `id_prefix` (a lowercase-hex prefix), so a frame naming both targets
  is not something a sender built from the schema can write; one naming neither is refused at decode. An `id_prefix`
  decodes as sent; the daemon strips an optional `0x` and lowercases it, and one that is then empty, longer than 32
  digits, or not hex is answered `AttachFailure::NoMatch`, exactly as the `Ops` verbs' `id_prefix` resolves: the two
  share one resolver so they cannot disagree on what a prefix is.

  A prefix is resolved against the pool inside the daemon, under the lock that takes the session handle, so an attach
  costs one round trip and no roster the client holds can go stale between the two; `Attached.info.id` carries the full
  id back. A teardown still races the subscription that follows, which answers `AttachFailure::SessionEnding`, so a
  session reaped mid-request does not always read as `NoMatch`. The exact form stays for the callers that already hold
  an id and did not shorten it: the reconnect ladder's trail entries and a roster-driven switch. Attach is additive; see
  the mirroring discussion in [session-lifecycle.md](../explanation/architecture/session-lifecycle.md). Multiple
  same-user clients may subscribe to one session at once, each with its own rehydrate burst, diff cursor, and pull
  cadence.

  `live_only` refuses the attach with `AttachFailure::SessionExited` when the session shell has exited. Every landing
  the client _picked_ sets it (a switch chord's roster pick, the exit ladder's automatic trail and ring rungs, the
  re-attach after transport loss), and the session task answers it against its own view of the exit, which is the only
  check the exit cannot race. A landing the user named leaves it `false` and may land on an exited session, including
  one the exit ladder runs on the user's behalf, since a parked `Reattach` or an attach-form retarget keeps the attach
  mode the user's own invocation had (see [session-lifecycle.md](../explanation/architecture/session-lifecycle.md)
  "Picking the session a chord lands on").

- `Create { args: SpawnArgs }`: client → daemon. Create a session through the daemon's factory **and attach this
  connection to it**, in one step; the daemon replies `Created { info }` only after the subscription landed, so the
  rehydrate burst follows it exactly as it follows an `Attach` ack. Empty `SpawnArgs` fields fall back to daemon
  defaults (`$SHELL`, the daemon's cwd), and an absent `SpawnArgs.dims` is how a create asks for the default 24 × 80
  grid. A `dims` that is _present_ must name rows and columns inside the REQ-605a bounds, so `0` rows or columns is
  refused rather than defaulted; only the pixel axes keep `0` = unknown (the `GridDims` table above).

  A caller that wants no window uses `Ops::Spawn` instead; that is what `felis sessions spawn` sends. The GUI window
  path drives creation here, and bare `felis -- <cmd>` fills `SpawnArgs.command` / `args` to run that argv in the new
  window: an _ordinary persistent_ session, not a transient one (lifecycle in [cli.md](cli.md)). `SpawnArgs.tags`
  applies opaque labels at creation (normalized exactly as `Tag`/`TagsUpdated` would), so the session is never listable
  untagged.

  `SpawnArgs.command`, `args`, and `cwd` are `string`, so they carry UTF-8 only, unlike the `bytes` of `env_base` below.
  A Unix-native program path, argv entry, or cwd is arbitrary bytes and so may fail to be UTF-8, rarely and never when
  the user typed it; a valid Windows UTF-16 value always converts. A client holding such a value refuses it before the
  wire rather than substituting U+FFFD, which would name a program or directory the user never asked for (REQ-105b).
  `felis sessions spawn` and `felis window retarget` answer one with a usage error ([cli.md](cli.md)).

  A create carries the child's environment in two fields, and which type each uses is fixed by where its content comes
  from:

  | Field                | Wire type                                                    | Origin                                              | Effect                                                                                    |
  | -------------------- | ------------------------------------------------------------ | --------------------------------------------------- | ----------------------------------------------------------------------------------------- |
  | `SpawnArgs.env`      | `repeated EnvPair` (`string` key and value, UTF-8)           | typed by a user — `--env KEY=VAL`, a bridge request | overlaid **last**, over the resolved base and over the daemon's identity stamps (REQ-912) |
  | `SpawnArgs.env_base` | `optional EnvBase` of `EnvBytesPair` (`bytes` key and value) | captured from a host by the dialing process         | **replaces** the base the daemon would otherwise resolve                                  |

  `SpawnArgs.env_base` is the environment the child's base is **replaced** with, no overlay. It carries raw platform
  bytes (Unix bytes; Windows little-endian `u16` code units, whole units only), not strings, and it carries
  **presence**: an empty-but-present snapshot means an empty base. Whichever process dials the `Create` fills it,
  immediately before dialing, and **only when its carrier to the target daemon is the local socket**. Why the two fields
  differ in type and who is expected to fill which is in
  [the IPC design explanation](../explanation/architecture/ipc.md#the-two-environment-fields).

  When the field is absent the daemon resolves the base through one frozen chain: the connection's relay carrier block
  if it has one, else the daemon's own environment. Then, in order: the base, the daemon's denylist scrub, the forced
  identity stamps, and explicit `env` last, which may override a stamp (REQ-912). A base source is scrubbed
  **silently**; an explicit `env` pair naming a reserved key is refused.

  Two values come out of the resolved base rather than out of the daemon: the `$SHELL` a create with no `command` runs,
  and the `FELIS_TERM` / `FELIS_TERM_PROGRAM` identity hatch, read before the scrub that removes them. Each falls back
  to the daemon's own environment when the base is silent ([terminal-identity.md](terminal-identity.md)).

  One stamp is conditional where the identity stamps are not: on macOS the daemon fills `LANG` from the host's current
  region between the stamps and the explicit `env` pairs, and only when the same entries the hatch is read from set none
  of `LANG`, `LC_ALL` and `LC_CTYPE` to a non-empty value ([terminal-identity.md](terminal-identity.md)).

  The daemon stays authoritative for validity, for both fields alike: names are nonempty and contain no `=` or NUL,
  values contain no NUL, Windows byte strings are whole `u16` code units, and Windows `=`-prefixed pseudo-variables
  (`=C:`) are dropped from `env_base` silently and refuse an explicit `env`. Names are compared under the **target
  platform's** semantics (case-insensitive on Windows), canonicalized before the reserved and denylist checks, before
  dedup, and before overrides are applied, so `felis_session_id` cannot slip past an exact-case check. Case-colliding
  explicit `env` pairs refuse the spawn; case-colliding `env_base` entries dedup with the later entry winning. Anything
  invalid, and any cap breach, is a `CreateFailure::SpawnFailed`. No refusal and no log line carries an environment name
  or value the caller supplied.

  The two `env_base` caps ("Semantic limits" above) are checked on the daemon in the spawn path rather than in
  `codec::decode`, and by the dialing process before it sends: a process whose own environment breaks one **omits** the
  field instead of sending it.

- `Detach`: client → daemon, sent while attached. The window is closing; the session lives on.
- `ConfigureTheme { fg, bg, cursor }`: client → daemon, right after attach. The client's configured colors (each an
  optional sRGB triple), so an `OSC 10/11/12 ; ?` query answers the real surface color. One atomic report per window:
  the daemon stores it against the reporting subscriber and resolves the trio a query sees from the first window in the
  candidate chain that has sent one: the last window input owner, then the remaining windows in reverse attach order,
  scripted `Ops` subscribers never. The chain is re-resolved after every report, attach, detach, and ownership transfer,
  so the report travels with every attach and an empty chain (the last window detached) answers with the compiled-in
  fallback
  ([session-lifecycle.md "Client-derived presentation state"](../explanation/architecture/session-lifecycle.md)).
- `Attached { info: SessionInfo }`: daemon → client, the `Attach` ack. The rehydrate burst follows. `info` is the
  session's full roster row (the same `SessionInfo` an `Ops::Listed` reply carries), so a client seeds its session list
  without fabricating a placeholder from a bare id + dims pair.
- `AttachFailed { reason, detail }`: daemon → client. Written on every refusal path of `Attach` and `Create` before the
  daemon closes, so a caller never has to read a bare EOF as a refusal. `reason` is a two-arm union naming which half of
  the request refused, and an unset one is refused at decode:

  - `attach: AttachFailure`: the attach half, which needs a session that already exists. `UnknownSession` (no pooled
    session holds the id), `SessionEnding` (the reap raced the attach), `SessionExited` (a `live_only` attach named a
    session whose shell has already exited; the session is pooled and its task is running, so this is neither of the
    previous two), `NoMatch` / `Ambiguous` (an `id_prefix` named no session, or more than one; the count rides in
    `detail`).
  - `create: CreateFailure`: the spawn half, which is about a session that does not exist yet. `SpawnFailed` (the create
    could not start its program), `GeometryOutOfRange` (the create named a geometry outside the REQ-605a bounds; nothing
    was execed, and the same request fails the same way), `SessionLimitReached` (the create arrived while the daemon
    already held its configured maximum of sessions; the request is fine and nothing was executed, so the remedy is to
    reap a session, REQ-915), `DaemonDraining` (the create arrived while the daemon was draining toward exit; no reap
    makes room, so the remedy is another daemon, REQ-917).

  Which values each request admits is exact, and a client is entitled to refuse anything else as a protocol failure
  rather than report it as a refusal: an `Attach` naming a full id admits `attach ∈ {UnknownSession, SessionEnding}`,
  plus `SessionExited` when it set `live_only`; an `Attach` naming a prefix admits
  `attach ∈ {NoMatch, Ambiguous, SessionEnding}`, plus `SessionExited` under `live_only`; a `Create` admits any `create`
  value, or `attach = SessionEnding` alone, which is its spawn landing and its subscription then losing the race against
  a child that ended first. `detail` carries the exec error behind a `SpawnFailed`, the offending axis, value, and bound
  behind a `GeometryOutOfRange`, the admitted count and the limit behind a `SessionLimitReached`, and the session id
  behind the rest; every reason carries one, and it is for logs and display, never parsed.

- `Created { info: SessionInfo }`: daemon → client, the `Create` ack. The connection is already subscribed when it
  arrives, so the rehydrate burst follows exactly as after `Attached`; `info.dims` is the real spawn geometry after the
  `SpawnArgs` defaults resolved. The session answers lookups by id and prefix on every connection before `Created` is
  written, and an ack that fails to write leaves it detached rather than rolled back. A create that fails _after_ the
  session was registered, the subscribe having failed to land, is rolled back before the refusal is written: the pool
  entry is removed, the session is shut down, and the daemon waits for the child to be reaped
  ([the IPC design explanation](../explanation/architecture/ipc.md#creating-a-session)).
- `InputFence {}`: client → daemon, while attached. A barrier over this connection's earlier `Input` frames, answered
  with `InputAccepted` on the fence's own `request_id`.
- `InputAccepted {}`: daemon → client. Every `Input` frame that preceded the fence on this connection has been processed
  by the session: the bytes the byte-carrying arms (`KeyBytes`, `Paste`, `Key`, `Mouse`) encoded to were admitted
  against the per-session input budget ("Input admission" below) and queued to the PTY writer. It does not say the child
  read anything.

  Every report the session generates for the other arms (a focus report under DECSET 1004 for `FocusChange`, a
  color-scheme report under DECSET 2031 for `ColorScheme`, an in-band resize report under DECSET 2048 for `Resize`) is
  an unreserved write attempted before the session reaches the fence, so the attempt order is guaranteed and delivery is
  not: a saturated writer drops one and the fence still completes. A fence answered while the session is ending says
  only that the bytes were admitted before it ended; once the session has gone the fence gets no reply and the
  connection ends.

### Ops (kind = 5)

Bidirectional request/reply on _named other_ sessions: the scripted operator's family. Every request carries a
`request_id` its reply echoes, so several may be in flight on one connection at once. The three pure queries, `List`,
`Info` and `Status`, are runnable by attach-capable modes (`Window`, `Ops`); mutating requests require `Ops` mode
("Connection modes" above). A `Window` connection that issues one anyway is answered
`ConnToClientMsg::Error { subject: Request(id), reason: InvalidRequest }` and keeps its connection: post-attach the
daemon is streaming a session on that wire, and a mode denial is the verb's answer rather than grounds to drop the grid
under a window. The `Refused`-then-close shape in "Conn (kind = 0)" above belongs to pre-attach connections, where no
session exists yet.

Every mutating request names its session by a lowercase-hex **id prefix** (`id_prefix` / `from_prefix`), resolved
daemon-side against the pool; the paired reply carries the resolution as a `ResolvedId`: `Ok { id }` (the full resolved
id), `NoMatch`, or `Ambiguous { matches }`. A caller therefore needs one round-trip, never a prior `List`. In wire-tag
order:

- `List` / `Listed { sessions: [SessionInfo] }`: client asks for the daemon's current roster; the daemon replies with
  every pooled session, attached or idle. Backs `felis sessions list`; the roster is not part of the handshake
  (`Welcome` carries none), so a live connection can re-list.
- `Info { id_prefix }` / `InfoReply { outcome }`: one session's roster row by prefix, backing `felis sessions info` and
  the bridge's `sessions.info`. `outcome` is a closed union in the `Spawned` style, so only the successful arm carries
  session data: `found { session: SessionInfo, short_id }`, `no_match {}`, or `ambiguous { matches }`. `short_id` is the
  display prefix the daemon shortened against its own pool at reply time, from the same snapshot the resolution ran
  against, so the row, the resolution and the shortening cannot disagree about which sessions exist. Runnable from the
  attach-capable modes, like `List`.
- `Destroy { id_prefix }` / `Destroyed { resolved: ResolvedId }`: remove a session from the pool and end the program
  running in it (backs `sessions kill`; the signals are in
  [session-lifecycle.md](../explanation/architecture/session-lifecycle.md) "Destruction"). A `NoMatch` / `Ambiguous`
  resolution maps to a CLI 1-exit rather than a hard error.
- `ForceDetach { id_prefix }` / `Detached { resolved: ResolvedId, was_attached }`: return a session to the idle pool by
  evicting every client that streams it (backs `sessions evict`, [control-surfaces reference](control-surfaces.md)). The
  daemon sends the live client a `PushMsg::Evicted`, pools the session, then replies; `was_attached` is false when the
  session was already idle.
- `Switch { from_prefix, target: SwitchTarget, scope: SwitchScope }` /
  `Switched { from: ResolvedId, to: ResolvedId?, queued, denied: SwitchDenied? }`: ask the windows attached to the
  session `from_prefix` resolves to, to move to `target` (backs `sessions switch`, `felis ssh`, and
  `felis window retarget`). `SwitchTarget` is a two-arm union, and the arm picks both the push and its gate:
  - `Session(prefix)`: another session on this daemon, also named by prefix. The daemon resolves it, pushes
    `PushMsg::Reattach` to `from`'s _window_ subscribers, and reports `queued` and the target resolution in `to`. A
    target that resolves to `from` itself is answered without resolving `scope` at all (`queued: 0`, `denied` unset),
    because the asked-for state already holds and no window has to be chosen to reach it.
  - `Carrier(RetargetTarget)`: a session on another daemon. The daemon pushes `PushMsg::RetargetHost { target }` to
    `from`'s window subscribers, relaying the descriptor verbatim; the reply carries `to: None` (this daemon cannot
    resolve another daemon's roster).

  `scope` is the orthogonal axis, _which_ of `from`'s windows move, and it is a two-arm union the daemon resolves inside
  the session actor, in the same step that enqueues the push. It is mandatory: a `Switch` that carries no scope is
  malformed and is refused at decode.
  - `Default`: the session's **last window input owner**, else the sole attached window. Neither available is a
    `denied: NoInputOwner` reply, never a guess among several windows and never a silent fallback. This is what the
    in-window verbs send.
  - `Attachment(id)`: one window, by the `Attachment.id` the roster reports. An id that is not attached to this session
    is a `denied: NoSuchAttachment { attachment }` reply; ids are never reused, so that can only mean "gone".

  `queued` counts the subscriber outboxes that took the push. It is **queue admission, not landing**: the reply is
  written the moment the frames are enqueued, and the landing arrives later, on a connection of its own, which this
  reply cannot await. The boundary is argued in
  [the IPC design explanation](../explanation/architecture/ipc.md#what-a-switch-reply-can-report). `denied` is symmetric
  with it: it is a **resolve-time** verdict only, so no arm of it can arrive once a push is enqueued.

- `Tag { id_prefix, add, remove }` / `TagsUpdated { resolved: ResolvedId, tags, denied }`: add and/or remove opaque
  labels on a session in one round-trip (backs `sessions tag`, whose positional TAGs are the adds and `--remove` the
  removes). The daemon applies adds then removes, sorts, and dedups; `tags` is the resulting set. The set is bounded
  (`MAX_SESSION_TAGS` = 32 per session, `MAX_TAG_BYTES` = 128 per tag): a delta that would breach a cap is refused whole
  (`denied` carries the reason, `tags` the untouched pre-request set), never partially applied. felis never interprets a
  tag (the marginalia model, [control-surfaces.md](../explanation/architecture/control-surfaces.md)).
- `Status` / `StatusReply { worker_threads, draining, resources }`: what this daemon is holding (backs
  `felis daemon status`). Live state only: the daemon's build and the wire pair it speaks are already established on
  every connection by `Welcome.identity` and the preface's accept, and a client composes the `version` and `protocol`
  its report prints from those (`felis daemon status` prints the daemon's own minor, not the connection's effective
  one). `worker_threads` is the size of the daemon's async runtime, an identity field rather than a row. `resources`
  carries one `ResourceReport { resource, unit, total_used, scope }` per accounted resource: `resource` is a closed enum
  (connections, sessions, image-store bytes, in-flight decodes and their bytes, subscriber-queue bytes, PTY input
  bytes), `unit` is `COUNT` or `BYTES`, and `total_used` sums every subject. `scope` is a required oneof naming what one
  subject is, and it carries every number that only that answer makes meaningful:

  | `scope` arm            | Carries                                                                                                       |
  | ---------------------- | ------------------------------------------------------------------------------------------------------------- |
  | `DaemonScope daemon`   | `global_limit`. The daemon is its own single subject, so `total_used` is already the deepest                  |
  | `SubjectScope subject` | `subject` (`SubjectKind`: `SESSION` or `SUBSCRIBER`), `max_subject_used`, `per_subject_limit`, `global_limit` |

  A daemon row with a per-subject ceiling is therefore unrepresentable rather than merely undocumented, and a consumer
  pairs each observation with the ceiling in its own arm: `total_used` against `global_limit`, `max_subject_used`
  against `per_subject_limit`. A `ResourceReport` carrying no `scope` is malformed and is refused at decode.

  Each ceiling is a `Limit`, itself a required oneof of `uint64 bounded` and `Unlimited unlimited`: `bounded: 0` is a
  resource nothing may hold, which is not what "felis budgets no ceiling here" means, so neither reading can stand in
  for the other. A `Limit` with no arm, or a `Limit` message absent where the arm declares one, is refused at decode.

  The `felis daemon status` rendering of the same arms, including the omitted key in `--format json`, is in
  [cli.md](cli.md) "Daemon status", which also carries the normative per-resource meanings. Every number is **sampled**
  as the reply is built, each subject read once and the subjects one after another, so a row is that set of samples
  rather than one daemon-wide instant. The report has no session-count field and no grid row. Why the row names its
  dimensions, why it samples rather than counts, and what earns a row are in
  [control-surfaces.md](../explanation/architecture/control-surfaces.md#diagnostic-verbs); why the scope and its
  ceilings are union arms is in
  [the IPC design explanation](../explanation/architecture/ipc.md#typed-unions-for-mode-selecting-fields).

  `draining` says whether this daemon has been asked to stop once it is empty (`Stop` below): while it holds, every
  create is refused and the count the sessions row reports can only fall.

- `Spawn { args: SpawnArgs }` / `Spawned { outcome: SpawnOutcome }` (`Ops` mode only, like every other mutation): create
  a session this connection does not attach to; backs `felis sessions spawn` and the editor bridge's `sessions.spawn`.
  `args` is the same `SpawnArgs` a `Session::Create` carries and the admission steps are identical ("Session (kind = 4)"
  above). The reply is a two-arm outcome: `Ok { info: SessionInfo }`, the new session's full roster row, or
  `Refused { reason: CreateFailure, detail }`. A spawn subscribes to nothing, so its vocabulary is the spawn half's
  alone and the field's type says so; the attach-half reasons a `Session::AttachFailed` can also carry are not
  representable here. A refusal is the reply, not a `ConnToClientMsg::Error`: the request failed, the connection did
  not. Because every `Ops` request carries a `request_id`, several spawns may be in flight on one connection and each
  reply is attributed by id.
- `Stop { mode: StopMode }` / `StopReply { outcome: StopOutcome }` (`Ops` mode only): ask this daemon to exit; backs
  `felis daemon stop`. `StopMode` is a three-arm union rather than a pair of flags, so no request can name two postures:
  `IfEmpty`, `Force`, and `WhenEmpty`, each an explicit arm; an unset oneof is refused at decode. `StopOutcome` answers
  in the same shape: `Stopping`, `Refused { sessions }`, or `Draining { sessions }`, where `sessions` counts what the
  daemon admits: pooled sessions plus the creates in flight, the same number the sessions row of a `StatusReply`
  carries.

  An `IfEmpty` stop against a daemon holding anything is `Refused` and changes nothing; against an empty one it is
  `Stopping`. `WhenEmpty` is `Draining` while sessions remain and `Stopping` when none do; `Force` destroys every
  session and is always `Stopping`. Once a daemon is draining, a further `IfEmpty` or `WhenEmpty` `Stop` answers the
  state it already reported: `Draining` while sessions remain, `Stopping` once none do. Those modes are therefore
  idempotent, while `Force` escalates past the drain and destroys what is left. A `Create` or `Spawn` arriving meanwhile
  is refused `DaemonDraining`.

  The decision and the refusal are one critical section, which is what the verb exists for: a create cannot be admitted
  after a stop was answered `Stopping`, because the admission it needs is refused by the same state that answered. The
  reply is written before the shutdown fires, so the caller reads the outcome of a stop that then happens; the teardown
  a mode owes runs first, which is why a `Force` reply arrives only once every child is reaped. Unlike the rest of the
  family this pair is admitted in the setup phase alone: the reply is the last frame its connection writes.

### Region (kind = 6)

Client → daemon while attached, with the daemon's region replies: the region-export family, open to the `Window` and
`Ops` modes; design in
[scrollback.md "Piping to an external command"](../explanation/data-model/scrollback.md#piping-to-an-external-command)
and [input.md "Action mapping"](../explanation/input.md).

The daemon serializes and the requester routes. Only the daemon holds a grid, so only it can read a region; every sink
runs on the requester's host, where the keymap that named the argv, the path and the pager lives
([input.md "Which daemon runs the command"](../explanation/input.md#which-daemon-runs-the-command)). One source
vocabulary serves both readers of a region: the keymap `pipe` chord asks for a stitched blob (`Request` / `Reply`) and
`felis sessions capture` asks for the same region row by row (`Rows` / `Row` … `RowsDone`). The `run` action never
reaches this family at all: it has no region to read, so the client creates its transient directly through
`SessionToDaemonMsg::Create`. In wire-tag order:

- `Request { source: RegionSource, ansi }`: serialize the named region of the attached buffer, plain by default or with
  reconstructed SGR when `ansi`; soft-wrapped rows are stitched into logical lines. The sink does not travel: the
  requester remembers what it asked for, which is also what keeps a `clipboard` sink's provenance intact (see
  [the design record](../explanation/architecture/ipc.md#pipe-to-clipboard-provenance)).
- `Reply { data, position, exit_code }`: daemon → client, one frame per request, shipped to the **originating**
  subscriber only: a region read is that user's action, not a session event. `data` is empty when the region was absent
  or empty (no closed `OSC 133` range yet); the reply is unconditional so a requester waiting on it is never left
  hanging.

  A stitched region is the only daemon → client body with no bound of its own, so one that serializes past
  `MAX_REGION_REPLY_BYTES` ("Semantic limits") is trimmed by the sender to its youngest resumable boundary rather than
  refused (see [the design record](../explanation/data-model/scrollback.md#a-region-too-large-to-carry)).
  `felis sessions capture`'s row stream reads the same region without the ceiling. `position` is the region's viewport
  anchor as 1-based stitched-logical-line coordinates (`top_line`, `cursor_line`, `cursor_column`), unset for the
  `OSC 133` mark sources, which have none, and for a trimmed reply; the client turns it into the child's `FELIS_*`
  position variables and the built-in pager's `+N` ([keybindings.md](keybindings.md)). `exit_code` is the code the
  youngest `OSC 133 ; D` mark carried, set whenever a region resolved and that mark carried one.

- `Rows { source, ansi, max_rows? }`: serialize the same region row by row instead, the structural read behind
  `felis sessions capture`. It opens a stream: the `Row` items and the `RowsDone` item carry its `stream_id`, a `Cancel`
  stops the walk, and one terminal closes it. The daemon encodes text itself, so a row consumer needs no cell codec:
  plain `text` always ships, and `ansi` asks for each row's SGR reconstruction _beside_ it (one stream carries both
  forms, which is what lets `capture --ansi --format jsonl` compose). `max_rows` emits only the youngest rows (still
  oldest-first) without renumbering: the `--lines N` tail, cut daemon-side so the wire never carries rows the client
  would drop.
- `Row { row, text, ansi?, soft_wrap_continued }`: daemon → client, one region row, oldest-first, to the originating
  subscriber only. `row` is the region's own coordinate space: scrollback rows are negative (`-1` the youngest retained
  row), live grid rows count `0..rows`, and a mark-range row counts from `0` at the range's first row.
  `soft_wrap_continued` ties the row to the one above as one logical line, so a consumer can stitch a wrapped URL / path
  itself.
- `RowsDone { exit_code? }`: daemon → client, the stream's **last item**, not its terminator: the terminal is the shared
  `ConnToClientMsg::End` / `Error` that closes every stream. It ships even for an empty or unresolved region, because
  `exit_code` (mirroring `Reply.exit_code`) is region data a family-agnostic terminal has no business carrying. It is a
  trailer rather than a result, so it is not in the terminal's `count` and an empty region reports `0`.

The rehydrate burst still precedes the row stream on the wire (any attach ships its markers); a capture client drains to
`RehydrateEnd` before writing its request. Under the `Ops` mode that burst is empty of session content: the daemon
serializes the region, so no grid is reconstructed client-side.

### Notify (kind = 7)

Notification-observer connections: the `Observer` mode's whole surface, and a correlated stream like any other.
`Subscribe` opens it, `Subscribed` / `Event` / `Lagged` are its items, and it ends in one `ConnToClientMsg::End` or
`Error`. The subscription is endless by intent; having an explicit terminal lets consumers waiting for an event
distinguish daemon shutdown from idle silence without relying on raw EOF (design in
[notifications.md](../explanation/protocols/notifications.md)).

- `Subscribe { session_prefix? }`: client → daemon, sent in place of an attach: the connection becomes an observer of
  every session's desktop notifications instead of attaching. An optional lowercase-hex id prefix narrows the stream to
  one session, resolved daemon-side like every `Ops` prefix. Backs `felis notifications subscribe`.
- `Subscribed { filter: ResolvedId? }`: daemon → observer, the ack before the event stream. `filter` echoes the prefix
  resolution (`None` when the subscribe carried no prefix), so a bad `--session` surfaces as a typed reply, never a bare
  close. A prefix that resolves to nothing then ends the stream with `Error { reason: InvalidRequest }`.
- `Event { session_id, notification, notify_id, session_title, cwd, attached }`: daemon → observer. One decoded desktop
  notification (OSC 9 / 99 / 777) from any pooled session, carrying the shared `Notification { title, body, urgency }`
  core plus the context the daemon already tracks. `attached` says whether a GUI is live on the session, so a consumer
  can `notify-send` only the detached ones. felis decodes and relays; it never draws a banner. The wire field
  `notify_id` is spelled `notification_id` at the CLI boundary: `felis notifications subscribe --format jsonl` emits the
  key `notification_id` for this value.
- `Lagged { missed }`: daemon → observer. The observer fell behind the daemon's shared notification ring and `missed`
  events were dropped before it caught up. Loss under backpressure is the contract (the daemon never buffers unboundedly
  for a slow reader), but it is never silent: the marker tells a consumer waiting on a specific event that it may be
  among the missed.

### Push (kind = 8)

Daemon → client pushes. `Reattach`, `SessionExited`, and `RetargetHost` each move or alert a window, so the daemon sends
them only to `Window` subscribers, and for the two relayed by an `OpsToDaemonMsg::Switch` only to the ones its `scope`
names ("Ops (kind = 5)" above). `Evicted` goes to every subscriber: it is the last frame before the daemon closes the
connection, and a scripted read wants the same notice a window does. In wire-tag order:

- `Evicted { reason }`: the session this client streamed was force-detached by another process (an
  `OpsToDaemonMsg::ForceDetach`); the client closes its window, the session survives. Distinct from the client-initiated
  `SessionToDaemonMsg::Detach`: the daemon is evicting, not the client leaving.
- `Reattach { id }`: another process asked (via an `OpsToDaemonMsg::Switch` carrying a `SwitchTarget::Session`) that
  this client re-attach to the named session. The client runs its ordinary switch path, so the push cannot put a window
  anywhere a switch chord couldn't.
- `SessionExited { id }`: the session's shell exited (PTY EOF), sent just before the subscription is dropped so a window
  client reads the close as an ending rather than as a lost transport. The window then takes its exit ladder, and closes
  on the shell's last screen only when no rung of it lands
  ([session-lifecycle.md](../explanation/architecture/session-lifecycle.md)). Only `Window` subscribers hear it; a
  scripted `Ops` attach hears nothing and its channel simply closes, which already ends its verb.
- `RetargetHost { target: RetargetTarget }`: another process asked (via an `OpsToDaemonMsg::Switch` carrying a
  `SwitchTarget::Carrier`) that this window leave its current daemon and dial the carrier `target` describes. The client
  detaches, dials, and attaches there, running the same sequential re-dial the switch path runs against one daemon (see
  [session-lifecycle.md](../explanation/architecture/session-lifecycle.md)). Distinct from `Reattach` because the
  payload is a carrier descriptor, not a session id, and the reaction re-dials rather than re-attaching in place. The
  daemon relays `target` **verbatim**: it never inspects the host, socket, or args, so target validity is the client's
  post-dial concern.

  `RetargetTarget` is the carrier descriptor: two nested unions, so a combination that means nothing (an SSH destination
  beside a local socket path, a session prefix beside a respawn command) cannot be written down.

  **`carrier`** says how to reach the target daemon, and is **required**: an unset carrier is a malformed frame.
  - `default_local`: the client's own default local socket. An empty message, since the sending daemon cannot name that
    socket for the client.
  - `local_endpoint: String`: an explicit local socket path.
  - `ssh: SshEndpoint { destination, ssh_args }`: an SSH-stdio carrier. `destination` is passed verbatim to `ssh`
    (`user@host`, a `~/.ssh/config` alias, an `ssh://user@host:port` URI) and `ssh_args` are tokens spliced between
    `ssh` and the destination, uninterpreted. felis models no part of `ssh`'s grammar.

  **`landing`** says what to do once the dial lands, and is **required**: "attach to nothing" and "create nothing" are
  both non-behaviors, so an unset landing is a malformed frame:
  - `attach: String`: a hex-id prefix of a session on the _target_ daemon, resolved by the client after dialing. A
    prefix rather than a resolved id, because the sending daemon cannot resolve another daemon's namespace.
  - `create: SpawnArgs`: create a fresh session there.

  The CLI destination grammar mirrors these unions one-to-one ([cli.md](cli.md)). For cross-carrier re-dial design
  rationale, see [session-lifecycle.md](../explanation/architecture/session-lifecycle.md).

### Input (kind = 1)

Client → daemon, listed in wire-tag order:

- `KeyBytes(Vec<u8>)`: literal bytes, forwarded to the PTY verbatim. What a caller means byte for byte:
  `sessions send --raw`, the `send_string` action, a committed IME string, a file drop's path. A physical keystroke
  rides `Key` instead. Capped at `MAX_RAW_INPUT_BYTES`.
- `Paste(Vec<u8>)`.
- `Mouse(MouseEvent)`: `{ button, action, mods, x, y, px, py }`. `x` / `y` are 1-based cell coordinates; `px` / `py` are
  1-based pixel coordinates within the text area, filled by the client from the pointer's physical position (only the
  client knows the cell metrics). The daemon passes both through and its encoder picks the pair the active mouse
  encoding needs: cells for SGR (`?1006`), pixels for SGR-pixels (`?1016`). Motion is reported at cell granularity (no
  separate sub-cell event), but every event carries true pixels. `mods` is a three-bit set (shift `1`, alt `2`, ctrl
  `4`), the xterm mouse triple, with no Super bit.
- `Resize { dims: GridDims }`: the window's full geometry, pixel dims included, as the client asks for it. The daemon
  **clamps** it into the REQ-605a bounds and applies the clamped tuple; a resize is never refused and never ends the
  connection, whatever the client reports. Pixel `0` stays `0` (unknown) rather than being raised to the pixel minimum.
  The clamping runs over the whole `uint32` domain, before narrowing, so a value past 65535 is clamped like any other.
- `FocusChange { focused }`: this window's own OS focus. The daemon reduces the per-window reports to a session-level
  fact (focused iff any window subscriber is focused) and writes `CSI I` / `CSI O` (under `DECSET 1004`) only on that
  session-level edge. A session whose shell has already exited is silent, on the same rule as the `DECSET 2031` report
  below.
- `ColorScheme { dark }`: the OS light/dark toggle observed by the client. Stored against the reporting subscriber and
  resolved along the same candidate chain as `ConfigureTheme`, independently of it, to the preference `DSR ? 996 n`
  answers with. When `DECSET 2031` is on the daemon reports `CSI ? 997 ; Ps n` to the PTY whenever a resolution
  _changes_ that answer (a fresh report, an attach, a detach, or an ownership transfer, whoever supplied the new value)
  and stays silent when it does not, so the re-report every client sends on attach reaches no program.

  A session whose shell has already exited is silent outright: the report still resolves for later queries, but the
  daemon's own reports (this one and the focus edge above) are never written to a PTY past end of file. The rule covers
  what the daemon sends on its own initiative, not what a user types: input still reaches the PTY, and a write that
  fails there still ends the session.

- `Viewport { lines_from_bottom }`: scrollback viewport request.
- `JumpPrompt { direction }`: prompt-jump request: the daemon resolves the previous / next `OSC 133` prompt-start mark
  to a viewport offset and replies through the normal `ViewportState` path.
- `NextGridFrame`: demand-driven frame pull: the client asks for one coalesced grid diff covering everything dirty since
  the last one (sent only on a connection whose `Hello` stated `pull_paced`; design in
  [rendering/pipeline.md](../explanation/rendering/pipeline.md)). The daemon withholds grid diffs and ships one cycle
  per pull, so the client's vsync paces emission. The cycle it answers is delimited by `GridMsg::CycleEnd` ("Grid (kind
  = 2)" below), and the client holds the pull until that marker. Payload-free: the pull carries no acknowledgement
  position, because the daemon keeps no per-subscriber frame count to compare one against.
- `Key(KeyEvent)`: `{ key, text, mods, kind, location }`. `key` is a named key, the key's own character, or neither (a
  dead key or bare modifier, which falls through to `text`); `text` is the text the OS composed for this keystroke,
  absent when there is none; `mods` is a four-bit set (ctrl `1`, shift `2`, alt `4`, super `8`), wider than the
  `InputMods` the mouse arm carries; `kind` is press, repeat, or release; `location` distinguishes the numpad, which
  DECKPAM encodes differently.

  The daemon encodes the event against the keyboard modes it has parsed (Kitty flags, DECCKM, DECKPAM,
  `modifyOtherKeys`, win32-input-mode), so the bytes follow the session's real state rather than the client's mirror of
  it (design in [input.md](../explanation/input.md) "Keyboard"; byte forms in
  [key-encoding.md](protocols/key-encoding.md)).

  `key`'s character is capped at `MAX_KEY_CHARACTER_BYTES` and `text` at `MAX_KEY_TEXT_BYTES`, both 32 UTF-8 bytes: one
  keystroke's worth. A frame past either cap is refused at admission, like an oversized `Paste`. Admission reserves
  `MAX_KEY_REPORT_BYTES` against the session's input budget, the widest report any mode can produce, because the child
  can flip modes between the reservation and the write.

Search and region requests are not `Input` messages: each rides its own kind ("Search (kind = 9)" / "Region (kind = 6)")
together with its replies.

### Grid (kind = 2)

Daemon → client, grouped by category: rehydrate framing, row content, cursor / viewport, session meta, events. The field
numbers are in `felis.proto`.

- `RehydrateBegin`, `RehydrateEnd`. The client blanks its shadow at the dimensions already announced (the
  `SessionToClientMsg::Attached` or `SessionToClientMsg::Created` this burst answers, or the latest `Size`), so the
  burst itself carries none. Both markers are sent on every attach, whatever the mode: they are the attach-ordering
  barrier a client drains to before it writes its own first request.

  What rides _between_ them depends on `Hello.mode` ("Connection modes" above). A `Window` receives the whole session,
  ordered for first-byte latency: the `Hyperlink` / `Cluster` entries the visible rows reference, then those cell rows +
  `CursorState` (the visible terminal), then title / cwd / theme / Kitty-kbd flags, then images (`Header` → `Chunk`s →
  `Complete`), then `Placement`s, then `RehydrateEnd`. Image bytes are the largest payload and go last so a cold attach
  paints the grid at "round-trip + cells" rather than "round-trip + every image". The registry entries no visible row
  names are not in the burst at all; they stream afterwards ("Registry delivery" below). The local path orders the same
  way; over a 100 ms SSH round trip this ordering is what keeps a cold attach feeling instant.

  Between the markers an `Ops` attach receives `KittyKbdFlags` and `ModeFlags` and nothing else: a one-shot verb reads
  the session through the `Region` / `Search` reply it asks for, so rows, cursor, meta, the prompt marks already on the
  grid, and the image store (up to the 256 MiB per-session store cap, read back out of the store to compose) would all
  be content it decodes and drops. The two mode messages stay because `felis sessions send --key` encodes its chords
  against them ([cli.md](cli.md)). Live `PromptMark` frames still reach an `Ops` subscriber after the burst, which is
  what `send --wait` blocks on.

- `CycleEnd`: the terminator of one pull-paced compose cycle. A **cycle** is everything the daemon composed in answer to
  one `NextGridFrame`: mode flags, prompt marks, viewport state, a scroll directive, the registry entries the rows
  reference, one or more `RowDelta` frames, cursor state, and the registry tail, in that order. The marker follows all
  of them, so exactly one arrives per non-empty cycle and nothing of that cycle follows it. An empty cycle ships nothing
  at all, the marker included: the pull stays armed daemon-side and outstanding client-side, and the next dirty cycle
  answers it. Only a subscriber whose `Hello` stated `pull_paced` receives one; an eager-push peer has no cycle to
  close.

  The marker is what clears the client's outstanding pull and what releases its grid paint, so no redraw source (a
  pending redraw, a cursor blink, an animation tick, a facet push) can put a cycle's `Scrolled` on screen without the
  rows and cursor that follow it. Payload-free, for the reason `NextGridFrame` is ("Input (kind = 1)" above). The
  rehydrate burst is the other boundary and keeps its own: it ships eagerly at attach, answers no pull, and ends at
  `RehydrateEnd`.

- `RowDelta { rows }`: one frame and one socket write per diff cycle, carrying a `(row, packed_cells)` entry per dirty
  row. A single dirty row is a one-entry batch; the many-row cycles (full-screen redraw, resize replay, rehydration) are
  what the shape is for. Entries apply in vector order: a later entry for a row overrides an earlier one. A cycle whose
  rows would together approach the frame ceiling (REQ-105) splits across consecutive frames instead; rows are keyed by
  index and apply independently, so the reader reaches the same screen.
- `Scrolled { region_top, region_bottom, n_rows, direction }`: whole-row shift of the band `region_top..=region_bottom`
  instead of per-row diffs. The daemon emits one for every scroll of the scrolling region (a line feed, `IND`, `NEL` or
  `RI` at a margin, `SU`, `SD`) and for `IL` / `DL`, whose band runs from the cursor row to the bottom margin, whether
  or not the band holds rows written in the same cycle. A band narrowed by DECSLRM left and right margins ships as rows
  instead. `direction` is `Up` / `Down` and `n_rows >= 1`.

  The client's shadow performs the same in-place row shift via `apply_scroll_directive`, filling the vacated rows with
  default cells, and the cycle's `RowDelta` then carries the vacated rows and the rows written since, which are the rows
  the shift does not reproduce. Consecutive shifts of one band in one direction fold into one directive; a directive
  whose whole band the `RowDelta` restates is not sent. A vacated row that the `RowDelta` restates with anything but
  default cells is preceded in the same batch by an entry of default cells for that row. A region or count outside the
  announced grid closes the attachment ("Grid admission" above). The pending scroll-directive queue is cleared on
  rehydrate (the burst restates the grid outright).

- `Cluster { id, text }`: grapheme-cluster registry entry
  ([grid-and-cells.md](../explanation/data-model/grid-and-cells.md)). Cells holding a multi-codepoint cluster carry a
  `NonZeroU32` id on their `RowDelta` instead of the text; the daemon ships each `Cluster` **before** any `RowDelta`
  that references it ("Registry delivery" below). Mirrors `Hyperlink`. `text` is at most 128 bytes (the daemon's fold
  cap), and ids run to 131 072 (the daemon's per-grid table cap). A client refuses id 0, an id past the cap, and
  over-cap text: a table it grew to an arbitrary id would be an allocation the sender chooses.
- `Hyperlink { id, anchor, uri }`: hyperlink registry entry from `OSC 8`, shipped **before** any `RowDelta` that
  references it (same delivery rule as `Cluster`).
- `CursorState { row, col, visible, style, blink }`: the coordinate is inside the announced grid whether or not the
  cursor is visible, a hidden cursor standing somewhere as much as a visible one ("Grid admission" above). A `DECTCEM`
  change is another `CursorState`, not a flag on some other message.
- `ViewportState { lines_from_bottom, max }`: scrollback-viewport mirror; `lines_from_bottom` is at most `max`.
- `Size { dims }`: authoritative (post-clamp, REQ-605a) grid dimensions changed mid-stream; see mirroring in
  [session-lifecycle.md](../explanation/architecture/session-lifecycle.md). Either the active mirror resized the PTY, or
  another mirror owns the size and this message corrects this client's own `InputMsg::Resize`. The shadow resizes to
  match and the renderer letterboxes the difference; cell content rides the normal `RowDelta` replay. No rehydrate is
  needed as dimensions are the only data exchanged.
- OSC-sourced session meta:
  - `Title { value }`.
  - `Cwd { value }`.
  - `PromptMark { line, kind, exit_code }` (absolute line).
  - `ThemeColor { channel, action }`: `action` is an explicit `Set { rgb }` / `Reset` oneof, as `PaletteColor` carries.
  - `PointerShape { name }`: a sanitized CSS cursor keyword the client maps to a `winit::CursorIcon`; `None` resets to
    the default arrow.
  - `KittyKbdFlags { flags }`.
  - `ModeFlags { … }`: the mode-bit mirror.
    - `mouse_protocol` is a `MouseProtocol` enum, `Off` / `ButtonEvents` (`?1000`) / `ButtonAndDrag` (`?1002`) /
      `AnyMotion` (`?1003`).
    - `modify_other_keys` is a `ModifyOtherKeys` enum, `Off` / `Level1` / `Level2`, the xterm `CSI > 4 ; Pv m` levels; a
      higher `Pv` is clamped to `Level2` at the parser.
    - The remaining six are bools: `bracketed_paste` (`?2004`), `alt_screen` (`?1049`), `application_cursor` (`?1`
      DECCKM), `application_keypad` (`ESC =` / `ESC >`, DECKPAM / DECKPNM), `win32_input_mode` (`?9001`), and
      `reverse_video` (`?5` DECSCNM, which the renderer XORs with each cell's SGR 7).
- Indexed palette: `PaletteColor { index, action }`, one `OSC 4` set or one `OSC 104 ; idx` reset. `index` is `0..=255`;
  `action` is an explicit `Set { rgb }` / `Reset` oneof, so a malformed frame cannot decode as a reset. An `OSC 4`
  carrying several `idx ; spec` pairs emits one message per pair. `PaletteResetAll` is the bare `OSC 104`: distinct from
  a per-index reset, and it leaves the `ThemeColor` channels alone. The whole override layer replays on rehydrate,
  ascending by index; no row payload carries a palette entry, since a cell names an index rather than a color.
- Events: `Attention { source }`, the session asking for the user's attention. `source` is an `AttentionSource`: `Bell`
  (BEL events coalesced into one per cycle) or `Notification` (a desktop notification fired on this _attached_ session;
  the content goes to observers via `NotifyToClientMsg::Event`, not here, and this source reaches window subscribers
  only, [notifications.md](../explanation/protocols/notifications.md)). The client's response is the same non-disruptive
  window-attention request for either source; the enum keeps them distinguishable for a client that later wants to style
  them apart.
- `ClipboardSet { selection, data }`, a program's `OSC 52` clipboard write (distinct from a `RegionToClientMsg::Reply` a
  `pipe → clipboard` chord asked for, which is the user's own action). `selection` is a `ClipboardSelection` bitmap (bit
  0 `c`, the system clipboard; bit 1 `p`, X11 PRIMARY) riding a `uint32` on the wire like the other bitflags; unknown
  bits are dropped on decode. Unlike the other facets this message is not broadcast: the daemon sends it to the
  initiating (active) subscriber only: per-client clipboard scope,
  [security-model.md](../explanation/security-model.md).

#### Registry delivery

`Cluster` and `Hyperlink` carry the two id registries a `RowDelta` addresses by handle. One rule governs both, on the
attach burst and on every diff alike: **the daemon sends every entry a row references before that row**, and sends no
entry twice to the same connection.

The rule is causal, not positional. The daemon tracks per connection which ids it has sent, and emits from that set's
complement the ids the rows it is about to ship name, so a client sees ids in reference order: permuted, and with holes
that later messages fill. Neither message family is a table dump, and neither ordering nor density is guaranteed:

- An attach burst carries only what the visible rows name. A session whose scrollback interned 100 000 clusters ships as
  many entries as a screenful of cells uses.
- A diff carries what its dirty rows name. Scrolling back into scrollback ships the entries those older rows name, which
  the burst had no reason to send.
- What no row has referenced still arrives: a window connection receives the unreferenced remainder of both registries,
  lowest id first, drained a bounded number of entries per compose cycle onto cycles that already carry content. This is
  convergence only: the reference rule above is what makes a row resolvable. An `Ops` connection receives neither the
  drain nor the referenced entries, and no row that could name one ("Connection modes" above): it reads the session
  through `Region` / `Search` replies, which carry text the daemon resolved.

A client therefore installs each entry at the id the message states, once, and grows its table sparsely. Sparse means
that ids no row names may be absent; it never means that a row may name an absent id. A second entry under an id the
client has installed is refused whatever it carries, identical text included: cells hold the handle, so the id names one
value for the life of the connection, and an attach burst reaches a table the `RehydrateBegin` emptied. Because the rule
above holds, a row naming an entry that has not arrived is corruption: the client closes that attachment ("Grid
admission" above) rather than draw a cell the daemon never composed. Assuming ids arrive densely from 1, or that a
reattach restates the whole table, is wrong.

The argument for this ordering is in [grid-and-cells.md](../explanation/data-model/grid-and-cells.md).

#### Row codec

When a row is dirty and ships to the client, felis encodes this content:

```
Row {
    graphemes:           [Grapheme],       // one per column
    attr_runs:           [(len, Attributes, link)],
    sized_cells:         [(col, Sizing)],  // OSC 66 side-band
    soft_wrap_continued: bool,
}
```

The byte layout is specified on its own page: [Row codec](row-codec.md) is the normative, language-neutral specification
of the `packed_cells` bytes: field order and widths, the grapheme and attribute-run encodings, the limits checked before
any allocation, the decoding rules, and the golden vectors every implementation must reproduce. Where that page and this
one could be read as disagreeing, that page is right. What follows here is the message-family view (what the field
carries and why it is shaped that way), not a second description of the bytes. The Rust implementation is
`crates/felis-grid/src/wire.rs`, hand-written in both directions with no serde derive on the path, so a type refactor
cannot move the bytes silently.

The payload is opaque to `felis-protocol` and to the schema: `felis.proto` declares `packed_cells` a `bytes` field and
the protocol crate never interprets it. Ownership sits in `felis-grid`, one crate below the protocol boundary.

Attributes and sizings ship **resolved**, never as the grid-local handles a `Cell` holds in memory: ids are grid-local,
so the daemon resolves `StyleId → Attributes` on encode and the shadow re-interns `Attributes → StyleId` into its own
table on decode. Each side interns into its own tables and neither's registry maintenance is visible to the other; see
[grid-and-cells.md "Style interning"](../explanation/data-model/grid-and-cells.md#style-interning). Decoded cells carry
`sizing: None`; the caller stamps OSC 66 handles from the `sized_cells` band; see
[grid-and-cells.md](../explanation/data-model/grid-and-cells.md). The `soft_wrap_continued` bit rides the row payload
([Row codec](row-codec.md) states where it sits and why), and the shadow mirrors it so client-side triple-click stitches
logical lines.

Attributes are run-length encoded: a uniform row collapses to a single attribute run, so the pen serializes once per
row, while the per-column grapheme stream stays a cheap 1–2 byte record per cell. Decode enforces that the runs cover
exactly as many columns as there are graphemes, so a malformed body errors at the frame boundary rather than silently
dropping or duplicating cells. For row codec design rationale and benchmarks, see
[ipc.md](../explanation/architecture/ipc.md).

The payload's leading `version` byte is the version handle: a later codec (columnar, attribute dictionary,
delta-against-previous-row) lands as version 2, leaving version 1 decodable as the documented codec for any second
reader. The byte is invisible to the protobuf envelope (`packed_cells` stays opaque `bytes`) but not to the row decoder,
which rejects a version it does not implement rather than skipping past it. The tag identifies; the effective minor
authorizes: a new codec version is an addition gated on the minor like any other, so a sender never writes a version the
other peer's minor does not define ("Versioning" below).

The session task encodes a row **once per ship cycle** and fans the shared `GridMsg` out to the mirrored subscribers
owed it. Every composition in a cycle reads the grid under one hold of its lock, and the cache starts empty each cycle,
so no payload outlives the grid it was encoded from. A client that cannot decode the packed payload does not mirror the
grid at all: it reads the session through `sessions.capture` on the stdio bridge, whose rows are text the daemon
rendered ("CLI clients" below).

### Image (kind = 3)

Daemon → client. Bytes stream as `Header { id, target }` → `Chunk { id, bytes }`\* → `Complete { id }`:

- `Header` opens a transfer and its `target` says what the bytes become. `New { width, height, format }` is a fresh
  image under `id`, replacing whatever the id held, animation frames included. `Frame { number }` is one frame of an
  image already transferred, in Kitty's 1-based numbering: `1` re-transmits the root frame in place, `number` in
  `2..=frames + 1` edits that frame or appends when it is one past the last. A frame inherits the image's geometry and
  cannot restate it, so the root has exactly one encoding and frame `0` does not decode.
- The byte count a transfer must deliver never travels the wire: felis ships coalesced frames, so every buffer of an
  image is `width × height × bytes_per_pixel` and the geometry states it once. That product is the header's allocation
  claim and is admitted on decode against `MAX_IMAGE_BYTES` (computed saturating, since nothing bounds the two axes on
  their own), and a `Frame` number against `MAX_IMAGE_FRAMES`. A client mirroring the store also holds its total to
  `MAX_SESSION_IMAGE_BYTES` ("Semantic limits" above).
- **One transfer at a time, chunks in order.** `Chunk` appends at the open transfer's own byte count and `Complete`
  closes it. Both name the transfer's `id` as a cross-check. Nine sequences are malformed and end the connection,
  connection-locally:
  - a `Header` while a transfer is open;
  - a `Frame` for an id the receiver holds no image for;
  - a `Frame` number past the append point;
  - a number past `MAX_IMAGE_FRAMES`;
  - a `Chunk` or `Complete` with no transfer open;
  - a `Chunk` or `Complete` naming another id;
  - a `Chunk` carrying more bytes than the geometry accounts for;
  - a `Complete` whose chunks fell short of it;
  - a header whose claim is over either cap.

  A zero-length `Chunk` is a legal no-op. A `Complete` is therefore the proof the buffer is whole: the receiver never
  zero-fills a shortfall and never normalizes a malformed sequence. `Delete` of the image a transfer is open on aborts
  that transfer.

- `Delete { id }`: drop an image and its placements. It also carries the store's own evictions: an image the daemon
  dropped to fit a new transmission or animation frame is announced the same way, so a mirror's byte total tracks the
  daemon's.
- `Placement { image_id, placement_id, anchor_row, anchor_col, cols, rows, source, z_index }` /
  `PlacementRemoved { image_id, placement_id }`.
- `VirtualPlacement { image_id, cols, rows, z_index }`: a Kitty Unicode-placeholder placement (`U=1`). The image is not
  grid-anchored; the producer paints `U+10EEEE` placeholder cells (carrying per-cell row/column diacritics + the image
  id) wherever it wants tiles drawn, and this message only carries the cell extent so the renderer can size each tile.
  Re-sent on each `U=1` transmit for the same id (idempotent upsert), replayed in the attach rehydrate burst after the
  image bytes, and re-stated when an alt-screen exit restores the primary placement context. No removal message exists:
  the extent dies with its image (`Delete`).
- `ShowFrame { id, number }`: display a frame of image `id` now, in the same 1-based numbering the `Frame` target uses.
  Playback timing stays on the daemon: its animation timer emits one `ShowFrame` per advance (and one on reattach to
  restore the live frame), so no per-frame gap travels the wire. Unlike the transfer messages this one is tolerant of a
  frame the receiver does not hold: the daemon collapses superseded transmissions, so a rehydrate can name a frame a
  later eviction in the same drain removed.
- `PlacementsShifted { lines }`: every placement's anchor shifted up by `lines` because the live region pushed that many
  rows into scrollback. The client replays the shift on its placement mirror; placements whose anchor left the retained
  scrollback get an explicit `PlacementRemoved`.

### Search (kind = 9)

Bidirectional on the _attached_ connection: the whole REQ-607 search conversation. Open to the `Window` and `Ops` modes:
the search overlay and `felis sessions search` drive the same conversation. In wire-tag order:

- `Query { query, options }`: client → daemon, opening a stream. Walk retained scrollback plus the live screen
  newest-first and stream the hits. The walk runs in slices, releasing the session lock between them, so a `Cancel` for
  this `stream_id` stops it mid-flight and a keystroke on the same session is not queued behind it. Each slice also
  bounds how many items it _enqueues per turn_, so a scrollback-wide match set never lands as one burst. That bounds the
  burst, not the outbox: the outbox itself is unbounded and gauged by bytes in flight, and a search that outruns its
  reader is cut by that gauge like any other backlog ("Backpressure" below).
- `Match { line_index, text, byte_spans, col_spans }`: daemon → client, one frame per hit logical line: soft-wrapped
  rows arrive stitched, `line_index` is the hit line's **topmost** row, `text` the stitched logical line, and
  `col_spans` a list of paintable `(row, col_start, col_end)` segments; a match that crosses a wrap edge contributes one
  segment per touched row, so segments are not 1:1 with `byte_spans`. Each `Match` carries the query's `stream_id`.

The family has no terminator of its own: a search stream ends in the shared `ConnToClientMsg::End { count }` or `Error`,
like every other stream.

`line_index` follows REQ-608's negative-from-top convention: `-1` is the youngest scrollback row, `-N` older; a live row
reports its grid row index (`>= 0`). A seam-straddling hit anchors at its topmost (scrollback) row and its `col_spans`
carry rows of both signs. Hits iterate newest-first across the whole surface: live bottom up, then scrollback youngest
to oldest. For search matcher design and limits, see
[scrollback.md "Search"](../explanation/data-model/scrollback.md#search). Why search rides the attached connection
rather than `Ops` is in [the IPC design explanation](../explanation/architecture/ipc.md#kind-or-arm).

## CLI clients

Scripted automation drives the daemon through the same wire as the GUI client, exposed via headless subcommands on the
`felis` binary: `felis sessions {list, info, send, kill, evict, switch, capture, spawn, search, tag}`. Non-Rust clients
reach these verbs through `felis bridge` ("Non-Rust clients: the stdio bridge" below). Attaching a window is not a
`sessions` verb: `felis attach <id>` launches the GUI client (`felis-client`) directly (see
[workspace.md](workspace.md)). Bare `felis` execs the GUI client to create a new session. The CLI surface itself is
[cli.md](cli.md): its verbs, flags, exit codes and `--format` machine output are normative there. Surface boundaries are
in [control-surfaces.md](../explanation/architecture/control-surfaces.md).

The verbs ride the families documented under ["Message families"](#message-families) above: the `Ops` requests (`list`,
`info`, `kill`, `evict`, `switch`, `tag`, `spawn`), and the `Search` / `Region` conversations for `search` and
`capture`, with the gating, reply fields, and design-record links stated per family there. A connection is persistent
and multiplexed, so a driver may keep one open and run several requests and streams over it, each correlated by its own
id. The CLI itself still opens one connection per invocation: a one-shot process has one operation to run and exits, so
nothing is left to multiplex. That is an implementation choice, not a wire constraint.

A `tag` write does not wake a parked session: the daemon mutates the session's shared meta directly, with no owner-task
round-trip, and the labels ride back to every reader in `SessionInfo.tags`.

`SessionInfo` (the row in an `OpsToClientMsg::Listed` roster) carries `id`, `dims` (a `GridDims`: the session's
effective geometry, pixel dims included where a client has reported them), `title`, `cwd`, `idle_seconds`, and
`sequence`, plus the `tags` above and two daemon-_derived_ annotations for the fzf/marginalia picker. `sequence` is
required; the optional fields are omitted from the machine framing when absent:

- `title` (`string`): the session's most recent `OSC 0`/`2` window title, absent until a program sets one. With `cwd`
  below, it is what `felis __complete-sessions` renders as a completion's description.
- `cwd` (`string`): the session's most recent `OSC 7` working directory, absent from a shell without the integration.
- `idle_seconds` (`u64`): seconds since the session's last detach, evaluated at list-build time. **Absent while the
  session is attached**, which is its one meaning for "attached": a present `0` is a session detached for under a
  second. A picker that reaps by age therefore tests presence as well as magnitude
  ([reap-sessions.md](../how-to/reap-sessions.md)).

- `sequence` (`u64`, 1-based, **required**): the daemon-assigned creation sequence, monotonic over the daemon's
  lifetime, stamped once when the session is created and never reassigned, so a reap leaves a gap rather than handing
  its place to the next creation. A `0` is refused (`WireError::MalformedField`), and since an omitted field writes `0`
  a row without a sequence is refused on the same rule: every row a daemon lists carries one. It is the order a window's
  `switch_session` chords step through, which random `u128` ids cannot provide (see the session picker discussion in
  [session-lifecycle.md](../explanation/architecture/session-lifecycle.md)). `Listed` stays in daemon recency order (the
  order `felis sessions list` prints); consumers that want ring order can sort by `sequence`.

- `last_notification`: the session's most recent decoded OSC 9/99/777 desktop notification
  (`{ notification: { title?, body, urgency }, age_seconds }`), stashed in `SessionMeta` by the notification fan-out
  (even while parked) and age-stamped at list-build time. The one typed status a full-screen TUI agent (Claude Code)
  emits, since it sets no OSC 133 prompt marks: the picker's "blocked / done" column.
- `foreground`: the `comm` of the PTY foreground process group, both taken while the roster is built: `tcgetpgrp` on the
  PTY master, then the name (`/proc/<pgid>/comm` on Linux, `proc_pidpath` on macOS). The column therefore names the
  program that owns the terminal at query time even for a session that has produced no output since. Read from the OS
  process table by pid, never the shell's output (principle 4).
- `exited` (`bool`): the shell exited and the session is lingering in the post-exit grace
  ([session-lifecycle.md](../explanation/architecture/session-lifecycle.md) "Post-exit reaping"). A client's automatic
  pick must skip these (attaching one blocks the reap and nothing ever pushes the window off it); deliberate attaches
  stay allowed, since viewing the corpse's final screen is what the grace is for.
- `last_exit_code` (`u32`): exit code of the session's most recent completed command: the youngest retained OSC 133 `D`
  mark's code, sampled from the grid into `SessionMeta` on each drain cycle. Absent when the shell sets no OSC 133 marks
  (a full-screen TUI agent, a shell without integration), when nothing has completed yet, or when the youngest `D`
  carried no code (an older mark's code is never substituted; it describes a different command). The typed "did it
  succeed" signal for a scripted driver.
- `attachments`: the session's live **window** attachments, oldest first,
  `{ id: u64, attached_at: Timestamp, input_owner: bool }` each. Always present (empty for a parked session, and for one
  only scripted `Ops` readers are on: an `Ops` attach has no window, so it is not addressable as a switch target). `id`
  is the daemon-lifetime attachment id (allocated per window attach, unique across every session the daemon holds, never
  reused), and it is the value a `SwitchScope::Attachment` names. `attached_at` is a `google.protobuf.Timestamp` on the
  daemon's clock, absolute where the roster's other stamps are ages, because an attach _instant_ is a fact about the
  daemon's clock a cross-host reader cannot re-derive from its own. A value outside that type's range (years 0001-9999),
  or with `nanos` outside `0..1e9`, fails the decode. `input_owner` marks the session's last window input owner; exactly
  one attachment reports `true`, or none while the marker is clear.

`SpawnArgs` (the GUI-launch payload on `SessionToDaemonMsg::Create`, and the `sessions spawn` payload on
`OpsToDaemonMsg::Spawn`) is fixed-shape per principle 1: `command`, `args`, `cwd`, `env`, `dims`, `tags`, `env_base`;
empty `command` falls back to the daemon's default factory (`$SHELL`). `dims` is optional and carries the requested
initial geometry: an **absent** `dims` means the daemon default (24 × 80) and pixel `0` means unknown, while any
rows/cols axis outside the REQ-605a bounds, `0` among them, refuses the whole create with
`CreateFailure::GeometryOutOfRange`, checked over the full `uint32` domain before the daemon mints an id or spawns
anything. What admission accepts it keeps: the resulting `(rows, cols, pixel_w, pixel_h)` is the child's first winsize
and the geometry every later surface reports, `SessionInfo::dims` included.

Each field falls back independently: a `cwd` or an `env` pair sent with an empty `command` applies to that default
shell. A non-empty `cwd` must be absolute: a relative one draws an `AttachFailed` ("SpawnArgs: cwd must be absolute")
rather than resolving against the daemon's own directory. An unusable absolute `cwd` fails the spawn only when `command`
is non-empty; with an empty one the daemon retries in its own cwd
([session-lifecycle.md](../explanation/architecture/session-lifecycle.md) "Creation").

The CLI inherits the v1 `0600` socket trust boundary ([security-model.md](../explanation/security-model.md) "Daemon
IPC"): any process running as the daemon's uid can drive any session. Cross-host CLI rides the SSH stdio carrier (see
"Cross-host carrier: SSH stdio" above): the global `--host user@remote` flag selects, for _every_ headless verb, the
`ssh <host> felis-daemon relay` carrier in place of the local socket, so `felis --host remote sessions list` drives the
remote per-UID daemon end-to-end. The wire is carrier-agnostic (`Connection<R, W>` is generic over its halves), so a
verb's body is identical on either carrier: only the _open_ forks, in one place.

Which forms may autospawn the daemon is decided by the verb, not by the carrier: over the SSH carrier a read or drive
verb dials `felis-daemon relay --no-spawn`, so a cold remote socket fails exactly as a cold local one does. The per-form
table is normative in [cli.md](cli.md) "Auto-spawning"; the argument is in
[control-surfaces.md](../explanation/architecture/control-surfaces.md).

### Non-Rust clients: the stdio bridge

Protobuf binary is the only encoding on this wire, so a client whose language has no maintained protobuf runtime reaches
the daemon through `felis bridge`: protobuf to the daemon, one JSON object per line on stdin and stdout. It is
LSP-shaped (one persistent process per editor client, alive until stdin EOF), because that is the shape editors already
supervise.

Its JSON belongs to the CLI machine-output contract, not to this wire: the request grammar, the object shapes, the
`"v":1` epoch, the per-operation parameters, and the aggregate ceilings one bridge process holds are normative in
[cli.md](cli.md) ("`felis bridge`", "Machine output", "JSON Schema"). That is what the boundary is for: a wire minor
lands without the editor noticing.

What the bridge owes this wire is the correlation contract, mirrored one layer up. A request carries a client-chosen
`id` that every reply, stream item, in-band event and terminal echoes, and a stream ends in exactly one terminal object,
the same one-terminal rule `ConnToClientMsg::End` / `Error` holds on the socket ("Correlation, requests, and streams"
above); subscriber lag is an in-band non-terminal event and the stream continues. One long-lived `Ops` anchor carries
the point requests, concurrent operations against one attached session share an auxiliary link, and each notification
subscription owns an observer link, so requests and streams interleave by id within a link exactly as they do on any
other connection. The bridge never spawns a daemon: a supervised helper must not conjure one as a side effect of being
launched.

The bridge is where a frame body crosses from protobuf to JSON, and the shapes it writes are the CLI's, spelled in
`felis-cli` and published by its own schemas. None of its ops carries a grid: a bridge client reads rows as
`sessions.capture` text. The JSON is not a second wire in either case: [Row codec](row-codec.md) governs the packed
bytes, and nothing but the socket ever carries them.

### Structural session JSON: `felis-json` v1

A consumer that re-exposes or records a whole session as JSON (a WebSocket gateway in front of a browser view, a
`.fcast` recorder and its replay) reads a second, named format rather than the bridge's verb shapes: **`felis-json`**,
owned by `felis-grid::json_v1` behind that crate's `json` feature. `encode` renders one protobuf frame body into it and
`decode` reads one back.

Every frame carries its version in band:

```json
{ "felis_json": 1, "kind": "grid", "msg": { "type": "cycle_end" } }
```

`kind` is one of `grid`, `image`, `conn`, `session`, `input`. No other family is part of the format; the families the
bridge serves belong to the [CLI output contract](cli.md) instead. Two `session` arms are outside it as well:
`InputFence` and `InputAccepted` are a correlated request and its reply, and the format carries no correlation envelope,
so a reply stripped of the `request_id` it answers would name no fence. Either one is refused at conversion rather than
rendered.

`msg` is a dedicated DTO, not the serde rendering of a Rust type: each variant is an object tagged by `type`, a session
id is its full 32-digit lowercase hex string, and every bitmap is an integer the schema bounds to the bits this build
defines, so a reserved bit is refused rather than dropped. A `GridMsg::RowDelta` row carries its cells the same way, as
attribute runs over a grapheme list with their own DTOs for graphemes, colors, flags and OSC 66 sizings; the packed
bytes stay on the socket and belong to [Row codec](row-codec.md).

What a reader refuses, the schema refuses with it, down to three structural checks it cannot state. Every bound the
conversion enforces that a validator can express is published:

- the integer width of every numeric field;
- the printable-ASCII window;
- the 1-based handles;
- the row codec's per-row caps as `maxItems`;
- the function-key range;
- the session-id pattern;
- the OSC 66 ranges, including the rule that a fraction stays below its denominator, which rides one `if`/`then` per
  denominator because JSON Schema compares no two properties.

Three checks a validator cannot reach surface only as a decode refusal:

- the attribute runs covering exactly as many columns as the row has graphemes;
- a sized cell naming a column the row has;
- a timestamp landing inside the reading platform's own clock range.

A `u64` bound carries `x-bound-exceeds-f64-precision`, because a validator comparing JSON numbers as `f64` enforces that
bound rounded. The published schema is `crates/felis-grid/schemas/felis-json-v1.schema.json`, regenerated by
`just schema`; the golden frames under `crates/felis-grid/tests/golden/json-v1/` are regenerated by `just golden`.

**Compatibility within a version.** Inside `felis_json: 1` the only change a writer may make is adding an optional
object field. A reader ignores every property the schema does not name, and no object the schema publishes is closed.
Everything else is `felis_json: 2`: a new `kind`, a new variant, a new enum value, a rename, a retype, or a field that
becomes required.

A reader checks `felis_json` before it looks at `msg`, so a version it does not know is one refusal at the envelope
rather than a mismatch discovered inside a DTO. A writer emits the version its consumer asked for, a recording is
written at the newest version the writer knows, and a reader serves both versions for a deprecation window. This is
stricter than the additive-minor rule for the wire (REQ-104b), because a `.fcast` file on disk and a deployed browser
view have no effective-minor handshake to gate on; [ipc.md](../explanation/architecture/ipc.md) "One encoding on the
socket, JSON at a bridge" carries the argument.

## Versioning

Three mechanisms govern compatibility. For schema evolution design rationale, see
[ipc.md](../explanation/architecture/ipc.md).

**Protocol major** names the schema. It is exchanged in the frozen preface ("Version preface" above) before any frame
exists, and a daemon that does not serve the client's major refuses there and closes. A major moves only for a semantic
break, and a bump ships side-by-side decoders in the daemon for a deprecation window rather than a flag day. Until the
compatibility freeze the dev wire carries no promise at all, so pre-freeze breaks change the bytes under major `1`
unbumped.

A pre-freeze break is acknowledged in `crates/felis-protocol/proto/BREAKING.md`, by a `base: <sha>` line naming the
revision the break was written against; the _proto-compat_ CI job accepts a `buf breaking` failure only against that
base, or a base that grew wire-compatibly past it ([testing.md](testing.md#wire-compatibility-gates) has the exact
matching rule and the base each run compares with).

The freeze is the first final release: `tools/proto/compat.py` run on a tag asks the forge for the newest published
final release and compares the schema with the `felis.proto` at that release's tag. It does not consult the
acknowledgment file for cover: a `base:` line buys nothing on a tag, and a break there requires a `PROTOCOL_MAJOR` bump.
While no final release is published, a tag run has no baseline to answer to and passes.

**Protocol minor** names an additive revision of that schema: new fields, new oneof variants, new frame kinds, new
row-codec versions, each with defined old-peer behavior. The effective minor, `min(client, daemon)`, is a **send-side
contract**: a peer never sends what the effective minor does not define. A minor bump never requires a coordinated
restart, which is the property the daemon exists to protect: the sessions it holds outlive every client rebuild.

The writer takes that minor at construction and has no default, so no send path can inherit this build's own ceiling by
omission: a carrier whose preface has not run (`FrameWriter::baseline`) authorizes the base schema and nothing else.
Only a harness that is both peers, and so negotiates nothing, builds a writer at this build's `PROTOCOL_MINOR`, through
a constructor no production dependency compiles.

Two consequences follow from the send-side rule, and they are what make the rest of this page's "corruption" language
exact:

- Unknown frame kinds and unknown oneof variants stay **fatal** ("Corruption" above). Between honest peers they cannot
  occur, so receiving one is corruption, not forward compatibility to tolerate. Protobuf's unknown-_field_ tolerance is
  what carries additive growth; it says nothing about kinds and variants, so the minor says it instead.
- Changing what an existing field, variant, or enum value _means_ is never a minor, whatever the bytes look like. It is
  a feature flag if opt-in works, and a major if it does not.

**Feature flags** are explicit opt-ins for behavior changes and heavy streams a peer may decline, never for
decodability: that question is already answered by the time a `Hello` is read. One optional optimization exists today,
`Hello.pull_paced`: when true, the client's `InputMsg::NextGridFrame` pulls pace grid emission; see
[rendering/pipeline.md "Demand-driven emission"](../explanation/rendering/pipeline.md#demand-driven-emission). When
false, the daemon eager-pushes. Everything else the daemon ships unconditionally, because `GridMsg::Scrolled` directives
and mid-stream `GridMsg::Size` announcements are cheap for every client to honor.

_Rendering_ ability is not negotiated at all: a client that cannot display a payload drops it client-side. Color depth,
sized text, and cell attributes are cheap structured state, so the daemon always ships them at full fidelity and a
limited client down-maps (true-color → 256, sized → flat) or discards locally. A graphics-incapable client still
receives the cheap `Placement` record and renders its own placeholder. Suppression would only ever apply to a payload
that is _heavy or ongoing_ over a cross-host link, and even then it would be another opt-in flag, never a correctness
gate. The argument, and what such a flag waits on, is in
[the IPC design explanation](../explanation/architecture/ipc.md#open-extensibility-considerations).

What a connection may _reach for_ is a separate axis from all three: that is `Hello.mode`'s job ("Connection modes"
above).

### The minor ledger

Every minor's additions are recorded here, one row per addition, with what an older peer does when it meets one. The
table is the review gate for "is this really additive": an addition that cannot be written as a row is a feature flag or
a major, not a minor. `felis-protocol`'s `MINOR_LEDGER` mirrors the first column, and a test holds the two together: a
`PROTOCOL_MINOR` bump with no row here fails the build.

The table is also machine-checked against the metadata the send gate reads ("Versioning" above), so a row names its
additions by the identifiers `felis.proto` spells them with: an arm as `Family::Variant`, a closed-enum value as
`Enum::VALUE`, a field as `Message.field`, and a row-codec version by its number. The check runs both ways: an addition
whose identifier is missing from its row fails the build with the name it expected to find, and an identifier a row
names that no arm, field or enum value declares at that minor fails it too. Only those three shapes read as an addition,
so a row is free to name a bare type, a variant of an inner enum, or a command line beside them.

| Minor | Addition                                                                                                                                                                                                                   | Older peer                                        |
| ----- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------- |
| 0     | The base schema of protocol major 1: the ten frame kinds, every family in "Message families" above, every arm, field, closed-enum value and connection mode they carry, the correlation envelope, and row-codec version 1. | — (no peer speaks an earlier minor of this major) |

A schema change that is not additive carries no row: no "older peer" cell could state a true degradation for it. Before
the compatibility freeze such a change rides the pre-release rule instead, acknowledged for the wire gate in
`crates/felis-protocol/proto/BREAKING.md`; past the freeze it is a major. The same holds for a tightened admission check
under "Semantic limits", which adds and moves nothing and so is neither a row nor, under REQ-104b, a major. Both rules
and their revisit trigger are argued in
[the IPC design explanation](../explanation/architecture/ipc.md#the-ledger-is-the-review-gate).

### Handshake

After the preface accepts, the client sends `ConnToDaemonMsg::Hello` and the daemon answers `ConnToClientMsg::Welcome`:
the application half of the handshake, in the schema the preface already selected. Neither frame negotiates a version: a
`Hello` that reached the daemon at all was decodable under the agreed major, and a `Welcome` says only that the daemon
accepted this connection's mode. The one exception is the mode itself: a daemon that meets a mode it does not define
answers `Refused { UnknownMode }` and closes, and every other undecodable `Hello` is corruption, closed unanswered
("Corruption" above). The split runs on the mode number: any nonzero value the daemon does not define is the newer
peer's, so it earns the typed refusal, while the `CONNECTION_MODE_UNSPECIFIED` sentinel `0`, which no conforming sender
writes, is malformed and takes the corruption path.

A `Welcome` puts the connection in the `Setup` phase ("Connection phases" above), from which it may attach, create, run
an `Ops` verb, or subscribe, and nothing else. A `ConnToClientMsg::Refused` arrives instead when the peer reaches for a
family its mode does not admit before it has attached; after that, a denial is the request's own typed error ("Ops (kind
= 5)" above).

Skew a client can meet, and where each is reported:

| Condition                                                               | Where it surfaces                                                                                                    |
| ----------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------- |
| The peer is not felis                                                   | wrong preface magic; the connection closes with no reply                                                             |
| The daemon does not serve the client's major                            | preface status `1`, carrying the majors it does serve                                                                |
| The daemon accepts (status `0`) naming a major the client never offered | `ConnectError::AcceptedUnofferedMajor`, closed before any frame; `felis version` and `felis doctor` name both majors |
| The daemon refuses for a newer reason                                   | preface status the client cannot name; treated as a refusal                                                          |
| The peer's mode does not admit a family, before it attaches             | `ConnToClientMsg::Refused { Role }`, then close                                                                      |
| The daemon is serving every connection it admits                        | `ConnToClientMsg::Refused { AtCapacity }`, then close; retryable                                                     |
| The peer names a mode the daemon does not define                        | `ConnToClientMsg::Refused { UnknownMode }`, then close; not retryable                                                |
| The peer stays silent through a pre-attach phase                        | no frame: the daemon closes at that phase's deadline                                                                 |

A major refusal is not retryable: both halves must be rebuilt, and a client that treats it as a transient EOF would burn
its whole backoff budget on a permanent condition.

**Each pre-attach phase is time-bounded.** A peer that opens a connection and then says nothing holds a task, a file
descriptor, and a read buffer, so the daemon cuts it at the deadline for whichever phase it stalled in:

| Phase           | Deadline                    | What must arrive                                                    |
| --------------- | --------------------------- | ------------------------------------------------------------------- |
| Preface         | 2 s                         | the client's 8-byte preface (and its carrier block, over the relay) |
| `Hello`         | 5 s                         | the first frame after the preface accept                            |
| First operation | 30 s (`Window`, `Observer`) | the `Attach` / `Create` or `Subscribe`, and each frame before it    |
| First operation | not timed (`Ops`)           | —                                                                   |

The deadline runs until the attach decision, not until the first frame of any kind: a `Window` may read the roster with
`Ops::List` before it names a session (the shipped GUI does), and each such frame restarts the 30 s rather than retiring
it, so a peer that lists and then falls silent is still cut. Nothing after the attach decision is timed, and an `Ops`
connection is not timed at all past its `Welcome`: `felis bridge` opens its anchor at startup so that "the bridge is
running" means "the daemon is reachable", then writes nothing until its editor asks for something, and an observer idles
for hours between notifications. A `Window` and an `Observer` owe a subject, so silence before they name one is a stall.

**Connections are admitted, not merely accepted** (REQ-916). The daemon serves at most the connection cap
(`MAX_CONNECTIONS`, 1024) at once, against a permit taken before the connection's task exists and released whenever that
task ends. A dial past the cap is answered `Refused { AtCapacity }` rather than queued or closed, so a caller can tell a
full daemon from a dead one. Answering costs a task of its own, so the refusal path is admitted against its own small
ceiling; past that the socket is dropped unanswered. Every step of a refusal is deadline-bounded, the write of the
refusal included: a peer that finishes the handshake and then stops reading would otherwise hold one of those few slots
for as long as it liked, and enough such peers would silence the refusal path for everyone.

## Backpressure

A connection is **one ordered frame stream per direction**. There are no per-kind channels, no per-kind priorities, and
no reordering: what the daemon queues for a subscriber is what goes on the wire, in that order. That is what lets a
`Region` / `Search` reply be ordered after the `RehydrateEnd` a verb drains to, a `RowDelta` after the snapshot it
patches, and an `ImageMsg::Complete` after its chunks.

The two directions are **serviced independently**. The daemon runs a reading loop and a writing loop per connection, so
a write parked on a peer that stopped reading delays only that peer's output: inbound frames keep being decoded and
routed to the session task throughout. This is the whole of the "a backed-up image stream must not stall input"
guarantee (REQ-103, REQ-1011). The one thing that does pause a reading loop is that connection's own input budget
("Client → daemon: the input budget" below), which pauses on the child it is typing into rather than another peer's
output.

**Scheduling happens only at frame boundaries.** A frame that has begun is written to completion before anything else on
that connection moves, so the largest frame the daemon can emit bounds the worst-case delay input sits behind bulk
output. `MAX_IMAGE_CHUNK_PAYLOAD` (256 KiB) is what sets that bound, which is why it is far below the 64 MiB
`DEFAULT_MAX_BODY` a frame may legally carry.

What the daemon does _not_ do is apply the socket's backpressure to the PTY. A subscriber's outbox is an unbounded queue
guarded by a bytes-in-flight gauge; a subscriber whose unwritten backlog crosses `SUBSCRIBER_BUFFER_CAP` (512 MiB) is
evicted; blocking on the slowest subscriber would freeze the shell for other mirrors (see
[session-lifecycle.md](../explanation/architecture/session-lifecycle.md)). There is no write deadline: a stalled peer is
cut on volume, never on time. Time bounds the _pre-attach_ phases instead ("Handshake" above), as far as the mode
allows: until `Hello` the daemon cannot tell a silent peer from a hung one, and once a mode that idles by design has
been named, the connection cap is the only bound left. Past the attach the daemon knows what the connection is for and
bounds it by what it queues.

The gauge is the bound, and the queue is unbounded so that the _cut_ is a decision the daemon makes rather than a stall
it suffers: a bounded channel would apply the slow peer's backpressure to whoever tried to enqueue, which on this side
is the session task serving everyone. What a producer bounds instead is how much it enqueues per turn ("Search (kind =
9)" above): a burst limit, not a queue limit. `felis daemon status`'s `subscriber_queue_bytes` row reports the deepest
live backlog against this cap, beside the daemon-wide sum of every outbox, which is the only place the gauge is
observable ([cli.md](cli.md) "Daemon status").

Grid updates do coalesce, but per cycle rather than per channel: one `RowDelta` per diff cycle carries every dirty row
(split across consecutive frames only when the batch would approach the frame ceiling), and a row dirtied twice inside a
cycle ships once.

### Client → daemon: the input budget

The other direction is bounded rather than cut. Each session admits `PTY_INPUT_BUDGET` (16 MiB) of client input that the
child has not yet taken. A connection acquires its share **before** the bytes enter the session's command channel, and
the reservation is released only once the PTY writer's `write_all` returns, so it covers the whole path from the socket
to the child's stdin rather than the enqueue alone.

| Message                 | Reserves                                                                                                                               |
| ----------------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| `Input::KeyBytes`       | the payload length                                                                                                                     |
| `Input::Paste`          | the payload length plus `PASTE_BRACKET_OVERHEAD` (12 bytes: `ESC [ 200~` and `ESC [ 201~`), whether or not the session has `?2004` set |
| `Input::Mouse`          | `MAX_MOUSE_REPORT_BYTES` (32), whatever the event encodes to — or nothing, if the active protocol filters the event out                |
| `Input::Key`            | `MAX_KEY_REPORT_BYTES` (280), whatever the active keyboard mode encodes to — or nothing, if the key has no encoding under it           |
| every other `Input` arm | nothing: none of them carries bytes of its own for the child                                                                           |

A paste reserves its bracketing unconditionally because the mode is read when the write happens, and the child can flip
it between admission and that write; the conservative count is the one that is always sufficient. A mouse report and a
key report reserve the widest form their encoders produce for the same reason: the protocol, encoding, and keyboard
modes are all read at write time. The key report's bound follows from the two per-key caps
([key-encoding.md](protocols/key-encoding.md) "Report size").

`MAX_PASTE_BYTES` (16 MiB − 64) is pinned below the budget minus that overhead: a reservation larger than the budget
could never be granted, so a paste past the cap is refused and the connection closed rather than parked forever. Both
senders refuse an over-limit paste locally: `felis sessions send` exits `1` (`invalid_request`) and a window logs and
drops it. The connection-closing path is reached only by a peer that ignored the published limit.

Only the reserved bytes ride the budget. The session actor's own writes carry no reservation: a `DA` answer to the
child, and the reports a client-side change owes it, which are a focus report under DECSET 1004, a color-scheme report
under DECSET 2031, and an in-band resize report under DECSET 2048. They are bounded separately, by the PTY writer's 1
MiB gauge of unreserved pending bytes, and dropped rather than queued past it. The two gauges are separate so that an
admitted 16 MiB paste cannot silence the replies a child is waiting on while it drains.

While a session's budget is exhausted, the connection feeding it stops reading its socket, except for noticing that the
peer hung up: a connection parked on a budget a wedged child will never release would otherwise hold its admission
permit and its subscription until the session ended. A peer that left more than a read-ahead's worth of pipelined bytes
behind it is noticed only once the child drains: EOF sits behind those bytes, and reading past them is the backpressure
itself.

The budget bounds what a session holds, not what the daemon does: a pump parks holding the one frame it had already
read, so the daemon's worst case is that frame per connection, bounded by the connection cap above it and the frame cap
below it rather than by this budget. Reserving before the body is read would need the reservation to be made against an
announced length the peer has not yet justified.

A hangup noticed there ends the connection _and_ the message it was admitting, so a peer that needs its input delivered
has to stay until the daemon has taken it: `felis sessions send` puts a `Session::InputFence` on the same connection,
which the daemon answers only after the input frames ahead of it cleared admission. The backpressure reaches the peer's
socket buffer and, in a window, its own send queue (never the session task, which keeps serving every other subscriber;
see
[session-lifecycle.md "Slow children: backpressure, not eviction"](../explanation/architecture/session-lifecycle.md)).
`felis daemon status`'s `pty_input_bytes` row reports the daemon-wide sum of admitted-but-unwritten input against the
per-session budget ([cli.md](cli.md) "Daemon status").

### A window's send queue

A window queues at most `CLIENT_OUTGOING_CAP` of encoded frames for its carrier: the largest single message the daemon
would accept (`MAX_PASTE_BYTES` plus its bracketing) plus 4 MiB of backlog on top, 20 MiB in all. The cap has to clear
one whole message, or the queue would refuse a lone 16 MiB paste against an empty queue and declare a healthy carrier
dead; it has to clear it with room to spare, or the next keystroke typed while that paste drains would. Two rules apply,
by message kind and never by payload:

- **Ordered** — `Input::KeyBytes`, `Input::Paste`, `Input::JumpPrompt`, `Input::NextGridFrame`, `Input::Key`, and every
  non-`Input` family: appended, never replaced, never reordered.
- **Replaceable** — `Input::Resize`, `Input::FocusChange`, `Input::ColorScheme`, `Input::Viewport`, and a buttonless
  `Input::Mouse` motion: a newer one overwrites the queued frame of the same kind **in its slot**, so a `Resize` queued
  ahead of a keystroke still leaves ahead of that keystroke after a later `Resize` replaces it. A drag carries a button
  and is ordered: it is what a selection is made of.

Two ordered kinds close queued slots behind them: every slot whose state the daemon reads when it handles that frame.

- `Input::JumpPrompt` closes the `Input::Viewport` slot: the daemon computes the jump target from the viewport the
  subscriber is at, so a scroll queued after a jump must not be hoisted in front of it by replacement.
- An ordered `Input::Mouse` (a press, release, drag sample, or wheel tick) closes two. It closes the motion slot,
  because it is a point in the same positional stream: a later motion hoisted in front of a press would report the
  pointer somewhere it had not yet been when the button went down, and leave the older position as the last one the
  program saw. It closes the `Input::Resize` slot as well, because its coordinates are cells of the grid the window had
  when the user clicked; a later resize taking an earlier one's slot would reflow the child first, and the click would
  land on whatever then occupies that cell.

Every other replaceable kind commutes with the ordered frames around it, which is what lets one slot serve the whole
queue.

Past the cap the window declares the carrier lost and takes the path a hangup takes, the re-dial in
[session-lifecycle.md "Transport loss"](../explanation/architecture/session-lifecycle.md#transport-loss). This mirrors
the daemon's "cut on volume, never on time": a full paste plus megabytes of input a socket has not accepted describes a
transport that is gone, not one that is slow.

## Authentication

- Local (Unix socket): file mode `0600` on the socket, inside a `0700` directory the daemon owns; only the daemon's user
  can connect. The UID is verified by both sides before any application byte: the daemon on accept, the dialer after
  connect (`SO_PEERCRED` on Linux, `getpeereid` on macOS and the BSDs). No further auth.
- Local (Windows named pipe): per-instance DACL plus an active client-SID check; see
  [security-model.md](../explanation/security-model.md) "Daemon IPC".
- Cross-host (SSH stdio): the SSH layer is the auth boundary. felis adds no second password / token layer (see
  "Cross-host carrier: SSH stdio" above).
