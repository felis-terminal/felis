---
title: Terminal identity
sidebar:
  order: 12
---

felis identifies natively across all identity surfaces: `TERM_PROGRAM` reports `felis`, DA3 unit ID reports `felis-1`,
and `TERM` reports `xterm-felis`. The terminfo entry advertises verified capabilities while preserving the `xterm-`
lineage prefix for compatibility heuristics. Architectural rationale for native identity rather than terminal emulation
spoofing is documented in [terminal-identity.md](../explanation/architecture/terminal-identity.md).

## The default identity

The daemon stamps identity variables onto child processes during spawn execution in `crates/felis-daemon/src/lib.rs`
(`apply_env_policy`):

| Variable               | Default                                                                                                                | Override source                                             |
| ---------------------- | ---------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------- |
| `TERM`                 | `xterm-felis`                                                                                                          | `FELIS_TERM` in resolved base or daemon environment         |
| `TERM_PROGRAM`         | `felis`                                                                                                                | `FELIS_TERM_PROGRAM` in resolved base or daemon environment |
| `TERM_PROGRAM_VERSION` | Crate version (semver without commit hash)                                                                             | Not overridable via base                                    |
| `COLORTERM`            | `truecolor`                                                                                                            | Not overridable via base                                    |
| `FELIS_SESSION_ID`     | Owning session ID (32 hex digits)                                                                                      | Reserved; cannot be overridden                              |
| `FELIS_SOCKET`         | Daemon listening socket endpoint                                                                                       | Reserved; cannot be overridden                              |
| `LANG`                 | macOS only, and only when the resolved environment names no locale: the current region as `{language}_{country}.UTF-8` | Any `LANG`, `LC_ALL` or `LC_CTYPE` already set              |

Both interactive GUI shells and CLI `felis sessions spawn` invocations pass through `apply_env_policy`.

Environment construction resolves in the following sequence:

1. Base environment: `SpawnArgs.env_base` (when provided via local socket), the connection relay snapshot, or the daemon
   process environment ([ipc.md](ipc.md) § "Session (kind = 4)").
2. Denylist scrub: Keys matching security filtering rules are removed, and so are the keymap-command origin variables
   ([keybindings.md](keybindings.md)), which, unlike denylisted keys, `SpawnArgs.env` may still set.
3. Identity and addressing stamps: The variables from the table above are injected. Five are unconditional;
   `FELIS_SOCKET` is stamped only when the daemon serves an endpoint, and a daemon serving none removes an inherited
   value rather than letting a child address the daemon it was launched from.
4. Locale fill: on macOS only, `LANG` is stamped when the resolved environment sets none of `LANG`, `LC_ALL` and
   `LC_CTYPE` to a non-empty value.
5. Custom overrides: Caller-provided `SpawnArgs.env` pairs are applied.

Before filtering, `$SHELL` and `FELIS_TERM` / `FELIS_TERM_PROGRAM` are extracted from the base environment. If missing,
they fall back to daemon environment values, with `$SHELL` defaulting to `/bin/sh` (`powershell.exe` on Windows).

The `LANG` fill reads the same entries: the base for a create that carries one, the daemon's own environment for a
create that runs on the daemon's birth environment. Its value is `CFLocaleCopyCurrent()`'s `kCFLocaleLanguageCode` and
`kCFLocaleCountryCode` joined as `{language}_{country}.UTF-8`, accepted only when `/usr/share/locale/<value>` exists and
`en_US.UTF-8` otherwise, and it is queried per create rather than cached. On every other platform no `LANG` is stamped.

Caller overrides (`SpawnArgs.env`) cannot rebind reserved addressing variables (`FELIS_SESSION_ID`, `FELIS_SOCKET`) or
keys present in the denylist; attempts are rejected with typed errors (REQ-912). Case normalization matches platform
rules (case-insensitive on Windows).

For cross-host SSH connections, `SSH_AUTH_SOCK` is redirected to the daemon's stable symlink (`<socket>.agent`), which
updates to track the most recently active relay connection ([ipc.md](ipc.md)).

Addressing variables (`FELIS_SESSION_ID` and `FELIS_SOCKET`) permit in-session utilities to interact with their parent
daemon and window without global discovery ([cli.md](cli.md) and [control-surfaces.md](control-surfaces.md)).

Device attribute queries (DA1, DA2, DA3) return static identification payloads specified in
[vt-compliance.md](protocols/vt-compliance.md) § "Reporting and queries". XTGETTCAP `TN` / `name` answers the `TERM` the
session's program was spawned with, after every step above. felis does not set `KITTY_WINDOW_ID`; Kitty protocol support
is probed via standard escape queries (`APC _G a=q` and `CSI ? u`).

## The terminfo entry

A terminfo definition is provided in `crates/felis-daemon/terminfo/felis.terminfo`, compiling to `xterm-felis`. It
inherits from `xterm-256color` and adds verified features: 24-bit truecolor with colon-delimited RGB parameters, styled
and colored underlines, bracketed paste, focus tracking, DECSCUSR cursor shapes, strikethrough (`smxx` / `rmxx`), cursor
color (`Cs` / `Cr`), and the Kitty keyboard flag `fullkbd`.

The entry omits DCS-based synchronization (`Sync`): felis implements only DEC private mode synchronization
(`CSI ? 2026 h/l`). Six standard capabilities are explicitly canceled. `acsc`, `smacs`, `rmacs` and `sgr` go because
character set switching (SCS) is a no-op (REQ-211); `xr` and `rv` go because their XTVERSION and DA2 response templates
match xterm's replies, not felis's.

Design rationale for capability selection is documented in
[terminal-identity.md](../explanation/architecture/terminal-identity.md).

The compiled entry adheres to macOS ncurses 6.0 constraints: compiled size must remain under 4096 bytes and color pairs
are bounded by `pairs#0x7fff` (validated by `nix/compile-terminfo.sh`).

The Nix package and the Linux and macOS release archives install the compiled entry under `share/terminfo`, and
`felis.app` carries a copy under `Contents/Resources/terminfo`. On Unix the daemon prepends the directory shipped with
its own executable to every session's `TERMINFO_DIRS`: the bundle's `Contents/Resources/terminfo` on macOS, else the
install prefix's `share/terminfo`, with symlinks to the executable resolved first. It keeps any existing value,
including one the spawn request sets, and, when the variable was unset, appends a trailing empty element that stands for
the host's default directories. A directory already listed is not added again, and a daemon whose tree ships no entry (a
`cargo build`, a distribution package that installs into the system database) leaves the variable alone. The archives'
`bin/` launchers prepend the same directory for the commands they run themselves, which is how `felis doctor` finds it.

A program that serves sessions from `felis-daemon` in its own executable has no such directory beside it. The library
exports the entry's source as `felis_daemon::TERMINFO_SOURCE`; the embedder compiles it with `tic -x` into a directory
of its choosing and names that directory in `DaemonCaps::terminfo_dir`, which the daemon lists in place of the shipped
one, by the same rules. Set to `None`, the field leaves `TERMINFO_DIRS` as the spawn would otherwise have it. The daemon
never compiles the entry itself.

On systems where `xterm-felis` is not yet installed in the system terminfo database
(`tic -x crates/felis-daemon/terminfo/felis.terminfo`), applications may fail terminfo lookups. Users can fall back to
standard profiles via `FELIS_TERM=xterm-256color`; a host reached by `ssh` gets the entry from the function in "Hosts
reached by ssh".

## Hosts reached by ssh

`share/felis/shell-integration/ssh.bash`, `ssh.zsh` and `ssh.fish` each define an `ssh` function that runs
`share/felis/shell-integration/ssh.sh` with `sh`. It runs `ssh` with the arguments unchanged unless `TERM` is
`xterm-felis` and the invocation is an interactive login: one destination, no remote command, and none of `-f`, `-G`,
`-M`, `-N`, `-O`, `-Q`, `-S`, `-s`, `-T`, `-t`, `-V`, `-W`, `-w`. It also leaves the arguments unchanged when `ssh -G`
reports any of these:

- a `RemoteCommand`, a `RequestTTY` other than `auto`, or a `SessionType` other than `default`;
- `StdinNull`, `ForkAfterAuthentication`, `PermitLocalCommand`, or a `Tunnel`;
- a `ControlMaster` other than `no` or `auto`;
- a `TERM` under `SetEnv`;
- an install connection whose configuration differs from the login's in anything beyond the options it sets itself
  (`Match command` and `Match sessiontype` can route it elsewhere).

Otherwise it first opens an install connection: the same arguments plus `-T`, `RequestTTY=no`, `RemoteCommand=none` and
`ClearAllForwardings=yes`, fed the local `infocmp -x xterm-felis` output, with a remote `sh` that compiles it with
`tic -x` into `~/.terminfo`. A remote without `tic` counts as installed when it already resolves `xterm-felis`. The
function adds no multiplexing options, so the install connection uses the user's own `ControlMaster` when one is
configured.

| Install connection                       | Login                                                                                                                              |
| ---------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| Exits 0                                  | `ssh` with the arguments unchanged                                                                                                 |
| Exits 255 (ssh's own failure)            | None; the function exits 255                                                                                                       |
| Exits otherwise, or no local entry found | `felis: xterm-felis is not available on <destination>; using TERM=xterm-256color` on stderr, then `ssh` with `TERM=xterm-256color` |

The function exits with the login's status. `SetEnv TERM=xterm-256color` in a host's `~/.ssh/config` block excludes that
host, which suits one whose server-side forced command must not run for the install connection.

## The escape hatch: `FELIS_TERM` / `FELIS_TERM_PROGRAM`

Users can configure terminal identity overrides by defining variables in their login shell configuration:

```fish
set -gx FELIS_TERM xterm-kitty
set -gx FELIS_TERM_PROGRAM kitty
```

Individual commands can override identity directly:

```console
$ TERM_PROGRAM=kitty claude
```

`FELIS_TERM` and `FELIS_TERM_PROGRAM` configure the identity stamp and are automatically scrubbed from child process
environments via `ENV_DENYLIST` ([security-model.md](../explanation/security-model.md)).
