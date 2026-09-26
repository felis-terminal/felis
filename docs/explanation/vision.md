---
title: Vision
sidebar:
  order: 1
---

felis is a terminal for an existing toolkit, not an environment to move into. It owns the session and its display;
layout and workflow remain with the window manager, the shell, and external programs.

## The terminal's job

A terminal connects running programs to a screen and an input device. felis treats the lifetime of that connection as
separate from the lifetime of a window: closing the display should not terminate the work behind it. Its daemon owns the
PTY, terminal state, and scrollback; a client attaches to that state and renders it.

Rendering belongs inside the same boundary. Applications should be able to use modern terminal protocols without an
intermediate emulator reducing their output to what it can reproduce. In felis, Kitty graphics and text sizing are part
of the terminal's responsibility, not reasons to give up persistent sessions.

Persistence here means surviving a closed window or a disconnected client. The daemon holds live state in memory; it is
not a checkpoint system for restoring processes after a reboot.

## The surrounding tools

Layout belongs to the window manager and workflow to the shell and external programs; each felis window displays one
PTY. felis exposes typed CLI commands and a versioned IPC so those programs can create sessions, send input, read
output, and consume events without their logic running inside the terminal. Utilities built specifically for felis are
separate tools on the same terms, whoever maintains them. The reasoning is in
[A drawn boundary](design.md#a-drawn-boundary) and
[Composition across a process boundary](design.md#composition-across-a-process-boundary).

## Persistence without a multiplexer

A terminal paired with tmux stacks two terminal emulators, so protocol support depends on both layers and their
passthrough behavior. felis puts session ownership in its own daemon and transfers terminal state to an attaching client
instead of replaying it, which keeps persistence inside the terminal without a multiplexer's layout responsibilities
([Correct by construction, not by replay](design.md#correct-by-construction-not-by-replay)).

## Who it fits

felis is intended for people who already choose their tools independently and want the terminal to participate in that
arrangement. A window manager that handles multiple application windows well is important to its ergonomics; workflows
built around several shells sharing one terminal window are a poor fit.

This is a boundary on responsibility, not a promise of a small implementation or a sparse display. The terminal can
render rich output and own a substantial session daemon while leaving the surrounding environment alone. The
[comparison page](comparison.md) places that choice alongside other terminals.
