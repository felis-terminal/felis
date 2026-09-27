---
title: IPC design
sidebar:
  order: 4
---

The wire itself (layers, framing, message families with kind numbers, payload packing, handshake, and version rules) is
specified in [the IPC wire reference](../../reference/ipc.md). This page is that spec's design record: the goals the
protocol serves, the encodings and carriers that were rejected, what is deliberately kept out of the wire, and the
extensibility questions still open.

## Goals

1. **Self-describing.** A client of version V_c connecting to a daemon of version V_d either interoperates or fails
   loudly with a clear error. No silent feature loss.
2. **Streamable.** The daemon must be able to ship a 50 MiB rehydration burst while a 200-byte input event on the same
   connection still reaches the PTY promptly, even when the burst is stalled against a peer that stopped reading.
3. **Cheap when idle.** An attached but inactive session must not cost CPU on either side.
4. **Forward-compatible.** A newer producer talking to an older consumer either works or fails, never silently corrupts.

## How the messages are split into families

A single pooled enum would carry the handshake, this connection's session lifecycle, the CLI one-shots, the region
export, the notification observer, and the daemon→client pushes in one family. Every daemon dispatch site would then
match that whole enum and reject the variants that make no sense in its phase, so most match arms would be "nonsense
here" guards. A variant's role already maps to a connection phase (a handshake message, a request on this connection's
own session, a one-shot on another named session, a mid-stream region reply, an observer event, a push), so the taxonomy
promotes role to the frame `kind` and splits the messages into `Conn` / `Session` / `Ops` / `Region` / `Notify` /
`Push`. Kind-level routing replaces the guard arms: each phase accepts only the families that belong to it, and an
out-of-phase frame is refused by `kind` rather than by a match arm buried in a handler.

Two other taxonomies are rejected:

- **A direction split** (client→daemon vs daemon→client kinds). It reads cleanly but separates a request from its reply
  (`Attach` and `Attached` would land in different kinds), scattering each round-trip across two families, so a phase
  could not admit or refuse a whole conversation by `kind`.
- **Keep the mono-enum and rely on variant ordering.** It leaves the "nonsense here" dispatch arms in place, the exact
  cost the split removes.

### Direction splits one level down, inside a family

Direction is split one level down instead, inside a family: a family whose arms travel both ways is two wrappers under
its one kind ([the reference gives their names and numbering](../../reference/ipc.md#message-families)). A round-trip
still shares a kind, and each receiver decodes only the wrapper that can reach it, so a wrong-direction arm has no
variant to land in.

Rejected: **one wrapper per family carrying both directions**. The driver would still refuse a wrong-direction arm
before any handler sees it, but every receive-side `match` would have to spell the other direction's arms as dead
guards: the same "nonsense here" arms, one level down, which the type cannot rule out.

What the split gives up is a check the single oneof made for free: a body carrying an arm of each wrapper decodes,
through the receiving wrapper, as that wrapper's arm with the other skipped as an unknown field, so the driver has to
look for the other wrapper's field numbers itself. It steps over field keys rather than decoding the body a second time
as the other wrapper, so an honest frame pays one pass that builds nothing.

_Revisit if_ an arm ever has to travel both ways, which the split would force into a copy per wrapper.

### Kind or arm?

The split promotes _role_ to the frame kind, and the question that follows is where growth lands: a new kind, or a new
`oneof` arm of an existing one. What settles it is which surface owns the message
([the reference states the criterion](../../reference/ipc.md#the-arm-table)): the state it addresses and the peers that
may address it, not the correlation lifetime the message happens to use. One surface may own several openers, so
bringing an opener and a lifetime of its own does not by itself make a message a kind.

So `Search` is a kind: matches over a session's history are a surface no other family speaks about, opened, streamed,
and refused as a unit. `Region::Rows` is an arm even though it opens a row stream of its own, because it reads the very
regions `Region::Request` names: it is a second read shape on a surface that already exists, and the mode and phase rows
of the region family already cover it. `Ops::Status` is an arm, another query about the daemon and the sessions it
holds; `Push::RetargetHost` is an arm of the pushes the daemon sends a subscriber; `Ops::Spawn` is an arm, and so is a
future point verb. A future stream a `Window` must never see is a kind, not because its opener is new but because a
`Window` has to be refused it before its body decodes, and the kind fold is the only gate that early.

#### The minor column is declared, not checked

A new kind and a new arm are both additive changes: the effective minor both peers agreed on decides whether a sender
may use either ("Schema evolution" below), so a kind or arm arriving that this build does not define is corruption
rather than growth.

The one column of the arm table a receiver does not enforce is the minor, which is declared rather than checked: the
effective minor is a send-side contract, so an arm above it is the _sender's_ violation, and a receiver that also
policed it would need the negotiation's outcome threaded into the driver to buy nothing the ledger's old-peer behavior
does not already give.

_Revisit if_ a frame can ever reach a driver under a minor its sender did not agree to; a mixed-version relay chain
would do it.

#### The arm table lives in the schema

Where the table lives is a schema question rather than a Rust one. Each arm carries its row as a `(felis.v1.arm)` field
option, so a peer that already runs a `protoc-gen-*` over `felis.proto` reads direction, correlation, modes, phases and
introducing minor out of the same descriptor, with nothing left to transcribe from Rust or from a comment
([the reference gives the form](../../reference/ipc.md#the-arm-table)).

Two other homes are rejected:

- **A routing manifest** beside the schema, which is the shape [the architecture overview](overview.md) turns down for
  the wire constants for the same reason: a second committed file to keep in step with the first, holding what the
  descriptor already carries.
- **Generating `ArmMeta` from the schema**, which would leave one statement of the row and so nothing for the driver's
  table to disagree with; the sync test exists because two independent statements can be compared.

What that choice costs is the gate: `buf breaking` compares field numbers, names and types and never looks at options,
so `the_schema_declares_the_same_arm_table` is the one check a wrong routing row fails.

#### Kind admission is a coarse fold of the arm table

Kind-level admission survives as the _fold_ of that table, never as an independent policy, and the fold is deliberately
coarse: `Ops` is admitted to both attach-capable modes because `Ops::List` is a query a window's session picker runs,
and the arm's own row settles the rest once the body decodes.

Two ways of making kind admission the whole policy are rejected:

- **Splitting families until whole-kind admission is literally true.** Four of the families contradict it on their own:
  `Ops` queries versus `Ops` mutations, `Region`'s point reply versus its row stream, `Push::Evicted` versus the
  window-management pushes, and `Conn`'s handshake arms versus its stream-lifecycle ones. Making it true takes a kind
  per verb (`OpsList`, `OpsDestroy`, …), which spends the `kind` word's numbering and a `FrameKind` row on every future
  verb, to buy a property (one `match` on the header word) that the decode has to perform anyway to read the body. Each
  verb would also add a family to every generator, to every non-Rust peer's dispatch, and to the docs.
- **Leaving the arm checks as ad hoc guards in the handlers.** A guard inside a family handler is a second mode policy
  the driver cannot see, so the rule the schema states and the rule the daemon applies drift apart with nothing standing
  between them to catch it.

#### Uniform families skip the arm walk, never the decode

The fold is coarse except where a family's arms all declare the same row: there it loses nothing, and the driver spends
that. `Grid` and `Image` carry every row delta, cluster and image chunk an attached connection receives, the
highest-volume traffic in the system, and the readers that drain a family they have no use for still owe it the table. A
drained frame of a uniform, envelope-less family is therefore judged on the kind columns rather than walked through the
arm's row. The shortcut is not a second policy: `MessageKind::arms_are_uniform` is asserted against the arm table, so a
family that grows a disagreeing arm loses the shortcut rather than quietly skipping a column.

What the shortcut does not skip is the decode. Dropping it there would spare the busiest stream on the link its
deserialization, at the price of making corruption a property of the reader: the same malformed `Grid` body would end a
window client's connection and pass unnoticed on a `felis bridge` link that drains the grid burst, which is exactly the
divergence a cross-language client would discover as a felis bug rather than its own. So the drain path decodes the body
and throws the message away, and the cost lands where it is affordable, on frames that are being dropped rather than
rendered.

The checks that stop short of the decode do so for reasons other than cost: `felis-transport` depends on
`felis-protocol` alone, so a check that needs the receiver's state is out of its reach, and a limit meant to be answered
rather than to be fatal belongs to its handler, since the only verdict the driver has is the connection's life. Both
stay with their consumers, the reference's semantic tier ([ipc.md](../../reference/ipc.md#corruption)).

_Revisit if_ a uniform family ever needs a per-arm decision, such as a `since_minor` the receiver checks, since that is
the point the fold stops being exact.

#### Search and region are kinds of their own

Search and region instance the rule at the kind level. Each owns a surface of its own (`Search`, `Region`), request and
reply wrappers under one kind, so a phase can refuse the whole surface. Neither owns a terminator: their streams end in
the shared `ConnToClientMsg::End` / `Error`, so a future streamed reply inherits the terminal rule instead of minting a
bespoke one ("Correlation rides in the body" below).

Two other placements are rejected:

- **Folding them into `Ops`.** An `Ops` one-shot addresses a _named other_ session with no attach state, but a region
  reads the attached session's live grid at the requesting subscriber's own viewport, and search hits reference that
  session's rows; both conversations are attach-scoped by data dependency, so they stay on the attached connection under
  their own kinds.
- **A separate capture kind beside the region family.** `felis sessions capture` reads the very regions the keymap
  `pipe` chord names, so the row-stream read is a second reply shape on `Region` (`Rows` / `Row` / `RowsDone`), not a
  parallel family; the daemon encodes each row to text itself, which keeps every cell codec out of the CLI and lets one
  stream carry plain and SGR forms side by side.

### Creating a session

Two arms create a session, and the split is about who the new session belongs to. `SessionToDaemonMsg::Create` creates
_and_ attaches the requesting connection; `OpsToDaemonMsg::Spawn` creates one nobody attaches to.

`Create` is one message rather than a create followed by an attach because the two-step form has a window in which the
session exists, is registered, is listable, and has no attacher. Anything that goes wrong in that window (the client
dies, the attach is refused) leaves a live shell whose id nobody was ever told. It cannot be attached (no one knows its
id), cannot be found by the user who asked for it, and still holds a session slot. Folding the attach into the create
closes it: the id is delivered only in the attached state, so a `Create` whose subscribe fails has left nothing behind,
and the daemon rolls the registration back (pool entry removed, session shut down, child reaped) before it writes the
refusal.

The fold alone would only shrink the window, not close it: the pool row is written by the spawn, before the subscribe
that can still fail. So the row is _held_ until the subscribe has landed: registered, counted against the session cap,
and answering no lookup. Nothing else can list it, resolve a prefix to it, attach to it or destroy it in the meantime,
which is what makes the rollback safe, the session it tears down being nobody else's.

The publication comes just before the ack is written, not after it. Published after, the ack would reach a peer that can
name the id on another connection (a window re-attaching after transport loss, or a script handed the id) before the row
answers lookups, and that peer would be refused a session it was just given. Published before, the ack is past the point
of rollback: an ack that fails to write leaves a detached session the roster shows, exactly as a lost `Spawned` reply
does, and the same session a client that dies just after reading its ack leaves.

The headless spawn rides `Ops` for attribution. The attach and create arms of `Session` are uncorrelated by design: a
connection attaches once, so its ack needs no request id. That is exactly wrong for a spawn, which is a pool operation a
scripted peer may want several of in flight, and an uncorrelated family can serialize them only behind a single
positional slot that refuses the second. `Ops` is the family whose every arm carries a `request_id`, so `Spawn` inherits
attribution for free, and its refusals become a typed `SpawnOutcome` arm rather than a connection error, the same shape
`ResolvedId` gives the prefix verbs.

A lost `Spawned` reply leaves a _detached_ session the roster shows, which is what a headless spawn wanted anyway; that
is the asymmetry that lets `Spawn` answer without attaching while `Create` cannot.

_Revisit if_ a client ever needs create-without-attach on a window connection: a window that spawns a sibling session it
does not display, say. Today that caller opens an `Ops`-mode connection; the trigger to reconsider is a caller for which
the second connection is the cost, not the round trip.

### The two environment fields

A create carries the child's environment in two fields
([the reference states their types and effects](../../reference/ipc.md#session-kind--4)), and the split is the two
origins rather than two spellings of one thing.

#### Bytes for the captured base, strings for argv and cwd

A value a user typed reached felis through UTF-8 arguments already; a captured one is whatever the host holds, so
`env_base` is `bytes`. A `string` field could not carry it back out unchanged: the lossy replacement a UTF-8 conversion
leaves turns a path into a path that does not exist, and a non-UTF-8 `SSH_AUTH_SOCK` is still the path the agent listens
on.

Argv and cwd stay `string` although `execve` accepts bytes for them too, and the bridge is the reason rather than the
exec layer: `sessions.spawn` carries `cmd` and `cwd` as JSON strings, and JSON cannot express a byte sequence that is
not UTF-8, so a `bytes` wire would hand the CLI a capability the bridge has no way to reach.

The exposure is narrow as well. A Unix-native argv entry or path is arbitrary bytes, so it can fail to be UTF-8, but a
value that reached felis because the user typed it came through UTF-8 arguments already, and on Windows a valid UTF-16
argv or path always converts. What a client owes such a value is a refusal, not a lossy conversion, because a path
spelled with U+FFFD dials a socket nobody listens on or opens a window in a directory that does not exist (REQ-105b).

_Revisit if_ a caller appears whose argv or cwd is genuinely not UTF-8: a parallel `bytes` field beside the `string` one
is an additive minor, where retyping the existing field would be a major.

#### The snapshot replaces the base, over a local socket only

The snapshot replaces the base rather than overlaying it because the daemon's own environment is what it exists to
displace: a warm daemon's environment descends from whichever login started it. For the same reason the dialing process
fills the field only over a local socket. Over the SSH relay the dialer's environment names the wrong host, and that
rule is what makes a retarget create correct: the _executing_ window dials the target daemon, so its environment
describes the host the child runs on, and the initiating CLI's descriptor carries only explicit `env` intent.

That "only over a local socket" rule stays a **send-side invariant**, deliberately: no decode step can enforce it.
Rejected: having the daemon refuse an `env_base` that did not arrive over a local carrier. A relay whose snapshot is
over the caps legally omits `FRLY` and splices a bare `FLIS` stream, so an SSH-carried create is byte-identical to a
local one, and the protobuf decoder has no carrier context to consult even where the relay does announce itself. A check
that cannot see the thing it checks would refuse correct creates and admit the case it exists to catch, so the rule is
stated where it can be held, in the dialing process, and the daemon takes what arrives.

#### Scrubbing and caps follow who asserted what

Two consequences of who asserted what. A base source is scrubbed silently, since inherited noise is not the caller's
assertion, while an explicit `env` pair naming a reserved key is refused. And the `env_base` caps are checked in the
spawn path rather than at decode: a decode-side check answers an over-cap base by ending the connection, where the spawn
path refuses that one create and leaves every other session on the connection alone. The dialer omits an over-cap
snapshot rather than sending it, because sending it would turn a large environment into a window that does not open,
where omitting it costs only the freshness.

## Connection modes, not per-feature bits

A mode is **routing and output shaping, never authorization**. The peer states its own mode, so a mode grants nothing a
peer could not have asked for by stating a different one; what it says is which surfaces this connection wants wired up,
and the daemon shapes its fan-out to match. Reading the modes as a permission ladder would be a category error with a
security consequence, since it would put a peer's own claim in the position of a grant.

Real authorization needs a credential the peer cannot mint for itself, bound to something outside the connection, and
felis has no such mechanism. The security boundary is the carrier instead: socket permissions and the peer-UID check
locally, SSH across hosts ([security-model.md](../security-model.md) "Daemon IPC"). Every peer that gets as far as a
`Hello` is already the same UID as the daemon, which is why the daemon can afford to take the mode at its word.

### `Hello` carries a mode and one pacing bool

`Hello` carries two fields, and the split between them follows from what each answers. "Does this peer pace its own grid
stream" is a genuine per-connection question with a per-connection answer: the SSH carrier eager-pushes because a
round-trip per frame is the wrong trade on a slow link. "What is this connection for" is one question with one answer,
fixed the moment the client decides what it is opening the connection _for_. A window opens to stream a session;
`felis sessions kill` opens to destroy one; `felis notifications subscribe` opens to watch the fan-out. Every surface
each needs follows from that, so a single field says it once.

Rejected: a **capability bitmap** for the other optimizations the daemon implements (scroll directives, mid-stream
dimension announcements). Every real client honors every such bit (each is cheap row-stream state any grid consumer can
apply), so the bits gate nothing while the daemon still branches on them per subscriber. An optimization earns a
handshake flag only when a real peer declines it; pull pacing is the one that qualifies.

`pull_paced` is therefore the only bool `Hello` carries, and the rule for the next optimization that qualifies is fixed
now: **it earns a mode, not a second bool**, because two bools beside an enum is the bitmap this section already
rejected, arriving one field at a time. Rejected: folding pacing into `ConnectionMode` today to be rid of the lone flag.
The two fields answer different questions: what a connection is for, and how it consumes the grid stream. Crossing them
multiplies the modes by the pacing choices and makes a peer name a combination rather than a purpose.

### Least privilege is a mode, not comprehension bits

Rejected: **per-feature comprehension bits**, one per gated surface (`SEARCH`, `CLI_OPS`, `REGION`, `NOTIFY`,
`SWITCH_PUSH`, `HOST_SWITCH_PUSH`), each meaning "I can decode the frames this unlocks". Decodability is settled before
any bit could be read: the preface fixes the protocol major and the effective minor, and the effective minor is a
send-side contract, so a peer is never sent a kind or variant it does not define ("Schema evolution" below). The daemon
would still branch on each bit at every gate to re-answer that. What withholding such a bit expresses is not
comprehension but _least privilege_ (the CLI's `capture` connection declining switch pushes is saying "do not move me",
not "I cannot parse `Reattach`"), and a mode says that directly, in one field, without asking each caller to assemble
the right bit set per verb.

Two things the bit scheme could express and the mode deliberately cannot. A peer cannot mix and match surfaces across
modes: a window may not destroy another session, an observer may not attach, and neither is configurable per connection.
That is the point: the combinations the bits allow are not requirements anyone has, and every one the daemon would have
to tolerate is a branch it has to get right. And a mode cannot grow a surface for one caller without granting it to
every peer in that mode; when that pressure arrives, the answer is a new mode, not a bit bolted back on.

### A newer peer's mode is refused by name

Adding a mode is therefore an ordinary additive minor, recorded like every other addition ("Schema evolution" below). A
mode is the one addition with no down-map, since answering a peer with a mode it did not name would wire up a surface it
did not ask for. What a daemon that meets an unreadable mode owes is a typed refusal rather than the bare close an
undecodable body earns: a newer peer's mode is the one handshake failure that is not corruption, so the daemon names it
(`RefusalReason::UnknownMode`) and lets the peer report daemon skew.

Naming the mode costs the daemon a second decode of the `Hello` body, which recovers the raw enum number the validated
codec has already refused. That second read is a diagnostic for a peer that is not honoring the contract, not a
compatibility fallback: a conforming client refuses a mode above the effective minor before it writes `Hello` at all
(`check_mode_minor`), so ordinary skew never reaches it. `serve/tests.rs` pins both halves:
`a_hello_naming_an_unknown_mode_is_refused_by_name` and
`an_unreadable_hello_that_is_not_a_future_mode_closes_unanswered`.

### The roster queries both attach-capable modes share

The queries shared across modes are `OpsToDaemonMsg::List`, `OpsToDaemonMsg::Info` and `OpsToDaemonMsg::Status`,
answered for the two attach-capable modes. The line is mutation, not privilege: a window's session picker and an Ops
verb's roster print both exist to show a _person_ the roster, and `Status` reports numbers about a socket its peer can
already list.

Nothing else needs them: every request that names a session carries a hex id prefix the daemon resolves itself
(`ResolvedId` in the reply, or the `found` / `no_match` / `ambiguous` union an `Info` answers with), so no mode has to
read the roster just to build an id. `SessionToDaemonMsg::Attach` takes a prefix on the same terms, resolved in the same
step that takes the session handle, and keeps an exact-id form for the callers that already hold an id: the reconnect
ladder and a roster-driven switch would otherwise pay a resolution for an id nobody shortened.

That is why the observer admits `Notify` alone: its `--session` filter rides the subscribe (`Subscribe.session_prefix`),
and wiring `List` to a mode that never displays sessions would put the roster on a connection with no use for it.

Client-side resolution (fetch the roster, match locally, send the full id) is rejected: it costs a second round-trip,
races the pool (the matched session can die between List and the act), and forces every non-Rust peer to reimplement the
prefix-match rules the daemon already owns.

`Ops::Info` covers the one caller that owes its reader a `short_id` as well as a row, `sessions info`, so that verb
needs no roster of its own: shortening against the pool is something only the pool's owner can do correctly, and doing
it in the reply that already resolved the prefix costs nothing extra.

## Why protobuf

The wire is a cross-language reuse surface: a consumer that is not the GPU client (a browser gateway, a `.fcast`
recorder, a host-terminal TUI) reads the same frames it does. That makes the body's _schema portability_ the deciding
constraint, and it is the one postcard cannot meet.

Postcard is the plausible alternative. It is byte-stable per released version (a wire break requires a postcard v2.0.0),
it is `no_std`-friendly so the reuse surface pulls in no async runtime or OS symbols, and one
`#[derive(Serialize, Deserialize)]` drives a binary and a JSON mode from the same types
(<https://postcard.jamesmunns.com/>). But its wire is _positional and Rust-shaped_: a struct is its fields in source
order, with no field names or tags on the wire. Two costs follow. A non-Rust consumer either hand-rolls a postcard
varint reader or couples to Rust's serde JSON shape, just to read `Welcome`. And the wire's shape is pinned to the Rust
types themselves, so a field or variant change (including a refactor that never meant to touch the wire) moves the bytes
silently, and a positional decode cannot skip what it does not expect: the mismatch surfaces as a catastrophic mid-frame
decode failure, not at a gate.

Protocol Buffers avoids both. The schema is a language-neutral `.proto` (`crates/felis-protocol/proto/felis.proto`,
package `felis.v1`) that any language generates a reader from with buf or protoc (<https://protobuf.dev/>), so the wire
contract is a file every consumer shares rather than a Rust type only Rust can read. And the shape of a break is
schema-shaped, not build-shaped: version selection happens in the frozen preface ("Handshake bootstrap" below) and
additive growth rides a minor ("Schema evolution" below), so any number of builds interoperate as long as neither sends
what the effective minor does not define. Postcard's Rust-shaped wire, by contrast, breaks with the _types_, schema or
no.

Rejected alternatives:

- **Stay on postcard.** Keeps the Rust-shape coupling every non-Rust consumer pays and makes every type refactor a wire
  event; shedding both is the deciding argument above.
- **MessagePack / CBOR**: self-describing binary, more compact than JSON, but neither ships a schema-contract or codegen
  story for other languages; a consumer still reverse-engineers the message shapes. CBOR is the one serde-native
  survivor of the format scoring, and what it would win over protobuf is self-description, which the bridge already
  delivers in the language a scripting consumer actually wants.
- **Cap'n Proto / FlatBuffers**: lose twice. Their zero-copy accessors pay off when a reader touches a few fields of a
  large buffer in place, while felis decodes a rehydration frame once, in full, into an owned grid it then mutates: a
  linear decode that pays their ~2× wire overhead with no random-access dividend. Both also need an external schema
  compiler (`capnp`, `flatc`) in the build, which the `felis-protocol` purity rule forbids.
- **Hand-rolled TLV**: maximum control, maximum cost. It re-implements protobuf's tag-length-value framing and
  additive-skip rules by hand, with no codegen and no upstream maintenance (principle 1: add only what earns its place).

_Revisit if_ the prost dependency weight or the `just proto` codegen friction ever outweighs the cross-language benefit,
or a consumer's language finds generating a reader from `felis.proto` harder than expected.

### The schema is the authority over the Rust types

A shared `.proto` makes the schema the authority rather than a projection of the Rust types
([the reference states how the crate is arranged](../../reference/ipc.md#application-messages)): where the two could
disagree the schema wins, and the tests that read the file (`the_protocol_version_matches_the_schema`,
`every_message_kind_matches_the_schema_enum`) are what stops them from drifting apart quietly. What the domain types buy
for that mapping is invariants the generated structs cannot hold (a `u128` session id, `NonZero` correlation ids) and a
`felis-protocol` free of `tokio` and OS symbols: `prost`, `prost-types` (the well-known types the schema imports) and
`bytes` are the only added dependencies, all pure-Rust and runtime-free.

### Protobuf costs no throughput against postcard

Protobuf costs no throughput against postcard. On the wire it is size-parity with postcard's binary (both varint-encode
integers, protobuf adding one field-tag byte per field), so the 50 MiB rehydration budget in "Goals" is unaffected.

In isolation the codec is _faster_ on byte-blob-dominated messages, because prost bulk-copies a `bytes` field where a
serde/postcard path walks a `Vec<u8>` element-by-element; the `ipc_throughput` bench (`crates/felis-protocol/benches/`)
measures this on the shapes the daemon emits. The image chunk payload goes further and is generated as `bytes::Bytes`
rather than `Vec<u8>` ("Bulk payloads travel by reference" below). At full-pipeline scale the difference shrinks: the
`end_to_end_throughput` and `client_consume` benches (`crates/felis-client-core/benches/`) sit within single-digit
percent of a postcard codec, inside measurement noise.

### Where a wire value stops being a request

`convert/` checks every rule the schema cannot state, once, on the way in, and the payoff is that nothing downstream
re-validates: a domain value the tree is holding is one the schema plus those checks already admitted. Geometry is the
one field that cannot be settled there, which looks at first like a hole in that invariant.

`GridDims` travels as four `uint32`s and the domain type is `u16`, so the obvious decode narrows, refusing anything past
65535 as `WireError::OutOfRange`, which ends the connection. That is the right answer for a hyperlink id (a wrapped one
would alias another link's URI) and the wrong answer for geometry twice over. A create asking for 70 000 rows deserves
to be _told_ so, in a reason a caller can branch on; a live resize deserves to be clamped, because the user dragging a
window edge must not lose the shell behind it (REQ-605a). Neither is a connection-level fault, and the layer cannot pick
between them: from inside `convert/`, `SpawnArgs.dims` and `InputResize.dims` are the same nested message, and a
narrowed value has already lost what the policy needed to see.

So the two _request_ fields decode into `RequestedDims` (`u32`, the wire's own width), and the daemon narrows them to
`GridDims` by applying its policy: `admit()` on the create path, `clamp()` on the resize path.

Every other `GridDims` field on the wire reports geometry an honest peer has already put through one of those. That is a
claim about the peer rather than a fact about the bytes, so the decode re-admits it with the same `admit()` instead of
trusting it: a create naming a grid and a peer announcing one are asking the identical question, since a create that
wants the daemon's default says so by omitting `SpawnArgs.dims` rather than by a zero axis. The invariant holds, and the
types say so: a `GridDims` is admitted geometry, a `RequestedDims` is a request that has not been admitted yet, and the
compiler is what stops one being used as the other ("Optional fields instead of in-band sentinels" below).

Two alternatives are rejected:

- **Keeping the single `TryFrom` and re-checking geometry at each use site.** It breaks the "no consumer re-validates"
  rule this layer exists to hold, and it fails open: a site added later inherits the unchecked value silently.
- **Passing a context flag into the conversion** (a `dims_from_wire(.., Policy)`). It keeps one type but puts the
  daemon's admission policy inside the schema's validator, where a client that never creates sessions would still link
  it, and where a future third context with a third policy has nowhere to go but another flag.

_Revisit if_ a third geometry-carrying request appears whose policy is neither reject nor clamp; two policies over one
wide type is the cheapest shape for exactly two.

### Bounding announced quantities

Geometry generalizes. The rule `convert/` holds is: **every scalar that controls an allocation or an indexed growth is
admitted in `TryFrom<v1::*>`**, at the wire's own width, before any consumer sees it. Announced `GridDims`, the geometry
an image header's `New` target announces, and its `Frame` number are the fields that meet it today; the test for a new
one is whether the receiver's memory use is a function of the number rather than of the bytes that carry it.

That distinction is the whole reason the frame-body ceiling does not already cover this. A `MAX_PASTE_BYTES` breach has
to _ship_ sixteen megabytes; the framing layer would have refused a large enough one anyway, and the per-operation limit
only sharpens where the line sits. A claim ships nothing. `GridMsg::Size { rows: 65535, cols: 65535 }` is twenty bytes
on the wire and 4.3 billion cells in the shadow grid; an `ImageMsg::Header` announcing a `u32::MAX × u32::MAX` canvas is
a few dozen bytes and a `vec![0; n]` that never returns. Bounding bytes received bounds the wrong quantity, so these are
checked where the number is read rather than where the body is sized.

Letting the client trust the daemon is the rejected alternative, on the argument that a daemon buggy enough to announce
a bad number is buggy enough to ship bad bytes, and the chunk writer drops what does not fit anyway. It is wrong on its
own terms. The bytes never have to arrive, so "they would still ship them" is not the failure mode, and the chunk
writer's bounds check runs after the buffer it bounds was allocated.

It is also wrong on trust: principle 3 has the client trust the daemon for _content_, which is a statement about what to
render, not a licence for a peer to order memory. The client already ends the connection on a frame that does not
decode; a frame that decodes into an impossible demand gets the same answer, for the same reason: the peer has stopped
being the authority the mirror was mirroring.

The region reply is the same test read the other way, and it is bounded on the sender alone. Its bytes exist and the
daemon chose them out of its own scrollback, so the remedy is a trim to the youngest resumable boundary rather than a
refusal: a `pipe` over a full 10 000-row buffer is an in-spec request, and answering it by tearing the connection down
would be the daemon's bug. A receiver-side check would add nothing and cost something, since a daemon that never trimmed
would lose its connection over bytes the client can hold.

The client's aggregate image total is the one bound here that is not a per-field check. It exists because per-image
limits compose badly: five hundred headers each a legal 64 MiB is 32 GiB, all of it individually admissible. The daemon
holds the same total by evicting; the mirror cannot evict without inventing a retention policy the daemon did not run,
so it refuses and the connection ends.

### Optional fields instead of in-band sentinels

The roster's session row states two facts a daemon may not have: how long a session has been detached, and when an
attachment landed (the creation sequence is not among them: every daemon mints it, so the row always carries it). Each
of the two could be spelled with an in-band value: `idle_seconds = 0` for "attached", an empty string for "no stamp".
Neither of them is. A sentinel drawn from the field's own domain has to be excluded from that domain by prose, and prose
is not what a decoder reads: the row that reads `0` is indistinguishable from a peer that meant `0`, so every consumer
carries the exclusion, and one consumer that forgets it is a wrong answer rather than an error.

Presence carries it instead: each of the two fields is optional, absence is its only spelling of "the daemon does not
have this", and the in-band value it frees means what it says (the reference lists the absent case field by field beside
[the roster row](../../reference/ipc.md#cli-clients)). Where a type could still carry the convention, the well-known
type takes it: `attached_at` is a `google.protobuf.Timestamp` rather than an RFC 3339 string or a felis-private epoch
integer, so its range is checked by a decoder instead of asserted by a comment, and every protobuf consumer already has
a mapping for it while none has one for a felis convention.

A create's geometry is the same rule applied to a request rather than a report. `SpawnArgs.dims` is optional, and
absence is how a create asks for the daemon's default; rows or columns `0` is refused like any other value outside
REQ-605a's band. Spelling the default as `rows = 0` would cost more than the sentinel it saves: one wire type would then
need three admissions (refuse-with-sentinel, refuse-without, clamp) whose only difference is which zeros they forgive,
and the choice between them would be made by the verb rather than by the type, so a create and a resize carrying the
identical bytes would mean different geometries. Presence splits them for free, because a message field already carries
it in proto3.

The pixel pair keeps its `0`: a headless create and an unmapped window have no pixel size to name, and pixel extents are
the one part of a geometry a peer may legitimately not know.

_Revisit if_ a field appears whose absent case must itself be distinguished from two kinds of absence; a second optional
layer is worse than a named enum, which is what such a field should be.

_Revisit if_ a producer appears that must distinguish "a zero-pixel window" from "a window whose pixel size is unknown":
retiring the pixel sentinel means an `optional` pixel pair and a major bump, since the wire cannot say it otherwise.

### Ordering and completion of image transfers

Two shapes are rejected against the single active transfer whose completion the chunk count proves
([Image (kind = 3)](../../reference/ipc.md#image-kind--3)).

**Offset-addressed chunks, reorderable.** Every chunk would name an `offset` and the contract would admit any order. The
carrier is an ordered stream and the producer emits contiguously, so nothing would use the freedom; what it buys instead
is a receiver that cannot tell a complete buffer from an incomplete one. Verifying the promise means tracking covered
ranges per transfer, a structure whose only purpose is to police a permission nobody exercises, and without it a dropped
chunk renders as transparent pixels the producer never sent.

**A 0-based optional frame index.** `frame: None` means the root and `frame: Some(i)` an animation frame, but `Some(0)`
also addresses the root, with different semantics: `None` replaces the image and drops its frames, `Some(0)` edits the
root's pixels in place. Two encodings of one thing is a bug generator on the wire, and both were reachable from real
Kitty traffic (`a=t` versus `a=f, r=1`). The replacement is a `oneof`: `New { width, height, format }` and
`Frame { number }` with Kitty's own 1-based numbering, so the two meanings have two shapes, a frame cannot restate the
geometry it inherits, and frame `0` is not expressible.

Interleaved transfers are the alternative to the single active-transfer state; they would need per-target state on the
receiver to buy a concurrency the producer does not have.

_Revisit if_ a carrier appears that reorders (a datagram leg, a multi-stream one), since chunk offsets would then carry
information the stream does not.

### One encoding on the socket, JSON at a bridge

Protobuf binary is the only encoding a frame body ever carries. The alternative felis does not take is a **second wire
encoding**: proto3-JSON advertised beside the binary leg, with each connection stating which one it speaks. The pressure
for it is real, and it has a name: Elisp has no maintained protobuf runtime or code generator, so an Emacs client cannot
generate a decoder the way a TypeScript or Go consumer can, and a self-describing text encoding is the only wire it
could read without one.

A second encoding does not deliver that property, though, and it is expensive in the place that matters most. It does
not deliver it because the row payload is opaque `bytes` in either encoding: a JSON frame still carries base64 wrapping
a packed structure ([Row codec](../../reference/row-codec.md)), so the "no codegen needed" client still hand-implements
the one format that is hardest to hand-implement.

It is expensive because a JSON decoder generated from proto3 rejects an unknown field rather than skipping it, which
turns every additive field into a break for JSON peers while protobuf peers never notice: the compatibility matrix over
decoder × change kind that "Schema evolution" below exists to abolish. One leg cannot be made tolerant without diverging
the two legs' validation semantics, since unknown-field rejection is also what catches a hand-written producer's typo'd
field name.

So the JSON surface moves off the socket and onto a boundary that can hold it: `felis bridge`, a stdio subcommand of the
CLI that speaks protobuf to the daemon and one JSON object per line on its own stdin and stdout. The bridge is
LSP-shaped (one process per editor client, alive until stdin EOF) because that is the shape an editor already knows how
to supervise. Its JSON belongs to the [CLI output contract](../../reference/cli.md): that is where the surface is
normatively specified, and what its version number tracks, so a wire minor lands without the editor noticing, which is
the whole reason the boundary is worth having.

What a bridge client never sees is a grid, so the bridge answers none of the pressure a consumer that does mirror one
feels. That consumer reads `felis-json` v1 instead, a named format with its own envelope, schema and compatibility
policy ([ipc.md](../../reference/ipc.md) "Structural session JSON"), whose rows are spelled cell by cell rather than
base64 over a packed structure, the property a second wire encoding never had. The record of why that format exists, and
why it is owned where it is, lives in [overview.md](overview.md) "Shared wire knowledge across the satellite clients".

The bridge is the one place felis's process model bends the other way round: everywhere else a persistent daemon serves
short-lived clients, and here a client's own helper process persists to hold connections open on its behalf. It earns
the exception by holding what a one-shot process cannot: live daemon connections, kept along the seams the wire does not
let it multiplex across, so an editor's tenth request pays no dial. It never starts a daemon, which is the headless
surface's standing rule rather than a bridge-specific one: an absent daemon is the automation's real bug, and a helper
that resurrects one and then reports an empty roster has masked it ([control-surfaces.md](control-surfaces.md) "A
session verb never spawns a daemon implicitly"). The bridge is launched by an editor, not by a person opening a window,
so nothing about it reads as intent to have a daemon.

## Bulk payloads travel by reference

A grid row is a few hundred bytes and a keystroke is a handful; an image chunk is 256 KiB, and a reattach re-ships a
whole image store, which the cap lets reach 256 MiB. On that path a copy is not a rounding error, and the daemon's shape
multiplies it: one message is encoded per subscriber, and the pixels it carries already exist in the image store.

So the image chunk payload is refcounted end to end. `just proto` generates that one field as `bytes::Bytes`
(`buf.gen.yaml` names it explicitly), the domain `ImageMsg::Chunk` matches, and the store's frames hold `Bytes` too, so
composing a chunk slices the store's buffer, fanning it out to N subscribers bumps a refcount N times, and the client's
decode aliases the frame body the transport read. What remains is one copy at each end that has to exist: the daemon's
compositor builds a fresh canvas for `a=f` / `a=c` (a `Bytes` cannot be blitted into), and the client copies each chunk
into the pixel buffer the renderer uploads from.

The price is retention: a queued chunk pins its whole backing image until it is written. That is bounded by the same
store cap that already governs image memory, plus whatever the in-flight queues hold, and it is the trade the copy-free
path makes; the alternative pins the same bytes twice.

Only that field is converted. Every other `bytes` field on the wire is a session id, a keystroke, or a clipboard
payload, where a refcount and its atomics buy nothing over an inline `Vec<u8>`.

_Revisit if_ a second field grows into the same size class (a scrollback region reply, say), in which case it joins the
list in `buf.gen.yaml` rather than growing a general rule.

## Carrier choices

### Windows local carrier: named pipes

Named pipes are the Windows carrier because they are what tokio supports asynchronously and they carry the identity /
DACL machinery the peer check needs.

Rejected alternatives:

- **AF_UNIX on Windows.** Available since Win10 1803
  (<https://devblogs.microsoft.com/commandline/af_unix-comes-to-windows/>), but tokio's `UnixStream` is `cfg(unix)`
  (<https://docs.rs/tokio/latest/tokio/net/struct.UnixStream.html>), so adopting it means hand-rolling async readiness
  over a raw socket or bridging a sync-only crate: strictly more unsafe code for no gain over the pipe DACL.
- **TCP on loopback + token auth.** The "no network ports / no second auth layer" stance is about attack surface, not
  transport distance; a loopback listener is still a listener.

_Revisit if_ tokio ships async AF_UNIX on Windows.

### Cross-host attach: SSH stdio

**Sessions live in the remote daemon, not in the SSH link**
([the reference states what a disconnect tears down](../../reference/ipc.md#cross-host-carrier-ssh-stdio)). That is the
no-multiplexer bet (_the daemon is the persistence layer_) carried across the network: closing the SSH window no more
kills the remote shell than closing the local window kills the local one, which is the whole point of attaching over SSH
instead of running a multiplexer on the far side.

#### What the SSH carrier costs

Known costs: one daemon per remote host, per UID (tmux's per-host model); the remote daemon binds a `0600` socket in its
own `0700` directory under `/tmp`, the same per-user socket local felis already creates (a per-UID file, not a
multi-user exposure); one SSH connection per window (user-side ControlMaster multiplexes); and the remote daemon
inherits the env of whichever SSH session first spawned it (`TERM`, `LANG`, …), same as tmux. A remote daemon that
should _not_ outlive its SSH session (a throwaway shell on a transit host) is a session-level concern, not a transport
one: see the ephemeral-session and idle-daemon-exit revisit triggers in [session-lifecycle.md](session-lifecycle.md)
"Revisit: ephemeral sessions and idle-daemon exit".

#### The endpoint is derived from the uid alone

The endpoint is `/tmp/felis.<uid>/daemon.sock` on every Unix
([the reference states the order](../../reference/cli.md#carrier-and-connection-lifetime)), with `--socket` and
`FELIS_SOCKET` as the only overrides; no environment variable takes part. A relay whose login exported no
`XDG_RUNTIME_DIR` (an SSH server without pam_systemd, a Tailscale SSH session, any SSH login on macOS) therefore lands
on the socket the user's desktop session already binds, and so does every other process of that uid. Everything else
follows from one endpoint: no probing, no endpoint pair, and the relay is a plain connect-or-spawn.

`/tmp` is the location because it has the lifetime the daemon needs. A daemon whose reason to exist is outliving the
login that started it must not keep its only endpoint in a directory that dies with a login: logind removes
`/run/user/<uid>` at the last logout of a non-lingering uid, and a daemon under it keeps its sessions while nobody can
dial it. `/tmp` lives until the host reboots, on every Unix felis targets, with or without systemd, and identically for
a desktop login, an SSH login and a relay. tmux made the same call for `/tmp/tmux-<uid>` and ignores `XDG_RUNTIME_DIR`
and `TMPDIR` for this reason. A reboot ends the daemon; whether the socket inode survives depends on the mount, and
either way the next daemon binds the same path, which is what a `FELIS_SOCKET` stamp left in a shell names.

macOS uses `/tmp` rather than launchd's per-user directory because that directory is neither derived from the uid alone
nor guaranteed to live until reboot: `confstr(3)` documents that the contents of `_CS_DARWIN_USER_TEMP_DIR` may be
deleted after three days, `_dirhelper` consults `DIRHELPER_USER_DIR_SUFFIX`, and `confstr` falls back to `TMPDIR`
internally. tmux avoids it there too.

#### A shared `/tmp` is what the bind rule answers

The name `/tmp/felis.<uid>` is predictable, so another local uid can create it, or a symlink there, before this uid's
first start. The daemon therefore judges the directory it binds into rather than trusting it, as tmux's `check_dir`
does, and unlinks nothing after it binds. Those rules, the startup lock that serializes concurrent starters, and the
peer-uid check a client relies on instead are argued in [security-model.md](../security-model.md) "Daemon IPC". The
design relies on no tmp cleaner honoring a lock, and on none of them removing a socket in use: systemd-tmpfiles skips
any `AF_UNIX` socket present in `/proc/net/unix`, which is where the kernel records the pathname given to `bind`, so a
listener bound at the name it keeps is exempt.

A process with a private `/tmp` (`PrivateTmp=` services, bwrap or flatpak sandboxes) sees its own `/tmp/felis.<uid>` and
reaches no daemon outside it; felis is not supported from inside such a sandbox ([non-goals.md](../non-goals.md)). No
detection is built.

#### Rejected endpoint and cleanup schemes

- **Keeping the endpoint under the login's runtime directory.** Every mechanism the loss of that directory needed
  (watching it, draining on loss, descriptor-relative writes, per-instance agent links, stale stamp recovery) is a
  defense against a directory the system is entitled to take away, which is the wrong layer when a directory it does not
  take away exists.
- **Any environment-derived root**, whether "environment first, uid second", `${TMPDIR:-/tmp}`, or a felis-owned
  `FELIS_TMPDIR`. A variable in the default is how one process of a uid ends up on a different endpoint from another,
  which is the whole failure; `--socket` and `FELIS_SOCKET` already serve the user who wants a different location.
- **`$HOME`-rooted socket directories**, because a socket belongs in a runtime location, not a persistent, backed-up
  one, and NFS and the `sun_path` limit both bite there.
- **Probing the login manager's location from the relay** while leaving the derivation alone: a second mechanism for
  reaching one daemon, with a four-outcome classification and two open races that deriving the endpoint has neither of.
- **A client-side flag naming the remote socket**, paired with `--host`; it is a permanent CLI and carrier surface for a
  hatch the user must remember on every invocation.
- **Unlink-on-drop guarded by an inode identity captured after bind**, correct in every scenario left but still a
  window, where "never unlink after bind" has none.
- **A daemon-held lifetime lock as a cleaner shield**: only systemd-tmpfiles honors it, and combined with the startup
  lock it would make every later starter wait on a live daemon.

_Revisit if_ a target platform mounts a per-login `/tmp`.

#### The cross-host carrier re-dials

The cross-host carrier re-dials, not just attaches once. Its `Reconnector` descriptor carries the SSH destination and
its ssh args, so a remote window drives the same detach → dial → attach a local one does: it session-switches, pipes,
and re-points at another daemon (`felis window retarget`), rather than treating the first attach as its only move. The
sequential re-dial itself, and why simultaneous multi-daemon attach is rejected, are the
[session-lifecycle decision record](session-lifecycle.md#cross-carrier-re-dial).

It dials in the same `Window` mode a local one does, but not pull-paced. Demand-driven per-vsync pulls
([rendering/pipeline.md](../rendering/pipeline.md) "Frame pacing") spend a round trip per frame, which fits a local
socket but not SSH latency, so remote windows stay on the eager-push path.

Each switch or re-dial over SSH respawns an `ssh` child, so a user doing heavy remote switching wants OpenSSH
ControlMaster in their own ssh config; felis does not manage ssh multiplexing. Nor does felis put a deadline on the
`ssh` child: connection timeouts (`ConnectTimeout`, `ServerAliveInterval`) are OpenSSH policy the user's ssh config
already owns, and a felis-side timer would fire under a password or host-key prompt that the inherited tty is still
delivering to the user.

_Revisit if_ a felis path dials with no tty, so that no prompt can reach the user and a hang has no operator.

#### Rejected cross-host carriers

- **SSH `-L` port-forward of the remote Unix socket to a local path.** The client would have to know the remote socket
  path (UID-dependent) and juggle local file collisions and auto-cleanup races; running the relay _on the remote_
  sidesteps the path dance entirely (the path resolves where it is known) while delivering the same persistent-daemon
  attach.
- **Custom protocol over TCP+TLS.** felis would have to ship a TLS stack, certificate management, and a bootstrap auth
  protocol, none of which add value over SSH for the unix-developer audience, while exposing felis-specific network
  ports.
- **Mosh-style UDP** (<https://mosh.org/>). The reconnection-survival story is real on flaky links, but it needs a
  dedicated UDP listener on the remote host and re-implements the reliable-stream invariants the frame layer assumes;
  re-examine with a bug-bash-level user base, not before.
- **WebSocket over reverse-proxy.** A remote-display story, not cross-host attach.

## Handshake bootstrap

Version selection cannot depend on the schema being selected. A version field inside a schema-encoded frame answers
"which schema do we share" only for peers that already share one: the frame has to be decoded before it can be read, and
the peer whose answer matters most, the one speaking a schema this build has never seen, is exactly the peer whose frame
will not decode. So the exchange that picks the version sits _below_ the schema, in a fixed layout that is frozen
forever: 8 bytes from the client, 10 always sent back
([the reference's "Version preface"](../../reference/ipc.md#version-preface) has the layout).

### The preface reply is self-describing

Frozen means the layout is how any future felis reports a mismatch to any past felis, so the reply is
**self-describing**: a status word discriminates it, and the two words after it mean what the status says they mean (an
agreed major and the daemon's minor on accept, the supported major range on refuse). The alternative shape, in which the
client compares the returned words against what it sent, is rejected: it makes the reply's meaning depend on the
reader's memory of the request, so a status this peer has never heard of could be misread as one of today's answers
rather than as the refusal it is. A status this build cannot name is therefore a refusal whose reason it cannot report,
and the room to add such a status later is what the discriminator buys.

Self-describing is a property of _decoding_; negotiation stays exact (REQ-104d). A peer that accepts a major it was
never offered is not running felis's negotiation, so the client treats the accept as an error of its own rather than as
a refusal. The lenient alternative, taking any status-0 reply as agreement on the offered major, is rejected because it
moves the failure to the wrong layer: a daemon that answers with a schema the client does not have would then fail as a
protobuf decode error after the preface, in the frames the frozen layer exists to refuse before they are read.

The daemon pairs the accepted major with that major's minor in one selection for the same reason: two independent
lookups let a build that serves two majors hand the older schema the current major's minor, and the client cannot tell
that pairing from a correct one until a frame fails.

_Revisit if_ a second major ships side-by-side decoders: the selection then grows a per-major minor table, and the
client's check stays as it is.

### The magic and the byte order

The magic `FLIS` leads both directions, and a stream that opens with anything else closes with no reply at all:
answering an unknown protocol in ours is how a stray connection to the socket becomes a confused peer instead of a
closed one. Every preface integer is big-endian while the frame header stays little-endian. The divergence is
deliberate: the preface is a self-contained frozen artifact where network order is the convention a reimplementer
expects, and the frame header is decoded a million times a second on hosts that are little-endian.

Nothing here defines a migration from a preface-less stream: the preface predates the compatibility freeze, and a dev
build carries no compatibility promise, so no released felis ever meets a peer without one.

### The relay carrier block sits below the schema

The frozen layer admits one more magic ahead of the client preface, `FRLY`: the relay carrier block, which carries the
SSH relay's own environment so a create over that carrier has a host-correct base to fall back to
([security-model.md](../security-model.md) "Process and environment boundary"). It sits in this layer rather than in the
schema because it is written before either peer has said what it speaks: a block a daemon had to negotiate access to
could not be sent at all. Its caps are frozen for the same reason: a per-minor or per-daemon cap would reintroduce
exactly the skew the frozen layer exists to remove, since neither side knows the other's minor yet. Consumption is
exact, and every malformed shape closes like a wrong magic.

Two shapes are rejected:

- **A protocol-aware relay**, decoding the client's frames and rewriting a create's environment field. It needs no new
  magic, but it turns the relay from a byte splicer into a second implementation of the wire, which must then track
  every schema change and be version-negotiated in its own right.
- **Probing the daemon's minor on a side connection** before deciding whether to send anything. It would be a racy extra
  round trip to learn what the frozen layer makes unnecessary: every released v1 daemon reads both forms.

#### The payload names its own format; the envelope never does

The protocol major cannot select a carrier decoder, because it arrives in the client preface behind this block, so the
payload's first word is a format version and the magic and length word stay fixed for all time. A reader that meets a
version it has no decoder for has already consumed the payload by that length word, so it skips the block, warns, and
reads the preface from the next byte: the same degrade a relay that prepends nothing produces, at the same cost.

Version 0 is malformed, which is what makes the introduction of the word fail fast in both skew directions instead of
hanging. A writer from before it puts its entry count where the version belongs, and the count's high half is zero; a
reader from before it takes the version word as its entry count's high half, which is at least 65536 and breaks the
4096-entry cap before any entry is read.

Two other ways of versioning the block are rejected:

- **Widening the header with the version** fails on that second direction: a reader expecting an eight-byte header would
  take half of a wider one as the payload length, and a length of 65536 against a shorter stream leaves both peers
  waiting on bytes neither will send.
- **A new magic per payload layout** spends the discriminator that tells a carrier block from a bare `FLIS` stream on a
  distinction the payload already makes, and would oblige every reader to carry the list of retired magics to keep
  skipping by length.

A later payload format therefore lands as a new version word under the same envelope, and the caps that guard the read
stay where they are.

_Revisit if_ a carrier payload has to carry something a connection cannot proceed without: skipping an unknown version
is safe only while the block's whole content is a fallback the daemon has a documented substitute for.

#### An oversized environment degrades rather than fails

An environment that will not fit the frozen limits therefore degrades rather than fails: the relay warns and splices
without a block, and the daemon falls back to its own environment. Refusing the connection instead would trade a create
whose agent socket is stale for no session at all, on a host the user reached specifically to get a session.

A **chunked** payload the daemon reads incrementally is rejected on the same ground the protocol-aware relay is: chunk
boundaries are framing, and a relay that has to respect them is not the byte pump the splice depends on.

_Revisit if_ an environment over 1 MiB is observed on a host felis is expected to serve; a measured login environment is
tens of KiB today.

### `Hello` and `Welcome` carry no version check

`Hello` / `Welcome` stay the application handshake inside the selected schema
([the reference's "Handshake"](../../reference/ipc.md#handshake)), carrying the connection mode and `pull_paced`. A skew
refusal is not among their jobs: the preface has already refused what it cannot serve, so a `Welcome` arriving at all is
the compatibility statement, and echoing a version back for the client to re-check could only confirm what the refusal
path already enforced.

## Correlation rides in the body

A connection carries several conversations at once: a window searching its scrollback while the grid streams, an editor
bridge with four `sessions` requests and a notification subscription in flight. Each reply and each stream item has to
name what it answers, which is a **correlation envelope**: a `request_id` on a request and the reply that echoes it, a
`stream_id` on a stream-opening request and every item, cancel, and terminal that belongs to that stream.

### The envelope is a body field, not header bytes

The envelope is a protobuf message inside the frame body, at field number **100** of every family wrapper, reserved
wire-wide. It is not header bytes ahead of the body. Header bytes would be a second hand-rolled layout that protobuf's
field rules cannot evolve: a fixed slot is spent or dead forever, and populating a reserved one later reinterprets bytes
every deployed peer ignores. That is the exact class of implicit contract that making the schema the authority removes.

One uniform field number is what lets a reader lift the envelope off any body without a per-family table, and that in
turn is what lets a single connection driver validate correlation for every family in one place. The cost of the choice
is a field number spent wire-wide; an absent optional message field encodes to nothing, so the uncorrelated
connection-scoped pushes (`Grid`, `Image`, `Push`, `Input`) are byte-for-byte what they would be with no envelope
defined at all.

### `Conn` carries no envelope; `Session` carries an optional one

`Conn` carries no envelope, deliberately: an optional envelope field cannot say "required on this arm", and a terminal
whose `stream_id` went missing is malformed. Kind 0 is where the stream-lifecycle messages live for the same reason they
are not per-family: one home is what lets the driver enforce "exactly one terminal per stream" without learning a
terminal shape per family.

`Session` carries the slot the other way round: the family is correlation-capable, and the attach and create arms leave
it unset, because a connection attaches or creates once and an ack with nothing to disambiguate needs no id. The input
fence pair (`InputFence` / `InputAccepted`) is the arm that uses it, a correlated barrier on the attached connection. A
family that never declared the slot would have made that pair a wire break instead of an additive arm.

### A connection is persistent and multiplexed

What correlation buys is that a connection is persistent and multiplexed: several requests and streams interleave on
one, and the answer to "which ask is this" is in the frame rather than in the connection's identity. A peer that opens
one connection per operation is then making a choice, not obeying the wire. The `felis` CLI makes it: a verb invocation
dials, operates, and disconnects ([control-surfaces.md](control-surfaces.md)), because a process that exits after one
operation has nothing to reuse a connection for. The bridge, which does, holds its connections open.

### One identity per arm

The envelope is a `oneof`: a frame names a request or a stream, never both and never neither. Two independent optional
scalars are rejected because the schema can then encode states the model has no reading for. Neither set is a frame
nothing can route; both set is a frame whose meaning is decided by whichever call site looks at which field first. That
is an optional-field bag rather than an identity, and a non-Rust implementation of this wire would have to reproduce
felis's call-site order to interoperate. No arm needs both concepts: a future operation that is genuinely a request
_and_ a stream opener should define a deliberate conversation shape, a second named field on that arm, rather than
widening every envelope on the wire to admit it.

The `oneof` keeps tags 1 and 2, so an honest frame encodes exactly as a bare scalar does, which is what lets the two
independent sequences, the ABA argument below, and the already-exclusive `Subject` stay as they are.

Which id an arm must carry is the `correlation` column of the arm table ("Kind or arm?" above), and the driver checks
the envelope against that column in `decode`, on every family: a wrapper that only `reserved`s field 100 still reads
back an envelope a hand-built frame stamped onto it. Validating in the driver rather than per handler is the point: a
handler that recovers its own mandatory id afterwards states "this arm requires a request id" once per call site and
enforces it nowhere a peer could read. The arm's class is the single statement, and a delivered message carries the
identity the driver validated.

The sender checks the class as well, and the redundancy is the point. A correlation violation is fatal to the
connection, so a caller that writes one reads an EOF and nothing else: the frame left, the peer refused it, and the
diagnosis lives only in the daemon's log. Checking the class where the frame is encoded turns that into an error at the
call site, on the side that can fix it. The minor gate resolves the same asymmetry, since `send` refuses an unauthorized
addition rather than letting the peer discover it. The sender's half stops at the class: it has no view of which ids are
outstanding, and duplicating that ledger would give the two ends two answers to disagree about.

One implementation detail is load-bearing. prost resolves a `oneof` **last-field-wins**, so a body setting both tags
would decode as a stream envelope rather than being refused, and "both ids is malformed" would hold only for senders
that already obey it. Mirroring field 100 as two plain scalars does not settle it either: a body writing `request_id`,
then `stream_id`, then `request_id` again as zero leaves the mirror holding one id, so the ambiguity is gone before any
check looks for it. The peek that lifts the envelope off a body therefore reads that submessage a tag at a time and
refuses a body in which both tags appear at all, whatever value each last held. The schema forbids both; the peek is
what proves a peer did not send both anyway.

### Two id sequences, both client-allocated

The client allocates both ids, and a stream-opening request carries the `stream_id` it is allocating rather than
receiving one back. That is what lets a client cancel a stream that has stalled before its first item: with a
daemon-assigned id, a stream that never produces is a stream the client cannot name.

Three alternatives are rejected:

- **A single shared sequence for both ids.** An inactive id would then have two readings, a stream that already
  terminated and an id that was a request and never a stream, so the daemon could not tell a late cancel from a
  fabricated one.
- **Reuse**, for the ABA it invites: a stale `Cancel` for a retired id would kill whatever unrelated stream now holds
  it, which is why exhausting a sequence ends the connection rather than wrapping.
- **Tracking only the _active_ streams**, for the same distinguishability reason as the shared sequence: it collapses
  "terminated" and "never opened" into one lookup miss
  ([the classification is in the reference](../../reference/ipc.md#correlation-requests-and-streams)).

The request counter is held the same way, for the same reason. A request id that skips or repeats is caught where it was
sent rather than at whatever reply it would later have made ambiguous. Nothing on the daemon side records _which_
requests are still unanswered: the sequence is strictly increasing, so an id is either the next one or already
malformed, and an unanswered-request set would have to be retired on every path that writes a reply (the two `Ops`
loops, `Region::Reply`, a typed `Conn::Error`) or grow for the connection's life while buying no check the counter does
not already make. The client keeps its outstanding set because it needs one for a different job: matching an
out-of-order reply to the caller waiting on it, and refusing a duplicate or late one.

### What a switch reply can report

A switch's result says `queued`, and means only that: the subscriber outboxes took the push. The frame is enqueued when
the reply is written, and every step that would make the move real happens afterwards, on connections of their own: the
dial, the attach, the supersession by a later switch. The reply cannot honestly report more than admission, which is why
it reports the count under that name.

Tracking each push through to a verdict is the alternative, and it is rejected on two counts. Nothing reads such a
verdict: the CLI and the bridge report `queued` and exit, and the shipped agent skill drives sessions that have no
window to move at all, so the daemon would carry the bookkeeping for no caller.

The verdict would also miss the case that most wants one. A carrier retarget lands on a different daemon, which holds no
record of the request and cannot resolve the origin's namespace, so the origin observes nothing whatever about the
landing it asked for; the only evidence it ever holds is silence, and a verdict inferred from silence is a timeout
dressed as an observation.

Having the client report its own landing back to the origin is rejected too, because it closes that gap by opening a
worse one: it makes the client a participant in daemon bookkeeping across hosts, where every host in a chain must be
trusted to report honestly about a session it does not own.

Accepted-only is therefore the frozen semantics of the whole switch family, not a gap left for a later completion path,
and both user-facing surfaces are worded to match: the CLI exits `0` on admission and says `not landed` in so many
words, and a landing that fails afterwards is the target window's own business
([control-surfaces.md](control-surfaces.md) "A relay verb reports queue admission").

_Revisit if_ a consumer appears that must act on the window's new home: a script that retargets a window onto another
host and then works against it there. That consumer is what would define the verdict, since what it must read is what
the reply would have to carry.

## A corrupt frame ends the connection

A frame can stop making sense in four places, and the reference enumerates them
([its "Corruption"](../../reference/ipc.md#corruption)): the kind, the direction, the body, the envelope. All four get
one answer. The connection ends, with a typed error naming what was expected against what arrived.

Skipping the frame and continuing is rejected. The peer has already put bytes on the wire that this side cannot account
for, so the position it would resume from is derived from a length that same peer computed, and there is no reason to
trust the next frame boundary more than the one just failed. The failure is also silent where it matters most: a client
that drops a grid frame it cannot read goes on drawing a screen that does not match the daemon's, and the user sees a
terminal that is merely wrong rather than a connection that is visibly gone.

A value that decodes and still cannot be true gets the same answer, which is the harder half of the rule. A row index no
geometry admits, a scroll region outside the grid, a cursor off the screen, a row naming a registry entry that was never
sent: each is a number the daemon could not have produced, so the mirror refuses it before it writes a cell.

Normalizing them instead is the tempting reading (clamp the coordinate, drop the row, draw the unresolved cell as a
fallback glyph), because each one keeps a window alive that would otherwise blink through a reconnect. It is rejected
for the same reason skipping a frame is: the client goes on reporting a healthy connection while showing a screen the
daemon never composed, and the divergence has no later event that repairs it. The one accommodation that survives is the
resize race, where a row lower or wider than the mirror has an honest reading, and the reference states its bounds
([its "Grid admission"](../../reference/ipc.md#grid-admission)).

The unit of failure is one connection, and that is what makes the strictness affordable. The daemon's session keeps
running, the PTY keeps its process group, and every other client of that session is untouched: a corrupt or malicious
peer can only kill its own attachment. Reattaching costs a rehydrate, which is the same cost the client already pays for
closing its window.

### One driver defines "unexpected"

"Unexpected" has to be one definition rather than one per call site, which is why the phase machine, the arm table
("Kind or arm?" above), and the cancel/terminal races all live in a single typed connection driver that the daemon and
both clients run. The cases where leniency is correct are in that definition rather than sprinkled through handlers: an
item racing a cancel is expected and dropped, a cancel for a terminated stream is an idempotent no-op, and a
stream-opening request past the per-connection bound is a typed refusal that keeps the connection.

Cancellation only means something if the producer can stop, so producers are written for it: a search or region reply
streams in bounded chunks rather than materializing its whole result and then emitting it, since a collect-all producer
has nothing to interrupt and a cancel against one buys the client only the right to stop reading.

### A surface outside the stated mode is answered, not treated as corruption

A peer reaching for a surface its stated mode does not admit is not corruption, since the daemon answers before closing,
and what the answer is depends on what closing would cost. Before the peer attaches, nothing is riding the connection:
the answer is `Refused`, and the connection ends, because a peer that ignores its own declaration has stopped being
predictable. Once it has attached, the connection carries a session the peer is streaming, and dropping that to punish
one misaddressed verb costs the user their window for a mistake the verb's own typed error already reports. The driver
decides _that_ the arm is inadmissible and reports it; which of the two answers to write is the caller's, because only
the caller knows whether it still holds the writer the pre-attach refusal needs.

### The phase ladder

The phase column is not just progress through the handshake; it is what the connection has _become_. A welcomed
connection that has not yet attached, one streaming a session, and one serving the notification fan-out take part in
disjoint conversations, and each frame's legality depends on which of the three it is. The ladder therefore runs
`Preface → Handshake → Setup → Attached | Observing`, and the reference tabulates the rows
([its "Connection phases"](../../reference/ipc.md#connection-phases)).

Rejected: **one post-handshake state**, with the rest reconstructed where the frame lands. It costs a second admission
policy per call site: the daemon's pre-attach loop refusing a second opener, the attached pump refusing an `Attach`, the
observer loop refusing everything but a cancel, each by a `match` arm no peer can read. That shape promises one
validator while leaving an implementer to reconstruct the sequencing from three handlers, and a rule stated in a handler
is a rule the arm table cannot restate for a non-Rust peer.

Rejected: a **typestate API**, a distinct Rust type per phase, so an out-of-phase call is a compile error. The driver is
shared behind a mutex in the GUI and behind `&mut` in three daemon loops, and a typestate would force each host to
thread a different type through the same reader; the driver would become three drivers, which is the shape already
rejected just below. The phases are data the driver owns, which is also what lets `felis.proto` restate them.

A `Closing` phase would name a state with no frames in it: a detach or a stream's terminal is the last frame either end
has a use for, so the interval after it admits nothing. A window that wants another session dials again, which it must
do anyway to reach a different daemon.

_Revisit if_ a connection ever needs to re-attach in place: a `Detach` that keeps the socket for a later `Attach` would
need `Attached → Setup`, and the ladder is where that edge belongs.

### Why the driver lives in felis-transport

The driver is protocol knowledge, so `felis-protocol` would be its natural home. It cannot live there: it holds a live
connection's mutable state and is driven by async readers, and `felis-protocol` is barred from `tokio` and from anything
OS-specific because it is the cross-language reuse surface. `felis-transport` is the first crate above it that already
depends on both `felis-protocol` and `tokio`, and everything else in the workspace depends on `felis-transport`, so the
driver reaches the daemon and both clients from there with no new edge in the dependency graph.

Rejected: a **new crate between protocol and transport** to hold it. It would carry one module, and the crate seams are
extraction points ([overview.md](overview.md) "Workspace: the crate-boundary decision record"): a seam that exists only
to separate a state machine from the bytes it validates is a seam a repo split would have to un-draw.

The driver owns no socket either, for a related reason: its three hosts drive their reads differently (the daemon runs
two independent pump loops over one connection, the GUI runs a reader task feeding an event loop, the CLI wraps its
reads in deadlines), and a driver that owned the socket would have to become three drivers to serve them.

_Revisit if_ the repo split needs transport bytes-only: a consumer that wants the framing without the protocol
vocabulary is the point at which this module's dependency on the message families starts costing something.

## Pipe-to-clipboard provenance

A `RegionToClientMsg::Reply` answering a `pipe`-to-clipboard chord and a `GridMsg::ClipboardSet` both end with the
client writing bytes to the system clipboard, yet they take different paths there. `ClipboardSet` is a running program's
`OSC 52` write, which the client treats as hostile input: it is gated behind `clipboard.osc_52`, which defaults to
felis's own mirror, so an arbitrary program cannot silently overwrite the user's clipboard. A region reply answers the
user's own chord (the same intent as `Ctrl+Shift+C`), so the client applies it ungated via `write_user`. Routing the
user's pipe through `ClipboardSet` would make the pipe silently no-op under the default config, because that frame's
gate cannot tell "the user asked" from "a program asked".

What carries the distinction is the _request_, not a marker on the reply. The client parks the sink the chord named
while the region is in flight and reads it back when the reply lands, so "the user asked for the clipboard" is something
the client knows first-hand: a fact about its own state, which no frame from the daemon can forge.

Rejected: **a distinct reply variant per sink**, so the clipboard case arrives pre-labeled. That puts provenance on the
wire, where it is one more thing the daemon has to get right and one more arm every reader has to match, to re-derive a
fact the requester already holds. Provenance is a property of who asked, so it belongs on the side that asked.

## Typed unions for mode-selecting fields

`OpsToDaemonMsg::Switch` names where the from-session's windows should go with a `SwitchTarget` union (`Session(prefix)`
or `Carrier(descriptor)`) rather than a session id beside an optional carrier that overrides it. The two destinations
are different kinds of place: an id is something this daemon resolves against its own pool, a carrier descriptor is
something only the client can dial ([the reference's "Ops (kind = 5)"](../../reference/ipc.md#ops-kind--5)). One union
arm per kind of place lets the daemon match the mode it must serve; a field pair would put both in every request and
leave each reader to infer which half is live.

The pair shape's larger cost is at the version boundary. An optional field is invisible to a peer that does not know its
number: it skips the carrier, reads the id, and a request meaning "leave this daemon" arrives as "switch to session 0",
a well-formed request the daemon would carry out. A oneof makes the same frame fail to decode, which is the correct
answer from a peer that cannot understand what was asked, and it holds without the sender pre-checking anything to
protect the receiver from its own leniency. The mode gate is a separate question: it decides whether this connection is
wired to ask, never whether the daemon understood what was asked.

The general rule this instances: when the presence of a field is what selects a mode, the mode belongs in a type.
Optional fields are for data that is genuinely absent, where the reader's default is the right reading.

### An unset union is malformed

The rule has a second half, which is what an unset union means. No absence on the wire carries meaning: every
mode-selecting union names all of its arms, including the arms that carry nothing (`RetargetTarget`'s `default_local`,
`OpsStop`'s `if_empty`), and a union that arrives unset is malformed. That is what makes an arm added later safe to
meet: a decoder that does not know the arm drops it as an unknown field and sees an unset union, which it refuses,
rather than performing whichever operation the schema nominated as the default. The cost is one tag-length on the arms
that carry nothing, paid on one-shot requests.

Rejected: **unset as the default arm**, where `RetargetTarget.carrier` reads as the default-local socket and
`OpsStop.mode` as `if_empty`. It reads well: the common case costs no bytes, and the safe posture is the one a producer
writing nothing gets. But it makes a frame the decoder could not understand indistinguishable from a frame that meant
the default, and the two want opposite answers. It also makes the default a property of the reader rather than of the
sender, so two peers built a minor apart disagree about what was asked while both decode cleanly. `SwitchScope` refuses
an unset union for exactly this reason; one policy across the families is what keeps a reader from having to remember
which is which.

_Revisit if_ a union appears whose arms are open-ended enough that refusing an unknown one costs more than serving a
conservative default: the answer is then a `_UNSPECIFIED`-style arm the sender chooses explicitly, not a return to
absence.

### `RetargetTarget` is two nested unions

`RetargetTarget`, the descriptor a `Carrier` switch carries, follows the rule twice over, as a pair of nested unions
rather than a bag of optional fields. **How** the client reaches the target daemon is one choice (its own default local
socket, an explicit local endpoint, or an SSH destination with its argument tokens) and **what** it does once the dial
lands is another (attach an existing session, or create one with `SpawnArgs`). A bag would put a socket path, an SSH
destination, a session prefix, and spawn arguments in every request and leave each reader to infer which combination is
live, including combinations that mean nothing: a session prefix beside spawn arguments, an SSH destination beside a
local socket path. Two unions make each of those unrepresentable, and the choice the daemon must relay is the one the
sender made.

### Palette, theme and attach targets

`GridMsg::PaletteColor` and `GridMsg::ThemeColor` are the rule's other instance: a `Set { rgb }` / `Reset` union for the
action and a message of its own for the whole-table reset, so neither a lost color nor a missing index can arrive as a
reset the program never asked for. `SessionAttach` is the shape without a default at all: its `id` and `id_prefix` are
one `target` union rather than sibling fields, so "both" is not something a sender built from the schema can write and a
peer generating its own codec reads the constraint from the descriptor instead of from a comment.

### A status report's scope is a union

`OpsToClientMsg::StatusReply`'s `ResourceReport` applies the rule to a report rather than a request. Which subject a row
counts per is what decides whether its per-subject numbers mean anything at all, so the scope is a oneof (`DaemonScope`
or `SubjectScope`) carrying those numbers inside the arm. Rejected: a scope enum beside three `optional` numbers, with
the correspondence frozen in prose. It leaves a daemon row carrying a per-subject ceiling representable, and it makes
every reader branch on a missing key to learn which denominator it is holding, on a report whose whole purpose is to be
read under pressure.

Each ceiling inside an arm is a `Limit` union of a bound and `Unlimited` for the reason "Optional fields instead of
in-band sentinels" gives above: collapsing to `0 = unlimited` cannot be made to work, since `0` is a legitimate
`max_subject_used` (every subject empty) and a legitimate ceiling (a resource nothing may hold). What each arm carries
is in [the reference](../../reference/ipc.md#ops-kind--5), and the `felis daemon status` rendering of it in
[control-surfaces.md](control-surfaces.md#diagnostic-verbs).

_Revisit if_ a resource appears whose ceiling is neither a count nor absent.

## Schema evolution: major, minor, feature flag

Every change to the wire is a major, a minor, or a feature flag
([the reference defines the three](../../reference/ipc.md#versioning)), and each of the three earns its place against
the same alternative.

The rejected alternative is a single **exact-match generation**: one number covering the whole schema, refused on any
mismatch. It buys a stronger invariant (same number, identical message set), and the price it charges for that is the
wrong one. Under exact match, adding an optional field no old peer would ever read still refuses every client until it
is rebuilt and restarted in lockstep with the daemon. Restarts in lockstep are how sessions die, and the daemon exists
to keep sessions alive across exactly this kind of churn: a versioning rule that makes persistence conditional on a
synchronized fleet upgrade has contradicted the reason the daemon is there. Also rejected, for a plainer reason: an
**exact-match check on the build**, which reads every rebuild as an incompatible peer even when the wire is untouched.

The three differ in what each asks of the peer on the other end. A major is expensive by construction, since a daemon in
a deprecation window carries side-by-side decoders, one message set per major it still serves; that expense is what a
change costs when no additive shape can express it. A minor never requires a coordinated restart, the one property the
whole policy is built to protect. And a flag earns its place only where a real peer declines something ("Connection
modes, not per-feature bits" above), which today is pull pacing and nothing else.

### The minor is a send-side contract

Protobuf's unknown-field tolerance covers unknown _fields_, not everything a schema can grow
(<https://protobuf.dev/programming-guides/proto3/#updating>), so the minor pins the rest. The effective minor,
`min(client, daemon)` fixed by the preface before any frame, is a **sender's** contract: a peer never sends a frame
kind, a oneof variant, or a row-codec version the effective minor does not define. It is a property of the connection,
never of the build: what `felis daemon status` reports as `PROTOCOL_MINOR` is the daemon's own, the highest it could
speak, and the two are equal only when the peer is as new.

Unknown kinds and variants therefore stay fatal, and the reason is not strictness for its own sake. Between peers
honoring the contract they cannot occur, so one arriving means either that the sender is not honoring it or that the
bytes are not what the sender wrote. Both readings say the connection has stopped being trustworthy, which is the
corruption case above. Tolerating them would also require deciding what a frame means while its meaning is the thing
missing: a skipped `Grid` variant leaves a client's screen quietly wrong, and a skipped `Ops` variant leaves a caller
waiting for a reply the daemon believes it answered.

The row codec is the one payload the schema does not describe, and it gets no exemption. It carries its own version tag,
but a new codec version is gated on the effective minor like any other addition: the tag identifies which codec wrote
the bytes, and the effective minor is what authorized the sender to use it.

### The ledger is the review gate

Each minor's additions are recorded in one table in the reference docs
([the minor ledger](../../reference/ipc.md#the-minor-ledger)): the minor, the kinds, fields, variants and codec versions
it adds, and what a peer that predates it does with each. The table is not documentation of a decision already made: it
is where the decision gets checked. A change whose old-peer behavior cannot be written in the cell is not additive, and
the cell is where that surfaces before the change ships rather than after.

A change that fails that test sits beside the table rather than in it, because no cell could assert that an older peer
ends up somewhere true. The dangerous shape is the one that lands the peer somewhere false, such as a repacked report
that an older peer reads as an idle daemon: nothing about it looks like a failure. A change that makes the older peer
fail to decode ends the connection instead, which is the outcome a skew should have and still not a cell anyone can
write, since a cell promises a degradation rather than a failure. Only the absence of a compatibility promise before the
first release makes either kind admissible, which is the same fact that keeps them off the table: after the freeze each
is a major.

A table alone gates review, and review is not what runs on the wire. So each row also exists as metadata beside the
definition it authorizes: an arm's `since_minor`, a `since_minor()` per closed-enum value, `ROW_CODEC_SINCE` per codec
version. The writer reads it, so a message answers what it costs to send and `FrameWriter` refuses one the connection's
effective minor does not define. Nothing reaches the carrier except through that gate, including the daemon's
pre-encoded fan-out and the client's send queue, which carry the requirement alongside the bytes because neither writer
ever sees the message. A feature that wants to talk to an older peer therefore has to say how: omit the field, map the
value down, or refuse locally.

Gating the receive side instead is the obvious alternative and does not work: a receiver can see that a value is
unknown, but not what the sender should have sent in its place, which is the whole content of a ledger row. The one
shape the writer cannot see is a value inside a body it treats as opaque, so an enum's down-map stays a call at the
construction site; what keeps that honest is the coverage test, which fails naming any value the ledger and the metadata
disagree about.

Conversations that exercise every row in both directions are the natural next gate. The first release has one row and no
older peer to protect, so the first minor added past the freeze is where that earns its cost, and the send-gate test is
the template it fills.

### Narrowing a decoder is not a minor

Narrowing what a decoder accepts has no cell in that table, and the absence is the point. The per-operation limits and
the image header's admission (["Semantic limits"](../../reference/ipc.md#semantic-limits)) add no field and change no
field's meaning, so they are not a minor's addition and not a major's break; what they change is the set of values a
receiver tolerates, which no "older peer" column can answer.

The bounds the first release ships are therefore part of the major, and the daemon emits nothing they refuse: each is a
receiver's independent verification of what the sender already does. Narrowing one further is a break in the only sense
that matters to a user, a working connection that now closes, so it owes a real old-peer answer: refuse the value but
keep the connection, or gate the new bound behind a minor the sender opts into.

### Field numbers and their retirement

Numbering and retirement follow from the same contract. Field numbers inside each family's oneof are the ones
`felis.proto` declares, and they are frozen for the life of the major. What a number owes on retirement is decided by
the release baseline (the newest published final release, the revision the wire gate compares a tag build against,
[testing.md](../../reference/testing.md#wire-compatibility-gates)): a number that baseline carries is `reserved` and
never recycled, because a released peer may still send it and its bytes must never be read as a new field. A number
retired while no baseline exists is free again, because the `base:` acknowledgment that permits such a retirement
already says both sides are rebuilt together, so no peer survives to send it. The committed `felis.proto` and its
codegen (`just proto`) are the record.

### What Buf checks, and what it cannot

`buf breaking` under the `WIRE_JSON` rules sees field numbers and types, and the two things this wire's contract also
hangs on are invisible to it: a field whose meaning changes under an unchanged number and type, which is the semantic
break the major exists for, and the row bytes inside `packed_cells`, which reach it as one `bytes` field ("The tooling
gap this leaves" below). That is why the ledger sync test and the row-codec golden vectors stay separate gates beside it
rather than folding into the schema comparison; [testing.md](../../reference/testing.md#wire-compatibility-gates) lists
the three with what each covers.

The comparison is only as good as its base, which is why the CI gate is a job that resolves one from the event (the PR's
merge-base, the push's prior revision, and on the tag path the newest published final release, which `release.yml` takes
on every tag) on a full-history checkout, and fails when it cannot. Running the `buf-breaking` pre-commit hook under
`nix flake check` would not do: the sandbox evaluates a store copy with no `.git`, where a hook can only skip, and a CI
checkout is `HEAD` itself, so even with history the hook would compare the schema with itself. That hook stays as the
developer's early warning (work tree against `HEAD`), which is the one place that comparison means something.

An intended pre-release break is acknowledged in a committed file naming the base it was written against. The file is
visible in the diff, in `git log -p`, and to a release workflow, which is what lets the acknowledgment carry a rule
(accepted against the named base or a base that only grew compatibly past it, never a line the base already carried,
never on the tag path) rather than a bypass. Two other ways of acknowledging it are rejected:

- **An environment variable.** `SKIP=buf-breaking` silences the hook on the developer's machine and is invisible to CI,
  which is where the gate runs.
- **A commit trailer.** A commit-message trailer is not in the PR diff, does not survive a squash, and gives the release
  gate nothing to read.

The release baseline is read from the released tag, with the forge deciding only which tag that is. Rejected: a copy of
the released schema committed to `main`, which the release workflow proposes as a pull request. The tag already holds
those bytes, so the copy adds a second record that must be kept equal to it, and because `main` is protected the next
tag fails the gate until a human merges that pull request.

_Revisit if_ Buf gains a plugin that can read the Rust domain types, in which case the ledger and codec gates could fold
into the schema comparison, or if the wire moves to a schema registry, in which case the registry's history replaces the
base selection above.

### The package name is a namespace, not the major

The schema's package name, `felis.v1`, is a namespace rather than a mirror of the protocol major, and it moves only when
a daemon has to hold two schemas at once. That is what a deprecation window needs: two complete message sets in one
binary, which is what a `felis.v2` package provides and a renumbered `felis.v1` cannot. Minor growth stays inside the
package either way.

### Build identity is not wire compatibility

`Welcome.identity` marks the boundary the policy deliberately leaves open: identity of build and compatibility of wire
are separate facts with separate jobs. The daemon's build rides along for `felis version` to display, and no peer gates
on it, since a gate on it would be the exact-match-on-the-build failure above. It is a typed `BuildIdentity` rather than
a string because three processes compare these values, and a shape each one scrapes out of the other's prose is a shape
they can disagree about.

The handshake is therefore the **only** place a connection learns what its peer is, and every later message carries live
state alone. `Ops::StatusReply` carries no build string and no wire pair: `felis daemon status` composes what it prints
from the connection itself, taking the build from `Welcome.identity`, the major from the accepted preface, and the
daemon's own minor from the accept's second word.

Rejected: a second copy of all three in the reply, so that a reader of one message has the whole report. The reply is
read on a connection whose handshake the same client conducted, so the copy names nothing the client does not already
hold, and two answers that nothing on the wire forces to agree leave a client with no rule for which to believe. The
daemon's own minor is not the effective minor and cannot be substituted for it: against a daemon newer than the client
the two differ, and the report is about the daemon.

### Breaks before and after the compatibility freeze

Before the compatibility freeze the dev wire carries no promise at all, so a break rides major 1 unbumped rather than
burning a number per break; nothing that could be stranded exists yet. The freeze is the first final release, whose
schema becomes the baseline the tag build compares against. After the freeze, a major owes a deprecation window in which
the daemon serves both message sets, which is what makes pooling worthwhile: pending semantic breaks land as one major
rather than several, because each one costs its own window.

_Revisit if_ a downstream client gains users of its own before felis itself is published, which is publication in
everything but name, and brings the freeze's obligations with it.

## The frozen frame header

The frame header is a 6-byte `len | kind`, with `len` counting `kind` as well as the body, arithmetic that would read
more plainly as a body length. It carries nothing else. A per-frame sequence number is the field a framing layer
accretes by default, and felis has no receiver for one: the stream layer already guarantees order, so a counter would
only be a reliability assertion no code asserts on, at four bytes per frame on the hot row path. The one thing a peer
acts on is typed instead, where the compiler sees it (`InputMsg::NextGridFrame` carries the client's grid-frame pull),
so it does not depend on a header field a future reader might mistake for a delivery guarantee.

## Pull pacing carries no acknowledgement position

`InputMsg::NextGridFrame` is payload-free: it arms the next dirty cycle and says nothing about what the client has
already applied. A pull that races a frame still in flight is therefore not detectable as redundant, and does not need
to be. `ship_sub_at` composes its diff off the per-subscriber dirty state, so a pull with nothing new emits nothing; the
redundant frame the acknowledgement position would suppress never gets composed in the first place.

A dedup that did act on such a position would need the session task to tally, per subscriber, the grid frames it has
handed the write pump, the only counter comparable with a client's own tally. Until that counter exists, an
acknowledgement field on the wire is a number no code can read, so the pull carries none. _Revisit if_ the task starts
keeping that per-subscriber tally.

## Row codec: why a hand-written RLE payload

The `packed_cells` row codec run-length encodes attributes rather than serializing them per cell, and its bytes are
written by hand against a published specification, [Row codec](../../reference/row-codec.md), rather than derived from
the Rust types. Two decisions, argued in that order below. The blob rides as an opaque `bytes` field of the protobuf
`RowDelta`, a grid-internal packing the protocol crate never interprets, so both arguments are independent of the
envelope encoding.

Attributes are run-length encoded because serializing them per cell is the daemon's single largest emit cost: a samply
profile of `daemon_side/plaintext_scroll` under a per-cell codec puts ~44 % of self-time into serializing `Color` /
`Attributes` / `Cell` (an 80-column `cat` line pays the three-`Color`-plus-flags serialize 80 times for one pen), and
the plaintext stream is daemon-produce bound, so that serialize is the binding constraint on throughput. A uniform row
collapses to a single attribute run, so the pen serializes once per row; the per-column grapheme stream stays, but a
grapheme record is a cheap 1–2 bytes that never re-serializes the pen.

Worst case, a row that alternates pen every column produces one run per cell, larger than a flat body, but real terminal
output holds runs even in heavily-colored TUIs; if a workload ever defeats RLE as a measured regression, the fix is a
flat-body codec version as fallback.

Rejected against RLE:

- **Trailing-blank trimming.** The shadow learns post-resize widening from incoming column counts, and a wide-but-blank
  row trimmed to zero cells would hide the new width.
- **A cheaper manual `Attributes` serialize.** It still pays per cell, where RLE pays per run.

Maximal runs stay a producer rule rather than a decoder invariant ([Row codec](../../reference/row-codec.md) "Canonical
form"): enforcing them on decode would turn another implementation's failure to coalesce into an interop break rather
than a size cost.

### Why the bytes are hand-written, not derived

Carrying the RLE shape on a serde format instead (postcard, say) is the cheaper-looking option, on the argument that a
derive-checked byte layer costs nothing and a hand-rolled codec merely re-implements varint and struct framing in both
directions. It is rejected for three reasons, all about the contract rather than the framing:

- **A derived layout is not a specification.** The bytes would be whatever serde and the format made of the Rust type
  declarations, so a client in another language would have to read `felis-grid`'s source and infer the format's framing
  to implement a row reader, which contradicts the reuse-path promise the protobuf envelope exists to keep
  ([overview.md "Reuse paths for non-Rust clients"](overview.md#reuse-paths-for-non-rust-clients)). The hand-off is only
  credible if the interior of the one opaque field is specified too.
- **A derive is not a contract.** A field reorder or a type swap in `felis-grid` moves the wire silently, with nothing
  to notice: the layout is a side effect of Rust declarations that no reviewer reads as wire.
- **A serde decoder leaves real holes.** `postcard::from_bytes` discards a trailing tail rather than rejecting it, a
  length prefix is not bounded before the allocation it drives, and a "version" exists only as an enum discriminant no
  reader can name. Those are the three properties [Row codec](../../reference/row-codec.md) makes normative.

Writing the bytes by hand is not the more expensive path either: daemon-side emit measures 18–29 % below the derived
path across all three input shapes (criterion `--quick`, one machine), which matters because the RLE decision itself was
made on a bench. The cost is that the layout has to be maintained deliberately, which is the point, and is what the
golden vectors and the fuzz target guard.

Also rejected: publishing a serde format's framing as the spec instead of writing the bytes. Postcard is a stable
format, but the spec would then have to describe how serde maps Rust enums and `Option`s onto it, so the Rust
declarations stay the real authority and the silent-reorder hole stays open. And a schema-driven inner codec, a nested
protobuf message, is measured and rejected below, on the client's decode path.

_Revisit if_ an off-the-shelf format arrives that is both specified independently of a Rust type and as cheap on the
emit path; the argument above is about the contract and the decode holes, not about byte-shuffling code being
intrinsically better.

### The tooling gap this leaves

The envelope's field-numbered compatibility stops at this `bytes` field. `buf breaking` sees a `bytes` field, not the
row payload inside it, so no schema tooling reaches the cell shape: adding a `Cell` attribute or reshaping the payload
is a layout change a peer on an older codec version misreads. Such a change rides the codec's own version tag and the
minor that authorizes it ("Schema evolution" above), with no `buf breaking` safety net to catch a forgotten bump. The
spec's golden vectors, the layout pins in `wire.rs`, and the row round-trip tests are the guard instead. Keeping cells
opaque trades that tooling coverage for the hand-tuned RLE packing above.

Schema-governed cells have a measured price rather than a hypothetical one. A spike swapped the row codec to a native
protobuf `Row` / `Cell` model (field-numbered, so `buf breaking` would guard it) and ran the end-to-end benches. The
encode side is free: the daemon's emit path (`daemon_side` in `felis-client-core`'s `end_to_end_throughput` bench) moves
within noise, because parse, grid-diff, and framing dominate the producer.

The decode side is not, because the row is decoded on every client's consume hot path where nothing else hides it:
`client_consume` regresses about 140 %, the full single-cycle round trip 40–60 %, the batched production path 13–16 %,
and each row ships 1.4–1.95× more bytes. Native cells would leave the producer flat and make every attached client
slower to mirror the grid. A JSON-only row is worse still: proto3-JSON base64-inflates the per-cell data to roughly
11–13× the bytes and 10× the decode. So the compact RLE payload stays until cell churn makes schema-governed cell
compatibility worth that client-side cost.

_Revisit if_ cell-shape changes turn frequent enough that maintaining the codec version by hand costs more than that
measured client-side price.

## Open extensibility considerations

Non-GPU clients ship today: `felis-tui` renders in a host terminal, `felis-web-component` in a browser, `felis.el` in
Emacs ([overview.md "Reuse paths for non-Rust clients"](overview.md#reuse-paths-for-non-rust-clients)). Rendering is not
negotiated for correctness: a client that cannot render a payload drops it client-side, and the daemon never down-maps
color or sizing, which would put presentation in the daemon and breach principle 3
([the resulting facts are in the reference](../../reference/ipc.md#versioning)).

The optimization is the open question: a suppression flag, one for animation frames and a possible graphics-pixel
sibling, so the daemon stops shipping heavy or ongoing payloads a client will discard. It is gated on a measured
cross-host saving, because the same-host case has no shortcut that makes it free: every window subscriber receives its
own copy of the pixels over its socket as `ImageMsg::Chunk` frames
([overview.md "No shared memory"](overview.md#no-shared-memory)), so a same-host client that discards them still pays
for the transfer.

Where a re-encoding client gets its ANSI is settled rather than open: the daemon emits structured `RowDelta` messages,
never raw VT byte streams, so a client painting into a host terminal reconstructs the escape sequences from grid state
through the shared `felis-grid::ansi`
([overview.md "Shared wire knowledge across the satellite clients"](overview.md#shared-wire-knowledge-across-the-satellite-clients)),
not a per-client emitter.
