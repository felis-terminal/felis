---
title: Fix terminfo and terminal identity problems
sidebar:
  order: 9
---

Get felis's honest `TERM=xterm-felis` accepted on a host that lacks its terminfo entry, or worked around in an
application that checks terminal names instead of querying capabilities. Find your symptom below.

## Unknown terminal type or garbled ncurses rendering

Programs under felis read `TERM=xterm-felis`, so a host with no `xterm-felis` entry can leave text-mode applications
(`vim`, `htop`) refusing to start or drawing corrupted screens.

Run `felis doctor` to see whether this host has the entry and which file path it found (see
[cli.md](../reference/cli.md#doctor)). An obsolete copy in `~/.terminfo` can shadow the packaged entry.

The Nix package bundles the compiled terminfo entry, and so do the Linux and macOS archives, whose `bin/` launchers add
it to `TERMINFO_DIRS` for every session they start. On a system built from source, or a remote host reached over SSH,
compile the entry with `tic` as shown in [Install](install.md#build-from-source). Because `~/.terminfo` takes precedence
over `TERMINFO_DIRS`, an entry compiled there by hand shadows later updates to the packaged one, so leave that directory
alone on a system where Nix manages felis.

If you cannot install terminfo entries on a remote system, configure the daemon to advertise a compatible generic
terminal identifier:

```sh
export FELIS_TERM=xterm-256color
```

For capability implications of fallback terminal names, see [terminal-identity.md](../reference/terminal-identity.md).

## On macOS, /usr/bin programs say unknown terminal but Nix ones work

When running macOS system utilities (such as `/usr/bin/vim` or `tput lines`), tools may report an unknown terminal while
Nix-installed tools function correctly. macOS system tools use an older ncurses version that cannot parse 32-bit
terminfo formats or entries exceeding 4096 bytes. felis compiles its packaged entry to conform to both limits (see
[terminal-identity.md](../reference/terminal-identity.md)).

If an older manually installed entry exists in `~/.terminfo`, it shadows the package entry. Remove any manual copies:

```sh
rm -f ~/.terminfo/x/xterm-felis ~/.terminfo/78/xterm-felis
```

Verify that system tools resolve the entry by running `TERM=xterm-felis /usr/bin/tput lines`, which should output your
window row count.

## An app gates Kitty-protocol features on the terminal's identity

Some applications check `TERM` or `TERM_PROGRAM` for specific terminal names rather than querying terminal capabilities
dynamically. Although felis identifies truthfully by default, configure the daemon environment to provide compatibility
overrides:

```fish
set -gx FELIS_TERM xterm-kitty
set -gx FELIS_TERM_PROGRAM kitty
```

Alternatively, set the environment variable per command: `TERM_PROGRAM=kitty <command>`. Common scenarios include:

- **timg renders coarse blocks instead of sharp images:** timg looks for `kitty` in `TERM` or the XTVERSION reply rather
  than probing with `a=q`, so `TERM_PROGRAM` does not reach it. Use `timg -p kitty`, or set `FELIS_TERM=xterm-kitty` as
  above. Applications that probe dynamically (such as yazi or presenterm) require no workarounds.
- **Shift+Enter in Claude Code sends a plain Enter:** Claude Code enables enhanced keyboard protocols only for
  allowlisted `TERM_PROGRAM` values. Pass `TERM_PROGRAM=kitty claude`, or rebind the specific key chord (see
  [fix-keyboard-input-problems.md](fix-keyboard-input-problems.md)).
