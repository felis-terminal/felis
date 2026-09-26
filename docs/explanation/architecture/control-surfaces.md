---
title: Control surfaces
sidebar:
  order: 3
---

"Drive the daemon" is not one surface in felis; it is five, layered over the wire protocol. This document names the
planes, states the criterion for placing a new capability, argues the diagnostic and stop verbs, the conventions the
surfaces share and the configuration architecture, and records which coverage asymmetries are deliberate. It is an index
over the per-surface owner docs, not a replacement for them: a change to (say) the keymap grammar still lands in
[config.md](../../reference/config.md). The cross-plane name map itself (which word each surface uses for each concept)
is the lookup table in [reference/control-surfaces.md](../../reference/control-surfaces.md).

## The five surfaces, over one wire substrate

| Surface              | What it is for                                                                                                                                                                 | Owner doc                                                                                                                                 |
| -------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------- |
| **Daemon CLI**       | Process / lifecycle of the daemon itself (`felis-daemon serve \| relay`, plus `--version`)                                                                                     | `crates/felis-daemon/src/main.rs`                                                                                                         |
| **Client CLI verbs** | Unattended / scripted automation with no window: `felis sessions …`, `felis notifications subscribe`, `felis completions`                                                      | `crates/felis-cli/src/main.rs` (the `felis` front-door), [ipc.md](../../reference/ipc.md) "CLI clients", [cli.md](../../reference/cli.md) |
| **Config file**      | Persistent, declarative, per-user preference (client-only): `[font] [theme] [clipboard] [window] [cursor] [mouse] [shader] [keymap]`, plus per-client `[client.<id>]` overlays | [config.md](../../reference/config.md)                                                                                                    |
| **Keybind actions**  | Interactive, in-the-moment control of the focused window: `[keymap]` → `Action` / `IpcAction`                                                                                  | [input.md](../input.md)                                                                                                                   |
| **Environment**      | Child-process / daemon identity, where the daemon reads no TOML: `FELIS_TERM`, `FELIS_TERM_PROGRAM`, `FELIS_SESSION_ID`, `FELIS_SOCKET`                                        | [terminal-identity.md](../../reference/terminal-identity.md)                                                                              |

Underneath them all is the **wire protocol** (`felis-protocol`, [ipc.md](../../reference/ipc.md)): the typed message
substrate every surface is a thin wrapper over. A capability exists on the wire first; the surfaces expose it.

## The placement criterion

When a new capability appears, route it by intent, not by convenience:

- Is it for a caller with **no window** (a script, a CI runner, an editor bridge)? → **Client CLI verb** (typed args,
  stable `--format` machine output, one daemon connection per invocation; [ipc.md](../../reference/ipc.md) "CLI
  clients").
- Is it a **persistent, declarative preference** the user sets once? → **`config.toml`** (client-side, reload-able,
  principle-7-closed).
- Is it an **interactive action** tied to a keystroke on the focused window, meaningful only while attached? → **keybind
  action** (closed `Action` / `IpcAction` enum; the set grows by variants, never by accepting a string).
- Does it concern the **child's environment or the daemon's identity**, where the daemon (which reads no TOML,
  [terminal-identity.md](../../reference/terminal-identity.md)) must see it? → **environment variable**.
- Is it a **new daemon operation** itself? → add the **wire message** first, as an arm the connection mode admits; the
  surfaces above wrap it. Never let a surface carry behavior the wire cannot express.

The hard line (principle 1): every surface carries **typed data**, never expressions, callbacks, or shell strings. There
is no `sessions eval`, no keybind DSL, no config hook.

## The naming rules the map encodes

The cross-plane table lives in [reference/control-surfaces.md](../../reference/control-surfaces.md); these are the rules
behind its word choices.

### `kill` on the user surfaces, `Destroy` on the wire

The keymap's `kill_session` and the CLI's `kill` speak the unix idiom the user reaches for (cf. `tmux kill-session`),
while the wire speaks object lifecycle. So `OpsToDaemonMsg::Destroy` sits in a Create/Destroy/Attach/Detach set matching
protocol convention (X11 `DestroyWindow`, Wayland `destroy`; docker splits `kill` = send a signal from `rm` = remove the
object), and `Destroyed { resolved }` reads as an object-removal report.

Renaming the wire to `Kill`/`KillSession` to land one word across all four layers is rejected. The Create side cannot
align anyway (keymap `new_session` vs CLI `spawn`), so the rename buys only partial alignment while dropping a lone
process-idiom word into the wire's lifecycle vocabulary. A wire rename moves no bytes (a field's identity on the wire is
its number), so it costs a codec regeneration rather than compatibility, which is itself why the wire need not chase
user-surface idiom.

### `new_session` and `spawn` are different words

`new_session` (keymap: a new session in _this_ window, tab-like) and `spawn` (CLI: detached pre-warming, cron / script
intent) are deliberately different words, because they are different operations; one root word for both would hide the
difference.

### `pipe` and `run` are two operations

`pipe` and `run` are two operations, not one. `pipe` (`RegionToDaemonMsg::Request`) routes a _region_ to a sink (command
/ clipboard / file / paste); the region is the subject. `run` launches a _command_ with no region; the command is the
subject. They share the client-side transient spawn when the sink is a command, but stay two action tokens so neither
name lies about half the space (a `run` never has a clipboard target; a `pipe` never has an empty source).

Only `pipe` talks to the daemon that holds the grid: with no region to read, `run` needs nothing from that daemon, so it
goes straight to `SessionToDaemonMsg::Create` on the client's own daemon. The transient's lifecycle, the earned use and
the "not recommended but not restricted" stance on unrelated commands live in [input.md](../input.md) "Action mapping".

## The two disconnect surfaces

Disconnecting a window from a session exists on two planes, and they differ in scope, which is why they do not share a
word. One word for both would have to mean "this window lets go" in the keymap and "throw everyone off" on the CLI,
leaving a reader of the CLI form unable to tell whether a colleague's mirror is about to go dark. The all-subscriber
form is **`evict`**, named for its effect; `detach` is the one-window key action's name and nothing else:

- **CLI eviction** (`felis sessions evict <id>`) disconnects _every_ client streaming the target (a hygiene operation
  that parks the session, which keeps running) via the `OpsToDaemonMsg::ForceDetach` / `Detached` / `PushMsg::Evicted`
  wire trio, owned and specified in [ipc.md](../../reference/ipc.md) (eviction `Notify`, exit codes). The wire keeps its
  descriptive `ForceDetach` name: a protocol name need not equal the UX word, and the push it fans out is `Evicted`.
- **GUI self-detach** (the `detach` chord) sends `SessionToDaemonMsg::Detach` (the same wire step a window close takes)
  and closes the window; the session survives ([session-lifecycle.md](session-lifecycle.md) "Detach"). An inbound
  `PushMsg::Evicted` (the daemon evicting this client on another process's behalf) closes the window the same way.

The chord detaches _this_ client from _its own_ session; the CLI verb evicts _every_ client.
[ipc.md](../../reference/ipc.md) draws the `SessionToDaemonMsg::Detach`-vs-`PushMsg::Evicted` line on the wire. Attach
is additive ([session-lifecycle.md](session-lifecycle.md) "Same-user mirroring"), so `sessions {send,capture,search}`
work against attached sessions without disconnecting anyone; eviction is a hygiene op, not a prerequisite for the
scripting verbs.

## Diagnostic verbs

felis carries three verbs whose subject is felis itself rather than a session: `daemon status`,
`config path|check|show-effective`, and `doctor`. They are grouped here because the placement criterion above routes
them all to the client CLI, and because what separates them is _what each is allowed to touch_.

### `daemon status` is not optional

`daemon status` exists because a ceiling nobody can observe is a ceiling nobody can act on. The daemon enforces
process-wide ceilings (a connection cap, a session cap, and accounting for image-store bytes, in-flight decodes, and
subscriber queues). An operator meeting `SessionLimitReached` or an `AtCapacity` refusal at connect has to be able to
ask what the limit is and what is holding it, and a daemon that answers only "refused" leaves them guessing between a
runaway script and a limit set too low. Caps without the report would make felis harder to operate than no caps at all.

#### A row names its dimensions

A row names its dimensions rather than leaving them to be inferred. Some ceilings bound one session or one subscriber
and some bound the daemon; some observations sum every subject and some are the deepest single one. So a row carries
them by name: `total_used` beside a `scope` arm holding `global_limit`, and, where subjects exist, `subject`,
`max_subject_used` and `per_subject_limit` in the same arm. Every ratio a reader can form then has both halves in one
denominator.

The rejected alternative is one `used` and one `limit` with a `scope` label saying which denominator each is in. It
hands the reader the disambiguation on every row, and it hides a second ambiguity the label does not even mention, since
`used` under that shape is a sum for a per-session resource and a maximum for a per-subscriber one. A qualifier the
reader must notice, on a report whose whole purpose is to be read under pressure, is a trap rather than a contract.

Why the wire carries the scope as a union arm rather than a label beside optional fields, and why a ceiling is a `Limit`
union rather than a `0`, is recorded in [ipc.md](ipc.md#a-status-reports-scope-is-a-union) with the rest of that rule.

A ceiling felis does not budget is `Unlimited` on the wire and an omitted key in `--format json`, never `0`. A consumer
that reads a missing key as `0` turns "no ceiling here" into "nothing is permitted", so the reading that looks safest is
wrong in both directions: it hides a real ceiling behind an imaginary one, and it reports a daemon that permits nothing
while it is serving.

#### A deepest-subject sample, not a per-subject table

Naming _which_ session holds the bytes would want a row per session, and a table here would grow with the pool while
this report is about the daemon's ceilings. What the deepest subject buys is the one number the total cannot give: how
close _some_ subject is to its ceiling, and therefore whether the pressure is one runaway session or the pool as a
whole. It does not name that session: `felis sessions list` inventories the candidates but measures none of these
resources, so the id is nowhere today.

_Revisit if_ an operator turns out to need it: at that point the row would carry a subject identifier beside the sample,
not a table.

#### What earns a row

Every row is a number that can _disagree_ with a ceiling, which is what decides whether something earns a row at all.
Grids do not: a session owns its primary and alternate screens as one fixed set, so the row could only ever restate
`sessions` under a second name, and a report whose purpose is to show real numbers is worse for carrying one that cannot
move on its own. The runtime's worker count does not either (it is fixed at startup, so the observation and the ceiling
would be one number printed twice), and it rides the reply's identity fields beside the build instead. Either becomes a
row the day it can move independently.

#### The report is a sample, not a set of counters

Every number is read off the live pool and the session actors as the reply is built, which costs one round of actor
messages per status call and nothing at all on the hot paths. Maintained counters are the rejected alternative: they buy
accuracy felis does not need (nobody watches this at frame rate) at the price of an increment in every image insert,
every outbox push, and every reassembly step, and a counter that drifts from the state it describes is worse than no
counter, because it looks authoritative.

#### The limits are compiled in

The limits themselves are compiled in, because **the daemon reads no configuration file at all**: the config surface is
client-side by design (the table above), and the daemon's own knobs live in a struct its binary builds. Inventing a
daemon config file to make one number movable would be a larger decision than the number deserves, and what the report
is for is accounting, not tuning. So v1 reports a ceiling it cannot move; the connection cap sits on the same compiled
struct rather than behind a `--max-connections` flag.

_Revisit if_ a knob wants _tuning_ rather than reporting: compiled constants with a report are honest, a knob users are
told to change without a place to change it is a config file denied.

### The config verbs never dial the daemon

`config check` answers "would felis accept this file", and the file is read by the _client_: the daemon has no opinion
about `font.size_px`. Routing the check through the daemon would make a pure local question depend on a running process,
which means the one situation where a user most wants to check their config (felis will not start) is the one where the
check cannot run. It also keeps the exit codes honest: `2` means daemon-unreachable everywhere else on this surface, and
a verb that never dials can never produce it.

#### One flag selects the file, and nothing else does

`felis --config PATH` exists because a one-off launch otherwise cannot be reproduced without editing the real profile,
and because it is the only way the Windows config and doctor suites can isolate a temp profile: the known-folder API
that resolves `%APPDATA%` reads no environment variable, so without the flag those suites run on Unix alone.

Two other ways to select the file are rejected:

- **An environment variable.** It is the obvious alternative and the one felis will not have. felis stamps the spawning
  environment into every session ([terminal-identity.md](../../reference/terminal-identity.md)), so a `FELIS_CONFIG` set
  for one launch would be inherited by every shell inside that window and would silently redirect the `felis config`
  runs a user makes _in_ it, a leak a per-invocation flag cannot have. A second spelling would also have to be kept in
  agreement with the first forever, and the precedence between them documented, for a knob whose whole value is that it
  is unambiguous.
- **A config key naming another config file.** Rejected on principle 1: that is an evaluator in the config format, one
  file deciding which file is read.

Two consequences follow from making it a flag rather than a discovery tweak. A relative path is resolved against the
invoking process's working directory before anything else sees it, because the window it launches runs somewhere else
entirely and could not recover what the user meant; only the absolute form crosses the exec. And a selected file that is
absent is an **error**, where an absent file at the discovered path stays the use-the-defaults case: discovery is a
guess felis made and a first run is its normal outcome, while a path the user typed carries an intent that silence would
answer with facts about a different document.

_Revisit if_ a multi-profile workflow appears that a per-invocation flag cannot serve. A named-profile surface would be
a different decision, not a second spelling of this one.

### `doctor` execs the frontend rather than linking it

Two of `doctor`'s checks (a GPU adapter and a clipboard backend) can only be answered by the code that owns them, and
that code is wgpu and arboard. Linking either into `felis` would end the GPU-free front door: the whole point of the
split is that a container, a CI runner, or an SSH session gets the verb surface without the graphics stack. So `doctor`
runs the frontend binary in a machine-output probe mode and merges its JSON report, reusing the exec seam bare `felis`
already uses to launch a window. A frontend that is not installed is a _degraded_ report (the probe rows say so), not a
failure, because "this is the headless build" is a true and useful answer to "is my felis healthy".

#### `doctor` never exits `2` for an unreachable daemon

`doctor` is the one dialing verb that never exits `2` for an unreachable daemon. Everywhere else that code means "I
could not run"; here a dead daemon is the finding, and the single command written to diagnose one must not answer by
refusing to report. So it borrows `config check`'s grep shape instead (`1` when a check failed, `0` otherwise, the
checklist on stdout either way), and the daemon row carries the condition as prose. A missing daemon is not even a
failed row: no daemon is the normal state before the first window.

## Stopping the daemon

Stopping the daemon is a control verb rather than a signal, because a signal cannot do the two things this operation
needs. It cannot be _refused_: SIGTERM either lands or does not, so a caller who wanted "only if nothing is running" has
to check the pool first and race whoever creates a session next. And it does not exist on one of the supported targets:
the daemon is built and gated on Windows, where `pkill` and SIGTERM are not available at all, so a lifecycle documented
in signals is a lifecycle documented for two of three platforms. A typed verb over the existing IPC surface reaches the
daemon the same way `--host` and `--socket` already reach it, which the same signal would need an SSH command and a
process name to imitate.

### The default refuses rather than destroys

Stopping the daemon ends every session it holds, because felis persists no session state
([session-lifecycle.md](session-lifecycle.md)). A verb whose bare form destroys work is one an operator must be careful
to type correctly; the default therefore stops only a daemon that is holding nothing, and otherwise reports how many
sessions stand in the way. Destruction is available, spelled `--force`, and the reply names the mode it took so neither
a human nor a script has to infer which of the two happened.

This is the read-side "no silent resurrection" posture in the other direction: the headless verbs never start a daemon
behind the user's back, and the stop verb never ends their sessions behind it.

### Three modes on one verb, not three verbs

`stop`, `stop --force` and `stop --when-empty` all end the same daemon and differ only in what they do about the
sessions in the way, so they share a name and the flag says which posture. Separate verbs would put the destructive one
a tab-completion away from the safe one under a different name, and the mutually exclusive flags are what makes "destroy
everything" and "wait for everything" impossible to ask for at once.

### Draining is admission state, not a separate flag

The daemon already decides every create under one lock, counting registered sessions and the reservations creates hold
before they register; the drain sets a flag inside that same critical section. That placement is the guarantee: "the
daemon is empty" and "no further create is admitted" are decided together, so a create cannot slip in behind a stop that
was already answered, and a create still in flight holds the drain open until it either registers or fails. A flag
checked outside the admission path would leave exactly the window the verb exists to close.

Draining is the first half of an idle-daemon exit ([overview.md](overview.md) "Daemon process lifecycle"); the daemon
still never exits on its own.

## Shared conventions

Where two surfaces or two verbs express the same concept, they express it the same way. Each convention is sourced in
its owner doc; what this section keeps is the decision behind it, so a new verb or wire message can be checked against
one list. The conventions fall into four groups: when a verb may reach or start a daemon, the machine-output contract,
the bridge's own bounds, and how identifiers and text are spelled. The [asymmetries below](#deliberate-asymmetries) are
the exceptions.

### Reaching the daemon

#### A session verb never spawns a daemon implicitly

For automation an absent daemon is almost always the real bug, and a script that silently resurrects a fresh daemon and
reports "no sessions" has masked it, so a cold socket exits `2` with "no daemon at \<path\>" instead. The exceptions are
the forms whose intent _is_ a live daemon: `sessions spawn`, since creating a session implies wanting a daemon to hold
it, and the window launches (bare `felis`, `felis attach`).

What decides is the verb's intent, never which carrier carries it: the remote autospawn decision lives in
`felis-daemon relay`, so a read or drive dial appends `--no-spawn` and a cold remote socket fails like a cold local one.
The local policy (spawn-then-retry) lives in `felis-client-core` (`connect_or_spawn_daemon`); the per-verb contract is
in [cli.md](../../reference/cli.md) "Auto-spawning".

A retarget is the one form that dials twice, and the two halves take different sides of this split: the source dial
drives a daemon that is already holding the window, while the landing on the target host is a window asking for a daemon
to hold it. So the landing spawns over SSH, and on a cold local socket it fails instead, because `dial_and_land` opens
that socket directly rather than through `connect_or_spawn_daemon`.

#### A diagnostic names what it found and changes nothing

`doctor`'s `daemon-sibling` row exists because nothing on the wire identifies a daemon instance: two endpoints can only
be reported as two endpoints. So the row states what answered the default endpoint and how to inspect it, and never
claims the two are separate daemons, never recommends stopping one, and unlinks nothing. The row itself is in
[cli.md](../../reference/cli.md#doctor).

### Machine output

#### `--format` selects the framing by the verb's class

**`--format`** selects the framing, and the _verb's class_ decides which values it accepts (the per-verb classification
is normative in [cli.md](../../reference/cli.md) "Machine output"). Making the class explicit is what buys the rest of
this group: once "point" and "stream" are words the surface knows, the channel rules, the terminal rules, and the error
shape all follow from the class instead of being decided per verb.

Three spellings of the flag are rejected:

- **A bare `--json` that means _both_** (one object on `kill`, a line-per-object stream on `capture`), with only the
  source telling which. A consumer cannot learn from such a flag whether to read one parse or a loop, and every new
  machine-output verb re-litigates the question.
- **A `--json` beside `--jsonl`.** Two flags for one decision; a verb would have to reject one of them anyway, so the
  ambiguity survives with more surface area.
- **`--json` as an alias for the right value per verb.** It would freeze the very ambiguity the class exists to remove;
  an alias, once shipped, can never be withdrawn within the epoch.

`--json` is therefore not a flag at all: it is an unknown argument (clap usage error, exit `2`), which names the mistake
instead of guessing at the intent.

#### A verb's source puts it in the stream class

What puts a verb in the stream class is its _source_, not the shape of its answer. A stream frames something unbounded
or arriving incrementally, which `capture`, `search` and `notifications subscribe` are; the session roster is bounded
and answered in one frame, so it is a point result with the whole array inside it.

Framing the roster as a stream is the rejected alternative. It obliges every consumer to accumulate lines and watch for
a terminal to learn a thing the daemon already knows in full, and `felis bridge` answers it as one object regardless, so
the two framings of one roster would disagree.

_Revisit if_ the roster ever becomes incremental (a `list --watch` reporting arrivals and departures), which would be a
stream verb of its own rather than a reframing of this one.

#### A proposed verb names its class before it is built

A proposed verb names its class before it is built, because the class belongs to the verb and never to one answer's
payload. A verb whose answer might grow large is a stream. The rejected shape is a verb that answers in one object when
the payload is small and streams it when it is large: its consumer then needs both parsers and both failure channels,
and learns which applies only from the output it is trying to parse.

A stream proposal also names its terminal: what the clean terminal's `count` counts, and which failures end the stream
in the failing terminal. A verb is exempt only when it has nothing structured to report or a fixed framing of its own (a
window launch, `completions`, `bridge`); exempt is not a way to defer the choice. A proposal with no class, or a stream
with no named terminal, is not reviewable, since the first implementation would otherwise decide the framing every
consumer inherits.

#### The class fixes the channels

The class also fixes the channels, and each half answers a consumer's problem. A point verb's failure is exactly one
typed error object on stderr, so a consumer parses stdout unconditionally and never has to distinguish "a result" from
"an error that happens to be JSON". A stream writes _every_ protocol object to stdout, because a piped consumer that
watches only stdout must still see a lag marker or a mid-stream failure.

The mutators earn the stable contract for the same reason the read verbs do: a picker that spawns or relabels a session
then feeds the id straight back into the next verb, so the id-bearing shape has to be as parseable as a roster row. The
coverage rule is the wire, not the verb's mood: where the daemon answers with a typed reply the human output would
otherwise discard, the result object carries the reply's own fields, with every id the _resolved_ full hex.

#### Frozen for `v:1`

The two words, the class binding, the one error object, the `kind` vocabulary with its exit codes, and the two
transports hold for the epoch; growth inside it is a new token or a new field ([cli.md](../../reference/cli.md) "Machine
output"). What fixes the transports is the process model rather than taste: a one-shot point verb has an exit status and
a stderr to answer with, a stream is a stdout protocol its consumer reads until the terminal, and a multiplexed bridge
has neither an exit status nor a stderr per request, so an `id` on stdout is the only channel that can carry a
correlated answer.

Unifying them fails in both directions. Moving the CLI's errors onto stdout breaks `$(felis sessions spawn …)` and every
shell contract built on capturing a result; handing the bridge per-request exit codes asks for a status a process has
once.

#### The published schemas are generated from the serde types

A hand-written bundle is the rejected alternative, and prose is the evidence against it: a hand-written reference page
drifts from the structs it describes. Generating keeps a field's schema and its serialization one edit apart, and makes
staleness a failing test rather than a reader's discovery.

The cost is that JSON Schema cannot state the envelope (which body may appear under which key, and that `"v"` is a
literal), so that layer is composed by hand around generated bodies; it is also the layer serde has no type for, since
the CLI and the bridge wrap the same bodies differently. The bridge's per-operation request parameters are the other
exception: the binary parses them by key rather than through a `Deserialize` type, so they are explicit schema types
held to the runtime's parameter table by a guard test.

**`error.kind`** is published as an open string with the epoch's tokens in an `x-known-values` annotation. An `enum`
fails: the vocabulary grows additively within an epoch, so a consumer pinned to an older bundle would reject an object
it is meant to fall back on.

A request parameter whose vocabulary is finite is enumerated in the published grammar rather than typed as a bare
string, so the schema refuses what the bridge would refuse and a line the bundle admits is never answered
`malformed_request` for one of those values. Syntax is all a schema can carry, so a parameter constrained by pattern
rather than enumeration keeps a residual gap.

A 64-bit bound keeps a second gap, in the validator rather than in the grammar: the bound is published exactly and
annotated `x-bound-exceeds-f64-precision` rather than narrowed to what a validator can enforce, because a validator
compares JSON numbers as `f64`, so an enforceable bound would sit below `u64::MAX` and reject counts the daemon really
emits. A few thousand values slipping past a bound nothing produces is the cheaper failure
([cli.md](../../reference/cli.md) "JSON Schema").

_Revisit if_ `"v"` ever moves, which closes the `error.kind` vocabulary for the epoch it leaves behind.

#### One rule decides where a refusal of the invocation lands

A refusal the argument parser can state is the parser's: a human message on stderr with exit `2`, since at that moment
no framing has been chosen. Every refusal after a successful parse is emitted in the verb's class framing, under the
kind `usage`. That leaves the interactions between a root flag and a subcommand that clap cannot express: a global
carrier flag on a verb addressed to a window rather than to a daemon, `--config` on a verb that reads no config file,
and a `--config` selection that is missing or unusable on a window launch, refused before the exec because the frontend
would otherwise open a window on the built-in defaults and exit `0`.

The rejected alternative was to teach each hand-rolled refusal to speak JSON while leaving the rule implicit: a consumer
would then have to know, refusal by refusal, whether stderr held prose or an object.

The same rule is why a machine format silences felis's own console log: even the `WARN` a failed systemd hand-off emits
would make a point verb's "exactly one error object" two lines. `RUST_LOG` still wins, so the diagnostic is one variable
away, and the failure's own detail is in `error.message` rather than only in the log.

_Revisit if_ clap grows a way to state a root-flag/subcommand conflict, which would empty the post-parse case and let
the rule collapse to "usage errors are human".

#### The exit code is a function of `error.kind`

The exit code is a function of `error.kind`, not of the call site: a typed refusal, meaning the operation ran to an
answer on the merits, exits `1`; a failure to ask at all (a bad invocation, an unreachable or broken peer) exits `2`.
Per-site literals are the rejected alternative: they let one condition exit `1` on one verb and `2` on another, so
`spawn`'s `invalid_request` and `capture`'s refused stream would tell a script "felis could not ask" when the daemon had
answered. The kinds are listed with their codes in [reference/cli.md](../../reference/cli.md) "Exit codes", and the list
is checked against the map it is written from, so the table cannot drift from the code that decides.

The Point-diagnostic verbs (`config check`, `config show-effective`, `doctor`) read `1` as "the diagnostics the verb was
asked for". `config check` and `doctor` reach it without an error object at all, so they are an exception to the code
being a function of `error.kind`, beside the two answers that are empty rather than refused (a `sessions search` with no
match, a `notifications subscribe --once` stream the daemon ended first); `config show-effective` still fails with
`invalid_request`, because the defaults it would otherwise print are not the effective config.

Folding the class into the refusal kinds is rejected: `no_match` on a config document would tell a script the daemon
refused something when the verb succeeded at the job it was given. The class is named in the reference instead, where a
reader meets it beside the verbs it covers.

#### `error.kind` grows additively within an epoch

`error.kind` grows additively within an epoch, so a consumer branches on the tokens it knows and falls back to the exit
code for the rest. A closed set was rejected: every new daemon refusal would then cost an epoch bump, which invalidates
every other consumer's parser over a token they will never see, and the epoch is one number for the whole surface. A
token's spelling still never changes within an epoch: additive means new tokens, not renamed ones.

#### Every top-level machine object carries `"v":1`

`"v":1` versions the _CLI output contract_, never the daemon wire: a wire minor must be invisible here, or a consumer
would re-negotiate for growth it cannot see. `felis bridge` stamps the same epoch on its own envelope, so the two
framings of one vocabulary move together. Moving the number costs every consumer at once, which is why fields may be
added but never renamed or retyped without it.

It is an epoch, not a negotiation. Two other shapes are rejected:

- **A `--format-version` request flag**, or any handshake where the caller states what it can read. A one-shot verb has
  no round trip to negotiate in, and a negotiated surface obliges felis to keep serving every epoch it ever spoke: the
  daemon wire pays that cost because a long-lived connection must span client and daemon upgrades, while a CLI
  invocation begins and ends inside one binary. A stamp a consumer _reads_ costs one field and lets a script refuse an
  epoch it does not know; a version it _asks for_ is a compatibility promise with no end date.
- **Stamping only the terminal.** A consumer would have to buffer a whole stream before it knew how to read the items it
  had already parsed.

#### A stream closes with exactly one terminal object

A stream verb closes with exactly one terminal object, and every stream spells its count `count`: the field names the
terminal's job, not the payload's noun, so a consumer closing out any stream reads one shape. Per-verb spellings
(`matches` for search, `rows` for capture) are rejected: they force every generic reader to branch on which verb it
drove, for no information the terminal does not already carry. `rows` appears elsewhere as a _grid height_ (`list` /
`info`), a different key on a different object.

Clean and failing are _different_ objects rather than one `done` envelope with an optional `"error"`, so a reader
branches on the event it already has to read rather than on a field's presence.

A stream emits its terminal even when it fails **before its first item** (a refused open, an unreachable daemon). A
human line on stderr and nothing on stdout would read identically to a stream that legitimately had nothing to say; a
consumer blocked on the terminal could not tell them apart without a timeout. The wire mirrors this: whatever the
family, a stream ends in exactly one typed terminal naming the stream it closes ([ipc.md](../../reference/ipc.md)
"Correlation, requests, and streams"; the two verbs' own records are in [scrollback.md](../data-model/scrollback.md)
"Search", "Capture").

### The persistent bridge has its own aggregate bounds

The daemon's per-connection and per-stream limits do not bound an editor helper that can open many connections and
retain output for a reader that has stopped, so the persistent bridge carries aggregate bounds of its own. The ceilings
are in [cli.md](../../reference/cli.md) "`felis bridge`"; two rejections shape them, and a rejected implementation shows
what they prevent:

- **Sharing one queue budget between items and terminals.** Unread items could then occupy every slot while every
  accepted operation still owed the caller a terminal, so terminal capacity is separate and reserved at admission, and
  lives through the flush.
- **Releasing a link's permit at its stream terminal.** Replacement links could then outrun teardown and escape both
  aggregate ceilings, so an auxiliary link keeps its permit, and its operation its in-flight entry, until detach and
  carrier shutdown finish, and the shutdown path stops stdin admission first so that aborting an operation cannot open
  that race.
- **An unbounded writer channel plus unbounded daemon stream channels** (the rejected implementation). Each daemon link
  would stay within its own limits while the bridge multiplied them and accumulated output with no process-wide ceiling.

Where the two kinds of ceiling differ is what they do when full, and that decides where loss is allowed: the admission
ceilings refuse only the new request with `at_capacity`, while the channel ceilings apply backpressure rather than
discarding an admitted operation's protocol. Capture and search therefore remain lossless until the daemon's own
slow-subscriber eviction; only notifications, whose source is intentionally a lossy ring, report an in-band `lag` and
continue.

A failed stdout write or flush ends the bridge immediately. Continuing to read stdin or daemon links after the only
protocol sink is gone cannot produce an observable answer and merely accumulates work; trying to synthesize error
terminals is equally ineffective on the failed channel. The bridge aborts its operations and closes every link instead.

_Revisit if_ the bridge gains resumable output with an acknowledged handoff; a larger fixed queue alone would not change
the lifetime argument.

### Identifiers and text

#### A session id is persisted whole and shown as a prefix

A session id is 32 hex digits and a display shows the shortest unique prefix, so machine output carries both, `id` to
persist and `short_id` to show, and every id slot accepts the full id or any unique prefix. The floor is 8 digits: below
that a prefix reads as a typo rather than as an id, and a roster of a few dozen sessions collides at four digits often
enough to matter.

A `short_id` can only ever be a display, because it extends until it is unique against a roster that changes as sessions
come and go: the roster carried in the same reply on `list`, the daemon's pool at resolution time on `info`. A script
that stores one has stored an ambiguity waiting to happen, which the reference page says where a script author will read
it.

Three other ways to shorten an id are rejected:

- **A shorter, denser encoding** (base32 / base58 / a Crockford alphabet), which fits a `u128` in ~26 characters and a
  useful prefix in fewer. Hex is what the wire, the logs, `$FELIS_SESSION_ID`, and every existing doc already spell, so
  a denser id would mean two spellings of one value and a conversion step in every consumer, and the prefix scheme
  already delivers the brevity that motivated it, at the only place brevity is wanted (the screen).
- **A daemon-assigned short handle** (tmux-style `%3`), which is a second namespace to allocate, collide, and reuse; ids
  are never reused precisely so a stale one fails closed, and a recycled short handle would silently address someone
  else's session.
- **Shortening client-side against a roster the caller fetched for that purpose.** The CLI would owe a second round trip
  on every point verb to learn a roster it otherwise does not need, for a prefix that is display-only either way.

A result with no roster in hand therefore carries no `short_id` at all: a prefix nothing checked would be an unchecked
claim of uniqueness, and a consumer reading one field on two kinds of reply has no way to tell the checked case from the
unchecked one. Emitting the floor-length prefix there anyway was the rejected alternative, since it reads as an id and
is not one.

What that rejection rules out is the _unchecked_ prefix and the _extra_ round trip, not shortening against the whole
roster as such, and a prefix the daemon shortened is neither. `Ops::Info` therefore answers one session's row and its
`short_id` together, checked against the pool the resolution ran against, in the reply `sessions info` was already
making, so the verb keeps its documented always-present `short_id` without an `Ops::List`. The pool's owner is also the
only party that can shorten correctly, since it is the only one holding the authoritative roster.

Human output still shows a prefix, because a display is where brevity is wanted and no script parses it. The computation
itself lives beside the resolver in `felis-protocol` (`session_prefix`), so display and resolution cannot drift apart.

#### Scrollback-aware row indices are negative-from-top

Scrollback-aware row indices follow REQ-608: negative-from-top. Wherever a CLI or wire field names a row that may live
in scrollback (`search`'s `line_index`, `capture --format jsonl`'s `row`, a `col_spans` segment's row), `-1` is the
youngest scrollback row, `-N` older, and a live row reports its grid index (`>= 0`) ([spec.md](../../reference/spec.md)
REQ-608). This is _not_ universal: the live-only `RowDelta.row` (`u16`), the cumulative `PromptMark.line` (`u64`,
resize-stable), and an image `Placement.anchor_row` (`i32`, 1-based) name different reference frames on purpose and do
not share the scheme ([ipc.md](../../reference/ipc.md)).

#### Soft-wrap stitching is one rule

Every text-reconstructing surface (`search`, `capture` in text mode, `pipe`, the client's selection) routes through
`felis_grid::logical_line_spans`, seam included, so a wrapped URL / path reconstructs identically everywhere
([scrollback.md](../data-model/scrollback.md) "Soft wrap").

#### Kebab-case on the CLI, snake_case in the keymap

CLI flags and enum values are kebab-case; keymap tokens are snake_case. A keymap token renders kebab on the CLI: the
pipe/capture source `command_output` is `--source command-output` ([input.md](../input.md) "Action mapping"). The token
_meaning_ is shared across the two surfaces even though the spelling differs by surface convention.

The word boundary is one rule across every token a user types, chord keys included: the key is `page_up`, the same
separator its scroll step (`step = "half_page_up"`) uses, and the escape mode is `c_style`. A token that ran its words
together would be a second convention for readers to keep straight.

## Configuration architecture

The declarative configuration system ([reference/config.md](../../reference/config.md)) governs how client instances
read and apply user preferences. Its design is guided by seven decisions.

### One config document, no per-client file

The base config and the `[client.<name>]` overlays live in one `config.toml` ([config.md](../../reference/config.md)):
one place to read the effective config, and the sections shared between clients cannot drift apart between copies.
Per-client files are the rejected alternative; they would reintroduce exactly that drift.

_Revisit:_ a client appears whose config legitimately cannot ship in the shared document (for example, a sandboxed store
with no access to it), or a second first-party client ships whose top-level vocabulary diverges from the GUI's, meaning
a section one client owns and the other must not read. The second case is what `config.d/<client>.toml` drop-ins would
answer: with the vocabularies disjoint, one file per client buys back the strict per-file validation the shared document
gives up, and nothing is left to drift.

### Where that document lives is `directories::ProjectDirs`' answer, unedited

felis joins `config.toml` onto `ProjectDirs::from("", "", "felis").config_dir()` and adjusts nothing per platform, so
the rows of the frozen table in [config.md](../../reference/config.md) are that crate's conventions rather than felis',
doubled Windows `config\` segment included. One maintained resolver decides where the file goes, and felis already takes
its answer for the cache directory everywhere and for the runtime and state directories on the platforms where the crate
populates those slots ([workspace.md](../../reference/workspace.md) "Filesystem layout").

Two per-platform adjustments are rejected:

- **Flattening the Windows path to `%APPDATA%\felis\config.toml`.** It is a felis-specific override of a resolver felis
  otherwise trusts, so every future `ProjectDirs` call in the tree would have to remember to mirror it, and the two
  would drift apart silently the first time one did not.
- **An XDG fallback on macOS.** Beyond that override, it buys a precedence rule and a two-candidate `config path`
  output, when a symlink from the `ProjectDirs` location already answers "my dotfiles live in `~/.config`" with neither.

_Revisit if_ `ProjectDirs` changes its layout.

### Lenient config parsing is the permanent contract, not a transition aid

Because the document is shared, no single consumer owns its full vocabulary: another felis frontend or an external tool
may legitimately read and write sections this client does not know. So an unknown key is never a parse error: the file
keeps applying, and every ignored key is warned with its full path so a typo never goes silently dead
([config.md](../../reference/config.md) "Behavior on missing / malformed values" is the normative matrix).

Strict `deny_unknown_fields` parsing is the rejected alternative: it zeroes every valid setting alongside one typo, and
it makes the shared document unreadable the moment any other consumer adds a key. The generated JSON schema is
deliberately non-strict for the same reason: it exists for editor completion and hover, and the runtime parser stays the
authority; the editor is also where typos surface loudest.

_Revisit:_ none; leniency follows from the shared document, which follows from the one-document decision above.

### An enum's vocabulary grows additively too, one field at a time

A token a build does not recognize degrades that field to its documented default and warns with the full path; the shape
of the value is still checked, so a table where a token belongs fails the document as a structural error. The rule
reaches the fields that have a default to fall back to; a `[keymap]` binding has none, so an unrecognized kind or
argument drops that binding and leaves the rest of the table, which is the same blast radius reached from the other
direction.

Rejecting the whole document on an unrecognized token is the rejected alternative, and it fails for the reason strict
key parsing does: a shared file written by a newer felis would zero this client's font, theme, and keymap over one
presentation option it has no code for, and every added variant would then be a config break needing a flag day.
Degrading the nearest typed value rather than the section keeps the blast radius at the one field the reader cannot
honor: an unknown `shader.post.builtin` leaves the rest of `[shader]` alone.

The generated JSON schema still enumerates only the tokens the build ships, because editor validation describes the
vocabulary in hand while the runtime rule is what carries a file across versions.

_Revisit if_ a key appears whose unknown token cannot have a safe default, which would need a per-field opt-out rather
than a different rule.

### The config crate splits three ways: document, effective config, diagnostics

`ConfigDocument` is the parsed input (raw TOML plus the file it came from), `EffectiveConfig` is one client's view of it
with defaults filled in and its `[client.<id>]` overlay folded in, and `ConfigDiagnostics` is the complete error/warning
set the resolution produced. Validation collects diagnostics and logs nothing; the live client's load path logs the set
once, and `felis config check` prints the same set and derives its exit code from it
([config.md](../../reference/config.md) "Behavior on missing / malformed values").

One type doing all three jobs is the rejected alternative: when each check reaches for `tracing` where it stands, the
only way to see a document's problems is to load it and read the log, which gives a diagnostic verb nothing to render
and no way to count errors. What each caller did about the problems stays out of the set and gets a line of its own
(startup used the defaults, a reload kept the live config), because that is the one thing a diagnostic cannot know, and
folding it in would make the same document report differently depending on who read it.

The document type is separate from the effective config for the second reason: one document resolves more than once (a
different client's overlay), and only the document knows which file it came from, which is what path-valued keys resolve
against.

_Revisit if_ a consumer needs diagnostics streamed as they are found rather than as a set; nothing in the CLI or the
client wants that today.

### File-valued keys resolve against `config.toml`, not the process's working directory

A `shader.post = { file = "fx/glow.wgsl" }` names the file beside the config that mentions it, and a `pipe` binding's
`{ file = "dumps/region.txt" }` sink writes beside it, whatever directory felis was launched from; absolute paths and a
leading `~/` are unaffected.

Resolving against the working directory is the rejected alternative: a config is a durable document read by a GUI client
the user starts from a launcher, a terminal, or a WM keybinding, so the working directory is effectively arbitrary; the
same file would load a shader from one and not the other. It also makes a config unmovable between machines, since a
relative path only works from one place.

A missing shader file stays a **warning**: felis renders without the pass, matching the fall-back-and-warn rule for
every other malformed value, and the same document may legitimately be shared with a machine where the file exists.

_Revisit if_ a key ever needs a search path rather than a single location, which would be a different (and much larger)
decision than this one.

### `font.size_px` is logical pixels, and the code says so at the DPI boundary

The key is multiplied by the window's scale factor, so one value holds across a HiDPI and a standard-DPI monitor. The
unit is frozen, and the naming carries it: config and client speak `*_logical_px`, the renderer speaks `*_physical_px`,
and the multiply is the one place they meet. A single `_px` name on both sides of that multiply is the rejected
spelling: it reads as correct at every site while a Retina window rasterizes glyphs at half size, which is the bug class
the names make visible.

_Revisit:_ none; the unit is a compatibility surface, and renaming costs nothing at the boundary.

## Deliberate asymmetries

Each coverage gap in the [name map](../../reference/control-surfaces.md) is recorded here with the trigger that would
reopen it. The gaps fall into five groups: where a verb sits in the front door, how a window is switched or moved,
region operations, session tags, and driving a session from a script.

One general trigger spans the whole map: a non-Rust client ships and surfaces a capability it can only reach through a
missing CLI verb. Add the verb under this map; do not open a parallel surface. Completing the grid for symmetry's sake
alone is rejected (principle 1: add only what earns its place); verbs are added on demand, each against a named caller.

### Where a verb sits in the front door

#### `attach` is a top-level launcher, not `sessions attach`

The `felis` front-door plays two roles: launching a window (`felis`, `felis attach <id>`, `felis -- <cmd>`) execs the
GUI client, while the headless verbs (`sessions …`, `notifications`, `config`, `doctor`, `version`, `daemon`, `bridge`,
`completions`) run in-process with no window. `attach` opens a window, so it sits with the launchers at the top level,
not under `sessions` with the scripting verbs; the level difference between `attach` and the `sessions` mutators
(`evict` among them) follows from launch-vs-script, not inconsistency.

The global carrier flags `--host` / `--socket` are _orthogonal_ to this split: they pick which daemon (remote over SSH,
or a local socket) any front-door invocation talks to, so they modify both a window launch (`felis --host remote`) and a
headless verb (`felis --host remote sessions list`).

The launch/verb line itself has no revisit trigger; it is the GPU-free-front-door split ([overview.md](overview.md)), a
structural boundary, not a gap.

#### The completion helper stays local-only

The one exception to the carrier flags' reach is the hidden `__complete-sessions` helper, which stays local-only: with
`--host` on the line it offers no candidates, and it never runs `ssh` ([cli.md](../../reference/cli.md) "Other verbs").
The reason is what one `<TAB>` may cost. The helper runs inside a completion function, once per `<TAB>`, and an `ssh`
child has no felis-side deadline: transport timeouts belong to OpenSSH, and felis inherits the tty so a password or
host-key prompt reaches the user ([ipc.md](ipc.md) "Cross-host attach: SSH stdio"). One `<TAB>` against an unreachable
host would therefore block the interactive shell.

The binary applies the rule to its own arguments rather than trusting the tokens its caller forwards, because an
installed completion script is regenerated only when the user reruns `felis completions <shell>`.

Rejected: a short hard timeout plus `BatchMode=yes` and a roster cache. felis would then be choosing ssh policy the
user's own config already owns, and a cache is state the completion path has no place to keep coherent.

_Revisit if_ OpenSSH grows a non-interactive probe felis can call without owning that policy.

#### The cross-carrier re-dial is `felis ssh` and `felis window retarget`

The operation re-points the current window at a _different daemon_ (over SSH, or a second local socket): the
cross-carrier re-dial in [session-lifecycle.md](session-lifecycle.md#cross-carrier-re-dial). It is headless like
`sessions switch` (it pushes to the current daemon and opens no window), but its target namespace is a _carrier_, not a
session id, so folding it into `sessions switch` would make that verb's name lie about half its argument space (a
`switch` that sometimes takes a session id and sometimes a `user@host` plus an `--ssh-arg` splat).

The local form is not a `sessions` verb either, and naming it for its object is what settles that: the thing acted on is
**the window this command runs in**. The session is not moved (it stays on whichever daemon holds it, and the window
simply attaches somewhere else), so `sessions <something>` would name the wrong object. Hence the `window` namespace,
whose sibling verbs (if any are ever earned) all act on that same object.

The remote form is spelled `felis ssh <dest>`, because "ssh somewhere" is the phrase a user reaches for, and its
everyday meaning is "point _this_ terminal at that host" rather than "open a new window there". It is a subcommand of
its own rather than a destination flag on `window retarget`: a required positional destination is what lets `--ssh-arg`
be unconditional, where a flag-supplied destination leaves the requirement as a hand-rolled post-parse refusal.
`felis --host <dest>` is the neighboring operation and keeps its own spelling: it opens a _new_ window over there, and
the two verbs are shown side by side wherever either appears ([cli.md](../../reference/cli.md)).

Each verb carries one destination, in its own positional slot. A bare `--host` on a verb would read as the root
`--host`, which names the daemon an invocation dials rather than where a window lands; the root flag keeps its name and
its before-the-verb placement.

There is no `--local` flag. Naming no destination on `window retarget` _is_ the default local daemon, which makes the
way home the absence of a choice rather than a third way to spell one, and leaves "no destination given" a meaning
instead of a usage error.

The global carrier flags stay one-meaning against it. `felis --host` / `--socket` before a verb name which daemon
receives a _headless verb_; a retarget is addressed to the window's own daemon through the window and carries its own
destination, so `felis --host a ssh b` is a usage error rather than a second meaning for one spelling. Two hosts in one
command line with no rule for which wins is worse than what the refusal costs.

What it costs is one thing, and only from _outside_: a window on a non-default local socket cannot be retargeted by a
command run elsewhere. From inside that window the verb works, because the daemon stamps the endpoint it serves into
every child as `FELIS_SOCKET`, and a front-door verb with no explicit carrier prefers that stamp over the platform
default ([terminal-identity.md](../../reference/terminal-identity.md)).

The stamp is what makes the refusal affordable. Without it the child carries `FELIS_SESSION_ID` and nothing else: the
verb knows _which window_ to move and not _which daemon to ask_, so an in-window retarget from a `--socket work` window
dials the default address and fails against a daemon that has never held the session, and no flag can fix it, since the
one flag that names a daemon is the one this verb refuses. Documenting that cost and leaving the hole is rejected
because "this verb does not work in this window, and cannot be made to" is not a cost a user can route around.

The stamp is the same machinery `FELIS_SESSION_ID` uses (scrubbed from every base, refused in an explicit `env` pair,
forced on every spawn path, REQ-912), so it adds a key, not a mechanism. The daemon itself never reads it: an
auto-spawned daemon can inherit its launcher's value, and honoring that would have it bind an address another process is
already serving.

_Revisit if_ a second `window` verb never materializes **and** the namespace reads as ceremony rather than as an object.

#### `felis bridge` is the session-automation surface, not a mirror of the CLI

Its op set is closed to the `sessions.*` ops, `notifications.subscribe`, and `cancel`, which is how a bridge client ends
a stream. The operator verbs (`daemon`, `config`, `doctor`, `version`), the window launches, and the shell plumbing stay
CLI-only ([cli.md](../../reference/cli.md) "`felis bridge`"). The line is surface policy, not a limit of the wire:
several of the CLI-only verbs are correlated `Ops` requests exactly like the point ops the bridge does expose.

What decides is the caller. A program drives _sessions_ through the bridge, while daemon lifecycle, config diagnosis,
GUI launches and shell integration belong to the operator, who reaches them through the one-shot process whose exit
status and exec are the answer; a non-Rust client that needs `daemon status` runs that verb with `--format json`.

A complete mirror is the rejected alternative. It puts process-lifecycle verbs behind a long-lived correlated stream,
where the reply to "stop the daemon" travels on the link being stopped, and it obliges every operator verb to grow a
bridge twin: one operation with two definitions to keep in step, which is the cost the smaller surface buys off.

Within the shared ops, three differences are policy and are frozen with their reasons:

- The bridge freezes the parameters its clients have needed, so the `keys` and `wait` that `sessions.send` lacks are
  additive params within `v:1` rather than guesses made before the tag.
- `notifications.subscribe` needs no `once` or `timeout`, because `cancel` already ends a stream.
- `sessions.switch` requires the `from` the CLI defaults from `$FELIS_SESSION_ID`: the bridge runs inside an editor
  rather than a felis window, so the variable would name whatever session launched the editor. That verb also answers
  `queued == 0` with a result rather than a refusal, because whether "no window moved" is a failure is the editor's
  judgement, while the one-shot CLI has to pick an exit code and picks `not_queued`.

_Revisit if_ a bridge client needs an operator verb it cannot reach by running the point verb beside it.

#### Alternate frontends live behind `felis frontend <name>`

Alternate frontends live behind `felis frontend <name>`, not behind any unknown token. `felis frontend tui …` execs
`felis-tui`. The implicit form (cargo's external-subcommand model, where any unrecognized first word becomes
`felis-<word>`) is rejected: it makes every typo an exec. `felis session list` (singular) would run whatever `$PATH`
answers to as `felis-session`, or report a missing binary, instead of naming the real mistake, and felis's verb list is
small enough that near-misses are the common failure. The explicit namespace costs one word in a launch command that is
typed rarely and usually wrapped in a desktop entry.

_Revisit:_ none; the safety argument does not weaken with more frontends.

### Switching and moving a window

#### The keymap switches by direction, the CLI by id

The chords step the session ring in creation order (`Previous` / `Next`), against a roster fetched for each press
([session-lifecycle.md](session-lifecycle.md) "Picking the session a chord lands on"); a static config cannot name a
runtime session id (principle 1: typed data, no expressions). `sessions switch <id>` is the by-id form: the daemon
pushes `PushMsg::Reattach` to one window of the from-session (default `$FELIS_SESSION_ID`, "switch _this_ window"; see
the target rule below), and that client runs the same switch path the chord uses.

The by-id verb earns its place because cycling reaches a session two hops away only by O(n) blind flipping; a caller
that knows the target's id needs the direct form. The selection UI stays outside the window (`sessions list` + the
shell's own filtering), per principle 1's no-selector rule.

#### A relay verb moves one window

Pushing to every window subscriber of the from-session is the rejected behavior for `sessions switch` and the retarget
verbs: it reads correctly only when a session has exactly one, and with a mirror open on a second machine it moves a
screen the user is not looking at and has not addressed. The default is the session's last window input owner (the
marker in [session-lifecycle.md](session-lifecycle.md#attachment-identity-and-the-input-owner-marker)), and
`--attachment <id>` names one window explicitly from the roster. A request that named a window and moved none is
reported as a failure, because it did not do what it said.

The exception is a switch onto the session the window is already on: the daemon answers it without resolving a scope at
all, since the state the caller asked for already holds and no window has to be chosen to reach it. Resolving anyway
would let an ambiguous default deny a request whose post-condition was already true.

#### A relay verb reports queue admission

A relay verb reports queue admission, and a landing that fails stays in the window. `sessions switch`, `felis ssh` and
`window retarget` exit `0` once the subscriber outboxes have taken the push; the dial, the attach, and any refusal run
afterwards on connections the source invocation never sees, which is why `queued` is the whole claim ([ipc.md](ipc.md)
"What a switch reply can report"). Both surfaces name that limit rather than leave it to be inferred: the machine result
carries the count under `queued`, and the human line says `not landed` ([cli.md](../../reference/cli.md) "Machine
output").

When the landing then fails, the targeted window keeps the session it is on and records the failure in its own log; the
grid shows nothing, and nothing travels back to the caller, which has exited. The one case where a failed landing does
move the window is a source session that has already exited, where the exit ladder owns the outcome and continues to its
next rung.

The pushed form and the directional chords part company here. A chord re-picks once from a refreshed roster, because its
target was chosen from a roster that can be stale by the time the attach lands; a push named its target, so there is
nothing to re-pick, and a second attempt would move a window the user has stopped asking about
([session-lifecycle.md](session-lifecycle.md) "Newest intent wins, and a landing re-picks once").

### Regions

#### `pipe` is keymap-only, and stays a separate verb from `capture`

`pipe` switches the window into a transient pager / picker session and back
([scrollback.md](../data-model/scrollback.md)): it _pushes_ a region to a sink (a transient TUI over the live grid, the
clipboard, a paste back into the PTY), an interactive, attached-window operation with no headless meaning. `capture` is
its complement: it _pulls_ a region to the **caller's stdout**, which only a connected CLI process can receive. The
split is by operation (push-to-sink vs pull-to-stdout), which is what forces the surface, not the reverse.

So a single `region(source, target)` type is rejected for the same reason `pipe` and `run` stay separate (a CLI
`target=paste` and a keymap `target=stdout` are both nonsense; neither name should lie about half its space).

Headless access to a _typed_ region (the `run`-menu fan-out pattern, [input.md](../input.md) "Action mapping" and "A
menu instead of a prefix", wants e.g. just the last command's output) is a richer _pull_, so it lands on `capture`:
`capture --source command-output|last-command` (the CLI's kebab-case rendering of the keymap's `command_output` /
`last_command` tokens) resolves the mark range daemon-side and streams it as the ordinary capture row stream
(`RegionToDaemonMsg::Rows` → `Row`… → `RowsDone`, [ipc.md](../../reference/ipc.md) "Region (kind = 6)"); only the keymap
`pipe` chord uses the one-blob `Request` → `Reply` push path ([scrollback.md](../data-model/scrollback.md) "Capture").
It is **not** a `sessions pipe` verb, which would wear the push name on a pull operation. The source _token_ aligns with
the keymap; `selection` is client-owned and has no headless form, so it stays keymap-only.

_Revisit:_ none open; the push/pull split is structural, and a richer region extends `capture`.

#### `pipe`'s `clipboard` sink overlaps `copy`, and keeps its place

`pipe { source = "selection", target = "clipboard" }` and `copy { what = "system" }` reach the same clipboard write with
the same bytes, the one place two keymap tokens spell one operation. The sink stays because `pipe`'s target set is
uniform across its sources: every region can go to a command, the clipboard, a file, or back as a paste, and carving
`clipboard` out for `selection` alone would make the grammar source-dependent: a user reading the `target` list would
have to remember which sink disappears for which source.

The duplication costs one redundant spelling; the alternative costs the regularity of the whole `pipe` grammar. `copy`
is the one that earns its own token: it is the chord on the selection a user makes with the mouse, not a region-routing
operation.

_Revisit if_ `pipe` ever grows a source-specific sink for another reason, at which point the grammar is already
source-dependent and this carve-out is free.

### Tags

#### Tags are CLI-only labels felis stores but never interprets

Tags follow the marginalia model. `sessions tag` attaches and removes opaque strings on a session (`OpsToDaemonMsg::Tag`
→ `TagsUpdated`), and `spawn --tag` (`SpawnArgs.tags`) seeds the same set at creation so a scripted spawn-then-filter
pipeline never exposes an untagged window; both are surfaced in `list` / `info` and their machine output;
`sessions list --tag <t>` is a client-side "any of" pre-filter over the roster the open already fetched. There is no
keymap `kind`: the user does not label a session by keystroke.

felis is the _data source_; the _annotator_ (column layout, color, which fields to show, deriving `git` branch from
`cwd`) lives in the external picker (fzf, the WM), exactly as Emacs `marginalia` runs its annotators in the consumer,
not the completion table. That split is principle 3 (daemon owns the raw label state; the client owns presentation) and
principle 1 (the picker does formatting better, so felis does not grow a `--pretty-columns` layout engine; richness
flows through the machine output).

Tags are **not** a session group: they drive no shared layout, synchronized attach, or input fan-out; that tmux-style
grouping is a permanent non-goal ([non-goals.md](../non-goals.md)).

Alongside the user-supplied tags, `SessionInfo` carries two felis-_derived_ annotations for the same picker:
`last_notification` (the most recent OSC 9/99/777, the only typed status a full-screen TUI agent emits, since it sets no
OSC 133 marks) and `foreground` (the `comm` of the PTY foreground process group via `tcgetpgrp`, read from the OS
process table by pid, not the shell's output; principle-4 clean). Both are read-side only; tags remain the one
write-path label.

_Revisit:_ a real consumer needs a label felis itself must act on (e.g. tag-scoped reap policy), at which point the
daemon would interpret a tag, and this "never interprets" line is what gets reopened.

#### The tag set is bounded

The label set is **bounded** (32 tags per session, 128 bytes per tag; the caps live in `felis-protocol` next to the
messages): tags are marginalia, and anything past those caps is content wearing a label's name: an unbounded store would
let one same-UID script inflate every roster read (each `list` clones and re-encodes the set) toward the 64-MiB frame
limit. A delta that would breach a cap is refused whole with a typed reason (`TagsUpdated.denied`), never partially
applied, so the reply's `tags` snapshot stays honest.

Classifying tags as an unbounded trusted-user store is the rejected alternative: same-UID trust already gates the
socket, but "trusted" is an argument about _who_, not about what the roster surface can absorb per read.

_Revisit:_ a real picker workflow that needs labels past the caps.

#### The `tag` verb and the `--tag` filter share one word by design

`sessions tag <id> <t…>` (a mutator) and `sessions list --tag <t>` (a read-side filter) intentionally spell the concept
the same way; grammar position disambiguates (a verb slot versus a flag slot) the way `gh label` and
`gh issue list --label`, or `docker tag` and a `docker images --filter`, reuse one noun across a mutator and a query.
Renaming the filter (`--tagged` / `--with-tag` are the candidates) would split the one concept across two word forms and
decouple it from the `tags` field and `OpsToDaemonMsg::Tag` that back both surfaces.

_Revisit if_ a tag-related verb and a tag-related flag ever land on the **same** subcommand, where the shared word would
then be genuinely ambiguous.

#### One `tag` verb: adds positional, removes behind `--remove`

`OpsToDaemonMsg::Tag` carries `add` and `remove` in one message and the daemon applies adds then removes, so a separate
`untag` verb would be two spellings of one round trip that between them could not express what the wire supports:
swapping a label without a window where the session carries both or neither. One verb is what makes that atomic, and it
keeps the verb list matching the message list. Rejected: an `untag` alias; one way per operation.

_Revisit if_ removal ever needs options adding does not (a `--remove-matching` glob, say), where the shared verb's flag
set would start pulling in two directions.

### Driving a session from a script

#### Waiting is `send --wait`, a pure client of the existing mark stream

There is no wait verb and no daemon Wait message. The blocking behavior a scripted driver needs ("tell me when the
command finishes, and how it exited") is built entirely CLI-side: attach as an ordinary subscriber, drain the rehydrate
burst, then block for the next streamed `GridMsg::PromptMark` of kind `D`. Every mark that arrives is a command
completing _now_: an `Ops` burst replays no historical marks, and the daemon seeds each subscriber's mark cursor at
attach either way.

A daemon-side `OpsToDaemonMsg::Wait` with a deferred reply is rejected: it would duplicate delivery machinery the
subscriber stream already has, add the daemon's first long-parked pending reply, and buy nothing: the stream is already
ordered after the connection's own input frames, which is what makes `send --wait` race-free (subscribe, inject, then
the D mark of even an instantly-finishing command must still arrive on this stream).

That ordering is also why the wait is a flag on `send` and not a verb beside it: a standalone `wait` opens its own
connection, so a command started by a preceding `send` can finish before the wait subscribes, a race the surface's own
docs would have to warn callers away from. Nothing is lost: `send --wait` with no text or `--key` injects nothing and
purely watches. The named caller is the agent-driving skill (`skills/felis`), which otherwise polls `capture` in a sleep
loop.

_Revisit if_ a waiter needs marks without the full grid-diff traffic an attach carries (e.g. hundreds of concurrent
waiters), at which point a lean mark-only subscription kind earns its place.

#### `notifications --session` / `--once` are stream filters, not a wire change

`send --wait` covers only producers that emit OSC 133 marks; a full-screen TUI agent sets none, and its one typed
completion signal is the OSC 9/99/777 stream. Rather than a second wait verb (or a daemon-side per-session
subscription), the existing observer firehose gains two client-side flags: filter to one session, exit after the first
match. The two waits stay complementary by producer (the mark wait carries an exit code marks-emitting shells provide; a
notification carries title/body/urgency and no exit code), so neither subsumes the other.

_Revisit if_ observers multiply enough that every subscriber decoding every session's notifications becomes a real cost,
at which point a daemon-side filter earns its place.

#### `send --key` reuses the chord grammar and sends the keystroke itself

`send --key` sends `InputMsg::Key`, encoded daemon-side. Driving a TUI needs arrows / Escape / Tab, and the correct byte
sequence depends on keyboard modes (Kitty flags, DECCKM, DECKPAM, modifyOtherKeys) only the daemon-side grid knows: a
`--raw` caller hand-writing escapes guesses. The flag is typed data (a parsed chord, principle 1's action constraint),
spelled in the `[keymap]` chord grammar so one vocabulary names a keystroke across config and CLI.

Keeping the encoder client-side and shipping the resulting bytes as `InputMsg::KeyBytes` is rejected, and the reason is
not encoder duplication: there is still exactly one encoder. It is the stale-mode race: a client encodes from modes the
daemon mirrors to it in grid frames, and a producer can enable a keyboard mode and then wait for input inside the window
between the daemon parsing that mode and the frame reaching the client. An occluded, pull-paced window makes that window
arbitrarily wide. Encoding where the modes are authoritative closes it, and the encoder moves whole rather than
splitting: the client resolves the logical key, its keymap bindings, and IME, and only the mode-dependent byte encoding
is the daemon's, which is where principle 3 puts it anyway.

A `--submit` convenience alias for `--key enter` is rejected too: two spellings of one keystroke is exactly the
duplicate-surface shape the one-verb-per-operation rule exists to refuse, and the general form already reads as the
intent.

_Revisit if_ a surface appears that must encode a keystroke with no daemon in reach, which would argue for a client-side
encoder.

#### `send` is one typed operation

Three constraints are its whole grammar ([cli.md](../../reference/cli.md) "Verb details"): at least one of TEXT, `--key`
or `--wait`; `--raw` with TEXT; `--timeout` with `--wait`. The operation is "deliver this input, then optionally await
the next prompt mark", which with no payload degenerates to watching the command already running.

Splitting it into `send` / `key` / `wait` verbs is rejected, and the two halves fail differently. A separate `wait`
races, for the reason the `send --wait` section above gives: it subscribes after the input was injected, so a fast
command's `D` mark can be gone before the verb is listening. A separate `key` verb has no race, since a finished `send`
has already enqueued its paste ahead of anything issued later; what keeps `--key` on `send` is that one call names one
target, resolves it once, and states the input sequence in order. Every combination of payload kind, chords and wait is
meaningful, which is what makes the cross-product one operation rather than three verbs sharing a target.

Hence the rule the grammar is frozen under: no new flag may depend on another. The two `requires` above are the whole
dependency graph, and a different wait condition or a different payload kind is a new verb, not a fourth flag that reads
only in combination with one of these.
