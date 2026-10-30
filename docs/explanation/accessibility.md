---
title: Accessibility
sidebar:
  order: 12
---

This page records why felis exposes the terminal to the platform's assistive technology, why it does so through
AccessKit, and where the line falls between what felis reports and what a screen reader decides. The node roles, the
text mapping, and the update rules live in the [reference twin](../reference/accessibility.md).

## Why felis builds an accessibility tree

Principle 1 admits a capability only when no dedicated tool does it better and a real consumer needs it
([principles.md](principles.md#1-add-only-what-earns-its-place)). Accessibility passes both clauses by construction. The
consumer is the screen reader every desktop OS ships (Narrator, VoiceOver, Orca) or that its users install (NVDA, JAWS);
no felis user has to exist first for that consumer to exist. And no tool outside the window can stand in, because a
screen reader does not read pixels: it queries the accessibility tree of the window that owns them. A native toolkit
builds that tree for free, but felis draws every glyph on the GPU, so its window has no tree unless felis builds one.

The contrast with desktop notifications shows where the boundary sits. felis relays notifications instead of posting
them ([protocols/notifications.md](protocols/notifications.md)), because any process holding the IPC stream can call the
platform notification API as well as felis can. An accessibility query has no such relay: the OS sends it to the window,
and only the process that owns the window can answer.

The intent is the same on every platform while the mechanism differs (UI Automation on Windows, NSAccessibility on
macOS, AT-SPI on Linux), which makes it a platform shim under the cross-platform rule
([non-goals.md](non-goals.md#cross-platform-constraints)), not a feature one OS may ship ahead of the others.

## Why an AccessKit tree

felis builds one platform-neutral tree and lets [AccessKit](https://github.com/AccessKit/accesskit) map it onto each
OS's API, attaching through the winit adapter to the window felis already creates. Two alternatives fail:

- **Hand-written glue per OS**, the route native terminals and Ghostty take, triples the surface felis maintains and
  lets the platforms drift apart, which the platform-shim rule exists to prevent.
- **Speaking directly** (Speech Dispatcher, a built-in reader in the style of tdsr) bypasses the screen reader the user
  has already configured, and with it braille output, the review cursor, and the user's verbosity settings.

Only Orca keys terminal-specific reading on the `terminal` role; NVDA reserves its terminal handling for Windows
Terminal's own UIA class name, and VoiceOver has none, so on Windows and macOS the screen reader reads felis as a text
document. Reporting that class name (`TermControl`, which AccessKit can set) to unlock NVDA's terminal handling would
claim another program's identity, the impersonation
[terminal-identity.md](architecture/terminal-identity.md#why-impersonation-is-not-the-default) rejects, and tie felis to
how NVDA treats Windows Terminal's internals, which neither project publishes as a contract.

## What the tree holds

The tree holds the viewport, never the whole buffer. Exposing the whole buffer as one text value makes each query
proportional to the scrollback: an assistive technology querying Ghostty on macOS, which reports its whole buffer as
visible, waits three seconds against 700K characters where other applications answer in milliseconds
([ghostty#9932](https://github.com/ghostty-org/ghostty/issues/9932)). AccessKit's AT-SPI adapter also diffs the full
text of a node on every update.

The client builds the tree from the shadow screen it already renders, so the daemon carries no accessibility state and
closing the window loses nothing that a reattached client cannot rebuild
([principles.md](principles.md#3-the-daemon-owns-state-the-client-owns-pixels)). The client builds nothing until an
assistive technology first queries the window, so a session with no assistive technology attached pays no cost.

## Announcing finished commands

A screen reader that reads felis as a text document follows the caret and echoes typed characters, but speaks no output:
neither NVDA nor VoiceOver diffs a document's text for new lines. On Windows and macOS felis therefore announces the
output of each finished command itself, through a live node the screen reader queues like any other announcement, so the
user's voice, braille display, and verbosity settings still apply.

The boundaries come from the shell's `OSC 133` marks: output is what lies between a command's `C` and its `D`. Two other
ways of finding output fail Principle 4 ([principles.md](principles.md#4-explicit-over-heuristic)). Windows Terminal
skips text that matches recent keystrokes, and iTerm2 compares the text of the command's row before and after it runs;
both decide what is output from what the cells contain. Tracking the cursor instead (output is what appears below the
row where Enter was pressed) reads no cell content, but cannot tell typing that crosses a soft wrap, or a prompt that
redraws its own row, from output. The cost of taking the marks from the shell is that a program emitting none (a shell
without integration, a REPL) gets no announcements; its output stays readable through the screen reader's review
commands.

The daemon cuts the text while it parses the `D` mark, not the client after the mark reaches it. By the time a window
handles a `D`, the same read can have carried the next prompt or, with typeahead, the next command's output, and output
longer than a screen has scrolled out of the shadow the client holds. Only the parser sees the grid exactly as it stood
at the `D` byte. Where the start of the output cannot be placed exactly anymore, because a program inserted lines above
it or a resize re-wrapped the row it began inside, felis announces nothing rather than a guess.

A window holds an announcement until output has been quiet for a moment instead of announcing per frame, because
VoiceOver cuts a polite announcement off when the next one arrives. With typeahead, a shell writes the second command's
output in a later write than the first command's `D` mark, so the two reach the window in separate frames and per-frame
batching would announce them separately. The wait reads only when output arrived, not what it says, so it stays within
Principle 4.

Linux gets no announcements. The `terminal` role already sends Orca the text-change events its terminal handling speaks
as output, and AccessKit's AT-SPI adapter would speak a live node too, reading every output twice.

## What felis leaves to the screen reader

Apart from finished commands, felis reports which nodes changed between two updates of the tree, comparing cells
mechanically without assigning meaning to them, and leaves the decision of what to speak to the screen reader. Two
refinements other terminals make inside the terminal are therefore out:

- **Suppressing typed-character echo** by matching output against recent keystrokes decides from the content of the
  output whether to report it, which is the heuristic Principle 4 rejects
  ([principles.md](principles.md#4-explicit-over-heuristic)). Screen readers already handle typed characters by their
  own policy.
- **Skipping full-screen redraws** requires classifying a change as a redraw or as new output from what the cells
  contain, the same heuristic. Windows Terminal's maintainers reached the same conclusion for the same reason
  ([microsoft/terminal#13666](https://github.com/microsoft/terminal/issues/13666)).
