---
title: Security audits
sidebar:
  order: 9
---

The standing security checklists. The threat model and the policies they verify are
[security-model.md](../explanation/security-model.md).

## `O_CLOEXEC` + `O_NOFOLLOW` audit (standing)

Every file descriptor and path open site is audited for `O_CLOEXEC` and `O_NOFOLLOW` flags; new open sites join this
checklist. Current status:

- **Sockets** (`UnixListener::bind`, `UnixStream::connect`): tokio / mio set `SOCK_CLOEXEC` by default. ✅
- **Socket-dir creation and judgment** (`SocketDir::lock`): `fs::create_dir` of the parent alone under an explicit
  `umask(0o077)`, then `open(O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)`, an exclusive `flock`, and `fstat` on that
  descriptor: a directory, not a symlink, owned by the uid, access bits exactly `0700`, or the start fails. The judgment
  is made on the locked descriptor, never on the pathname, and a parent felis did not create is never `chmod`ed. ✅
- **Socket bind** (`UnixListener::bind` under `umask(0o177)`): the socket exists `0600` from creation, so there is no
  post-`bind` `chmod` and no window with looser bits. After the bind, one `lstat` of the parent confirms it still names
  the locked directory. ✅
- **Startup probe** (`clear_endpoint`): `fs::symlink_metadata` first, so a symlink or a directory at the endpoint fails
  the start untouched; a socket is connected to and unlinked only on `ENOENT` / `ECONNREFUSED` with its peer uid checked
  first. ✅
- **Font reads** (`memmap2::Mmap::map` in `felis-shaping`; see
  [text-shaping.md](../explanation/rendering/text-shaping.md)): Discovered system font files are root-owned and
  read-only. ✅
- **PTY file descriptors**: `felis-pty` opens slave descriptors with `O_CLOEXEC` and flags master descriptors with
  `FD_CLOEXEC` immediately after allocation. ✅
- **`t=t` temporary file path** (Kitty graphics): Uses pinned parent descriptors with
  `openat(parent, name, RDONLY | NOFOLLOW)` and `unlinkat`. On Windows, `t=f`, `t=t`, and `t=s` are rejected with
  `ENOTSUP`, so producer `a=q` probes fall back to direct transfer (`t=d`). ✅

## OS hand-off audit (standing)

Every site handing terminal-supplied strings to an OS launcher is audited to ensure payloads reach the system as data
rather than shell command lines:

- **OSC 8 activation** (`felis-client`'s `hyperlink::open_url`): Grid strings cross `felis-client-core`'s typed boundary
  `ActivationTarget::parse`, which re-validates scheme allowlists and rejects interior NULs, control characters, and
  bidi overrides before reaching `open_url`. Linux and macOS invoke `xdg-open` / `open` passing the target as an
  isolated argv element. Windows invokes `ShellExecuteW` directly with `lpParameters = null`; no `cmd /c start` path
  exists. ✅

## `felis-protocol` crate-purity audit (standing)

`felis-protocol` is the cross-language wire vocabulary surface ([overview.md](../explanation/architecture/overview.md)):
it must contain no runtime or operating system dependencies.

- **Production dependencies** (`cargo tree -p felis-protocol --edges normal,build`): synchronous and OS-agnostic, with
  no `tokio`, `mio`, `libc`, `rustix`, `nix`, `windows-sys`, `core-foundation`, or `objc`. ✅
- **Source tree**: `crates/felis-protocol/src/` contains no `std::os::*`, no `cfg(unix)`, and no async primitives. ✅

`tools/dep_bans.py` under `just deny` ([workspace.md](workspace.md) "Crate map") fails a pull request that adds `tokio`,
`async-std`, `mio`, `libc`, `rustix`, `nix`, or any `windows-sys` / `core-foundation` / `objc` crate to the production
dependency graph on any target.
