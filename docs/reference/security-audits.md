---
title: Security audits
sidebar:
  order: 9
---

The threat model, trust boundaries, and mitigation policies verified by these checklists are documented in the companion
explanation page [security-model.md](../explanation/security-model.md).

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
  the locked directory. Documented inline in `crates/felis-transport/src/unix.rs`. ✅
- **Startup probe** (`clear_endpoint`): `fs::symlink_metadata` first, so a symlink or a directory at the endpoint fails
  the start untouched; a socket is connected to and unlinked only on `ENOENT` / `ECONNREFUSED` with its peer uid checked
  first. ✅
- **Font reads** (`memmap2::Mmap::map` in `felis-shaping`; see
  [text-shaping.md](../explanation/rendering/text-shaping.md)): Discovered system font files are root-owned and
  read-only. ✅
- **PTY file descriptors**: `felis-pty` opens slave descriptors with `O_CLOEXEC` and flags master descriptors with
  `FD_CLOEXEC` immediately after allocation (`crates/felis-pty/src/unix.rs`), preventing leaks into unrelated child
  processes. ✅
- **`t=t` temporary file path** (Kitty graphics): Uses pinned parent descriptors with
  `openat(parent, name, RDONLY | NOFOLLOW)` and `unlinkat` (`crates/felis-daemon/src/graphics/image_decode.rs`). On
  Windows (`x86_64-pc-windows-msvc`), `t=f`, `t=t`, and `t=s` are rejected with `ENOTSUP`; producer `a=q` probes fall
  back to direct transfer (`t=d`). Design rationale is documented in
  [kitty-graphics.md](../explanation/protocols/kitty-graphics.md). ✅

## OS hand-off audit (standing)

Every site handing terminal-supplied strings to an OS launcher is audited to ensure payloads reach the system as data
rather than shell command lines:

- **OSC 8 activation** (`felis-client`'s `hyperlink::open_url`): Grid strings cross `felis-client-core`'s typed boundary
  `ActivationTarget::parse`, which re-validates scheme allowlists and rejects interior NULs, control characters, and
  bidi overrides before reaching `open_url`. Linux and macOS invoke `xdg-open` / `open` passing the target as an
  isolated argv element. Windows invokes `ShellExecuteW` directly with `lpParameters = null`; no `cmd /c start` path
  exists. The invocation runs on a dedicated worker thread. ✅

## `felis-protocol` crate-purity audit (standing)

As documented in the architecture overview ([overview.md](../explanation/architecture/overview.md)), `felis-protocol`
represents the cross-language wire vocabulary surface: it must contain no runtime or operating system dependencies.

- **Direct dependencies** (production): `serde`, `bitflags`, `thiserror`, `bytes`, `prost`, and `prost-types` (plus
  optional `schemars` behind the `schema` feature). All production dependencies are synchronous and OS-agnostic. ✅
- **Transitive dependencies**: Verifying `cargo tree -p felis-protocol --edges normal,build` confirms no dependency on
  `tokio`, `mio`, `libc`, `rustix`, `nix`, `windows-sys`, `core-foundation`, or `objc`. ✅
- **Source tree**: `crates/felis-protocol/src/` contains no `std::os::*`, no `cfg(unix)`, and no async primitives.
  Binary row payloads conform to golden vectors in [row-codec.md](row-codec.md). ✅
- **Dev dependencies**: Benchmarking and fuzzing utilities (`criterion`, `proptest`) are restricted to non-production
  targets. ✅

This invariant is enforced by the `crate_purity_no_async_or_os_deps` integration test in `crates/felis-protocol/tests/`.
If a pull request adds `tokio`, `async-std`, `mio`, `libc`, `rustix`, `nix`, or any `windows-sys` / `core-foundation` /
`objc` crate to the production dependency graph, the test fails.
