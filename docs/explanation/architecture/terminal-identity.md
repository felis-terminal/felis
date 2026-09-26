---
title: Terminal identity design
sidebar:
  order: 5
---

Why felis identifies honestly as itself (`TERM=xterm-felis`, `TERM_PROGRAM=felis`, a terminfo entry that claims exactly
the verified capability set), and why impersonating another terminal is a user-side opt-in rather than the default. The
identity surface itself (the environment table, the terminfo entry, the escape-hatch mechanics) is
[reference/terminal-identity.md](../../reference/terminal-identity.md).

## Verified capabilities only

The terminfo entry is `use=xterm-256color` plus exactly the extensions felis is _verified_ to implement. Under-claiming
hides real features from terminfo-driven applications; over-claiming invites them to drive unimplemented paths (the
concrete case: advertising the DCS-form `Sync` capability would make tmux emit `DCS = 1 s ST` that felis ignores, so the
entry omits it and felis advertises only the DECSET form it implements).

The DA1 feature mask is the deliberate exception to this rule. The reply is xterm's VT420 default mask verbatim
(`?64;1;2;6;9;15;16;17;18;21;22;28;29c`), and several of its bits name families felis does not implement (printer port,
locator, NRCS beyond parse-and-ignore) or implements partially (rectangular editing). Trimming the mask to verified bits
was rejected: programs that branch on DA1 treat a sparse mask as "legacy terminal" and drop into fallback paths that
hurt UX for no functional gain, while feature _selection_ happens through terminfo (the layer this doc holds to
verified-only), not through DA1's coarse class bits. What DA1 must be is constant and version-independent (the
identity-class query rule in [security-model.md](../security-model.md)); accuracy per bit is terminfo's job. _Revisit
if_ a real program is found driving an unimplemented family off a DA1 bit alone; that flips the cost-benefit the same
way the DCS-form `Sync` case does for terminfo.

The two costs this design carries are accepted deliberately:

- Requiring a `tic` install for `xterm-felis` is tolerable because the escape hatch (`FELIS_TERM=xterm-256color`) is a
  one-variable fallback on hosts without the entry.
- The entry not being reconstructable from `infocmp` on a remote host (the price of `use=xterm-256color` over kitty's
  standalone entry) is acceptable because felis cross-host work goes through the daemon ([ipc.md](ipc.md)), not TERM
  propagation. _Revisit if_ a host without `xterm-felis` becomes the common case rather than the `ssh` exception: a
  standalone entry gives up the inheritance but survives `infocmp` transport.

## Why the entry is trimmed and 16-bit

The "verified capabilities only" rule cuts both ways: `use=` lends felis every `xterm-256color` capability, including
ones felis does not implement, so the entry cancels them ([the reference twin](../../reference/terminal-identity.md)
lists which). The alternate-charset group is the one with visible fallout: SCS is a deliberate no-op (REQ-213), so an
ncurses `box()` drawn through `acsc` would put literal `qqlk` on the grid; canceled, ncurses falls back to the ASCII
`+`/`-`/`|` approximations: ugly rather than wrong.

The entry's numeric and size limits are its readers' decision, not felis's: macOS bundles ncurses 6.0, which predates
the 32-bit terminfo format and refuses an oversized entry, so the pin and the ceiling
([the reference twin](../../reference/terminal-identity.md) carries the values) say what that reader can parse. Either
violation is total and silent: every `/usr/bin` tput, vim, less and top inside felis reports an unknown terminal while
Nix-built programs work, so the symptom can be as small as an empty `tput lines` in a benchmark harness. Because the
limits are invisible in the source, [the Nix build](../../../nix/compile-terminfo.sh) asserts both.

Three alternatives are rejected:

- **Compile the entry with macOS's own `tic`** (in the package, or at home-manager activation). It produces a readable
  entry, but resolves `use=` against Apple's 2015 `xterm-256color`, which claims `kbs=^H` and `kmous=\E[M` where felis
  sends DEL and SGR mouse, trading a format bug for a wrong-capability bug. It also puts `/usr/bin` in a Nix build.
- **Transcode the 32-bit entry to 16-bit after `tic`.** felis would own a terminfo binary-format writer to avoid one
  line of source.
- **Install into `~/.terminfo` at activation.** That directory outranks `TERMINFO_DIRS`, so the hand-installed copy
  would shadow the package through every later upgrade.

_Revisit when_ macOS ships ncurses 6.1 or newer: the `pairs` pin, the size ceiling and the build assert that guards them
can all go, and the entry can inherit whatever its base offers. The cancellations stay regardless: they are about what
felis implements, not about what its readers can parse.

## Why the escape hatch is an environment variable

The escape-hatch variables are read from the spawn's resolved environment (the base environment a create carries, else
the daemon's own), because the daemon owns the spawn site and reads no config file: the dependency between the binaries
is one-way, `felis-client → felis-daemon` ([overview.md](overview.md)), so the client's `config.toml` never reaches the
daemon. An `[env]` section there cannot reach the spawn site without new cross-boundary plumbing (a daemon-side config
reader, or a new protocol field at session create). The environment is the one control surface the daemon already reads
([control-surfaces.md](control-surfaces.md)), and a login-scoped variable delivers the same "one place to flip it"
outcome at a fraction of the cost. The config path is deferred, not rejected; it can supersede the escape hatch if a
non-env workflow demands it.

## Why felis does not seed `KITTY_WINDOW_ID`

felis answers the Kitty graphics query (`APC _G a=q`) and the Kitty keyboard query (`CSI ? u`) truthfully, so
applications that _query_ detect the protocols regardless of the name; only env-sniffing applications are missed, and
seeding a kitty-namespaced variable by default would contradict honest naming.

`FELIS_SESSION_ID` (which felis _does_ stamp unconditionally) is deliberately not the `KITTY_WINDOW_ID` case: it is
felis-namespaced, it is addressing rather than identity, and it cues nothing about another terminal's behavior.

## Why impersonation is not the default

The pressure to impersonate is real. Some applications gate capability negotiation on a terminal-name allowlist instead
of querying: Claude Code enables the Kitty keyboard protocol (and modifyOtherKeys; a binary audit of Claude Code 2.1.156
confirmed both are behind the same gate) only when `TERM_PROGRAM` / `TERM` is in
`["iTerm.app", "kitty", "WezTerm", "ghostty", "tmux", "windows-terminal", "WarpTerminal"]`, and sends a non-allowlisted
terminal no enable sequence at all. The honest `felis` identity gets neither protocol, so Shift+Enter stays broken there
until either the user opts into the escape hatch or `felis` lands on the allowlist upstream (tracked as
<https://github.com/anthropics/claude-code/issues/27868>; the workaround lives in
[fix-terminfo-problems.md](../../how-to/fix-terminfo-problems.md)). The same gate shape appears on the graphics side:
timg decides Kitty-graphics support by string-matching `kitty` / `ghostty` in `$TERM` or the XTVERSION reply,
deliberately not by the `a=q` probe, so the honest identity gets half-block fallback until the user passes `-p kitty` or
impersonates.

Impersonating kitty by default (`TERM=xterm-kitty`, `TERM_PROGRAM=kitty`, seeding `KITTY_WINDOW_ID`) would close that
papercut with zero work, and is rejected anyway:

- It is dishonest for a terminal that is its own project.
- `xterm-kitty` over-claims: it advertises the DCS-form `Sync` capability felis's own entry deliberately omits
  ("Verified capabilities only" above).
- A seeded `KITTY_WINDOW_ID` cues some applications to attempt kitty-only behaviors.

Keeping `xterm-256color` is rejected too: honest about _not_ being xterm, but it under-claims. Styled underlines,
underline color, and DECSCUSR all disappear from terminfo-driven applications.

The escape hatch preserves impersonation as a deliberate, single-knob user choice rather than a hard-coded default or a
per-command prefix.

## Why the version-to-programs stays bare semver

The `--version` output carries the commit revision (every binary's `--version` is
`<name> <semver> (<revision>[-dirty])`, and `felis version` compares the cli's, the client's and the running daemon's),
but `TERM_PROGRAM_VERSION` and the XTVERSION reply carry only the semver. The two version surfaces have opposite
audiences and so want opposite things. `--version` is read by an operator asking _which build is this?_ Because the
daemon outlives the window (a rebuilt client can reattach to a stale daemon), the commit revision is the only thing that
distinguishes two builds sharing one semver. The version-to-programs is read by software inside the terminal doing
feature detection; those checks want a value that is stable across the many rebuilds of one release and comparable
between machines, which a per-build hash breaks. Leaking the hash there would also enlarge the fingerprint a program can
key on, against the honest-but-minimal identity this doc argues for elsewhere.

This is why the compatibility handshake is unaffected: the gate is the frozen preface's protocol major and minor
([ipc.md](../../reference/ipc.md) "Versioning"), never a version string, so embedding a churning hash in `--version`
costs nothing there. The daemon does put its build identity on the wire, in `Welcome.identity`, and no peer gates on
that either ([ipc.md](ipc.md) "Schema evolution: major, minor, feature flag").
