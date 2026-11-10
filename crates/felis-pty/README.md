# felis-pty

The async PTY layer of [felis](https://github.com/felis-terminal/felis), a daemon-backed terminal emulator for
persistent sessions and headless control: `posix_openpt` on Unix and `ConPTY` on Windows, bridged to Tokio.

## Sessions

`spawn` starts a `Command` on a new terminal of a given `Size` and returns its `PtyParts`, one handle per concern:

- `eof`: resolves when the session's output has ended.
- `writer`: the child's input, an `AsyncWrite`.
- `child`: hangs up, kills, and reaps the child.
- `resizer`: resizes the terminal and reports its foreground process group.
- `quiescer`: parks the session's threads for a hand-over.

Output does not come through an async read. Each session runs three OS threads: a reader on the terminal, a parse thread
that calls the caller's `ByteSink` with every byte the child wrote, and a writer that feeds queued input to the child.
`PtyEof` resolves only after the sink's last call, so an owner that sees the end has seen all the output.

## Backpressure

On the output side, the reader stops reading once 1 MiB of output waits for the sink; the child's writes then block in
the kernel. A sink that blocks therefore slows the child down, and loses nothing.

On the input side, bytes the OS has not taken yet are counted against `PTY_WRITE_PENDING_CAP`. At the cap, an
`AsyncWrite` write waits, and `PtyWriter::write_owned` drops the bytes and says so. A write that carries a
`WriteReservation`, the caller's own admission token, always queues.

## Teardown

Dropping `ChildHandle` never signals the child. The child ends when it exits on its own or when the owner calls `hangup`
(`SIGHUP` to its process group on Unix) or `kill`, and the owner reaps it with `try_wait`. On Windows, dropping the
`Resizer` also closes the pseudoconsole, which ends the child's console.

## Platform differences

- Windows runs shells on a `ConPTY` host this crate bundles, or on the system's own host when the process is not at
  Medium integrity. `set_conpty_dir` names the directory the bundled host is extracted to; it refuses every spawn until
  that is set.
- `Resizer::foreground_pgrp` is `None` on Windows.
- `Resizer::output_drained` is always `true` on Windows, where output the host has not rendered yet cannot be told apart
  from output already delivered.

## Hand-over

The `handover` module moves a running session into another process without ending the child: `Handover::of` takes the
session's terminal and child once the `quiescer` has parked it, and the receiving process rebuilds an `Adopted` session
and starts it with a new sink. On Unix the master fd crosses `execve`; on Windows the pseudoconsole's handles are
duplicated into a separate successor process, which only the bundled host supports.
