---
title: Configuration
sidebar:
  order: 13
---

The declarative configuration system ([reference/config.md](../reference/config.md)) governs how client instances read
and apply user preferences: one shared document at the `ProjectDirs` path, lenient parsing and additive enum growth, the
three-way split between document, effective config, and diagnostics, file-valued paths resolved against `config.toml`,
and `font.size_px` in logical pixels.

## One config document, no per-client file

The base config and the `[client.<name>]` overlays live in one `config.toml` ([config.md](../reference/config.md)): one
place to read the effective config, and the sections shared between clients cannot drift apart between copies.
Per-client files are the rejected alternative; they would reintroduce exactly that drift.

_Revisit:_ a client appears whose config legitimately cannot ship in the shared document (for example, a sandboxed store
with no access to it), or a second first-party client ships whose top-level vocabulary diverges from the GUI's, meaning
a section one client owns and the other must not read. The second case is what `config.d/<client>.toml` drop-ins would
answer: with the vocabularies disjoint, one file per client buys back the strict per-file validation the shared document
gives up, and nothing is left to drift.

## Where that document lives is `directories::ProjectDirs`' answer, unedited

felis joins `config.toml` onto `ProjectDirs::from("", "", "felis").config_dir()` and adjusts nothing per platform, so
the rows of the frozen table in [config.md](../reference/config.md) are that crate's conventions rather than felis',
doubled Windows `config\` segment included. One maintained resolver decides where the file goes, and felis already takes
its answer for the cache directory everywhere and for the runtime and state directories on the platforms where the crate
populates those slots ([workspace.md](../reference/workspace.md) "Filesystem layout").

Two per-platform adjustments are rejected:

- **Flattening the Windows path to `%APPDATA%\felis\config.toml`.** It is a felis-specific override of a resolver felis
  otherwise trusts, so every future `ProjectDirs` call in the tree would have to remember to mirror it, and the two
  would drift apart silently the first time one did not.
- **An XDG fallback on macOS.** Beyond that override, it buys a precedence rule and a two-candidate `config path`
  output, when a symlink from the `ProjectDirs` location already answers "my dotfiles live in `~/.config`" with neither.

## Lenient config parsing is the permanent contract, not a transition aid

Because the document is shared, no single consumer owns its full vocabulary: another felis frontend or an external tool
may legitimately read and write sections this client does not know. So an unknown key is never a parse error: the file
keeps applying, and every ignored key is warned with its full path so a typo never goes silently dead
([config.md](../reference/config.md) "Behavior on missing / malformed values" is the normative matrix).

Strict `deny_unknown_fields` parsing is the rejected alternative: it zeroes every valid setting alongside one typo, and
it makes the shared document unreadable the moment any other consumer adds a key. The generated JSON schema is
deliberately non-strict for the same reason: it exists for editor completion and hover, and the runtime parser stays the
authority; the editor is also where typos surface loudest.

## An enum's vocabulary grows additively too, one field at a time

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

## The config crate splits three ways: document, effective config, diagnostics

`ConfigDocument` is the parsed input (raw TOML plus the file it came from), `EffectiveConfig` is one client's view of it
with defaults filled in and its `[client.<id>]` overlay folded in, and `ConfigDiagnostics` is the complete error/warning
set the resolution produced. Validation collects diagnostics and logs nothing; the live client's load path logs the set
once, and `felis config check` prints the same set and derives its exit code from it
([config.md](../reference/config.md) "Behavior on missing / malformed values").

One type doing all three jobs is the rejected alternative: when each check reaches for `tracing` where it stands, the
only way to see a document's problems is to load it and read the log, which gives a diagnostic verb nothing to render
and no way to count errors. What each caller did about the problems stays out of the set and gets a line of its own
(startup used the defaults, a reload kept the live config), because that is the one thing a diagnostic cannot know, and
folding it in would make the same document report differently depending on who read it.

The document type is separate from the effective config for the second reason: one document resolves more than once (a
different client's overlay), and only the document knows which file it came from, which is what path-valued keys resolve
against.

## File-valued keys resolve against `config.toml`, not the process's working directory

A `shader.post = { file = "fx/glow.wgsl" }` names the file beside the config that mentions it, and a `pipe` binding's
`{ file = "dumps/region.txt" }` sink writes beside it, whatever directory felis was launched from; absolute paths and a
leading `~/` are unaffected.

Resolving against the working directory is the rejected alternative: a config is a durable document read by a GUI client
the user starts from a launcher, a terminal, or a WM keybinding, so the working directory is effectively arbitrary; the
same file would load a shader from one and not the other. It also makes a config unmovable between machines, since a
relative path only works from one place.

A missing shader file stays a **warning**: felis renders without that stage, matching the fall-back-and-warn rule for
every other malformed value, and the same document may legitimately be shared with a machine where the file exists.

## `font.size_px` is logical pixels, and the code says so at the DPI boundary

The key is multiplied by the window's scale factor, so one value holds across a HiDPI and a standard-DPI monitor. The
unit is frozen, and the naming carries it: config and client speak `*_logical_px`, the renderer speaks `*_physical_px`,
and the multiply is the one place they meet. A single `_px` name on both sides of that multiply is the rejected
spelling: it reads as correct at every site while a Retina window rasterizes glyphs at half size, which is the bug class
the names make visible.

Points were rejected. kitty, Ghostty, WezTerm and Alacritty size fonts in points, which reach pixels through a per-OS
convention, 96 dpi on Linux and Windows and 72 on macOS, so their `14` is 18.67 px on Linux and 14 px on macOS. A
logical pixel is one size on every OS. The cost is a conversion when copying a point size from another terminal, which
the config reference states.
