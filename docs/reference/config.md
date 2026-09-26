---
title: Configuration
sidebar:
  order: 2
---

The `config.toml` schema: where the file lives on each platform, every key with its default, and what reloads live.

felis reads its TOML configuration from the platform-native directory resolved via
[`directories::ProjectDirs`](https://docs.rs/directories) at startup:

| Platform | Config file path                                                             |
| -------- | ---------------------------------------------------------------------------- |
| Linux    | `$XDG_CONFIG_HOME/felis/config.toml` (default `~/.config/felis/config.toml`) |
| macOS    | `~/Library/Application Support/felis/config.toml`                            |
| Windows  | `%APPDATA%\felis\config\config.toml`                                         |

On macOS, files under `~/.config/felis/` are not read. Move or symlink the file to
`~/Library/Application Support/felis/config.toml`. On Windows, the path includes an explicit `config\` segment.

This table is frozen for 1.0. Each row is whatever `ProjectDirs` returns with `config.toml` joined onto it; felis
overrides no platform's answer. The reasoning and the rejected alternatives are in
[control-surfaces.md](../explanation/architecture/control-surfaces.md).

`felis --config <path>` overrides discovery for a single invocation and window. An explicitly selected path that does
not exist produces an error. A missing file at the default path falls back to built-in defaults.

Two settings take a path: `shader.post.file` ([shaders.md](shaders.md)) and a `pipe` binding's `{ file = "<path>" }`
sink ([keybindings.md](keybindings.md) § "Unbound by default"). Both resolve relative to the `config.toml` containing
them, unless absolute or prefixed with `~/`. A config with no file behind it, which is a document parsed from text
rather than read from a path, resolves a relative path against the process's working directory.

The configuration schema is additive: unknown keys produce warnings and are ignored, allowing forward-compatible files
across versions. Six keys take a token from a closed set and carry a documented default: `cursor.blink`,
`shader.animate`, `shader.post`, `shader.post.builtin`, `window.backdrop`, and `clipboard.osc_52`. A token this build
does not know in one of those warns with the field's full path, takes that field's default, and leaves every other
setting in the file applying. A value of the wrong _shape_ (a number or a table where the key takes a token) is a
structural error that fails the document, as it does in every version.

A `[keymap]` binding's kind and its arguments are not on that list, and follow the per-entry rule instead: an
unrecognized one drops that binding with a warning and the remaining keybindings apply, because a binding has no default
to fall back to ([keybindings.md](keybindings.md)).

## Editor support (JSON Schema)

A JSON Schema (draft 2020-12) describing every field is available at
[schema.json](../../crates/felis-client-core/felis-config.schema.json). It provides completion, documentation on hover,
and validation.

Validation covers the shared base and the `[client.felis]` overlay. The root object and every config section stay open:
none carries `additionalProperties: false`. Sections for other clients accept any key, so an unrecognized section key is
a warning at load time (see the table below) rather than a validation error, and that stays true in 1.0. Nested typed
values are the exception: a `[keymap]` binding is validated by shape, so an unknown field inside one does fail schema
validation even though the loader only warns about it.

To configure schema support in editors using Taplo or VS Code:

```toml
#:schema https://raw.githubusercontent.com/felis-terminal/felis/main/crates/felis-client-core/felis-config.schema.json
```

A local repository path may also be referenced: `#:schema <path>/crates/felis-client-core/felis-config.schema.json`.

## Complete annotated example

```toml
[font]
# Font family name. Omitted: the system monospace font. When the
# system's `monospace` alias names a family that is not installed, the
# first installed of DejaVu Sans Mono, Liberation Mono, Noto Sans Mono,
# Ubuntu Mono, Menlo, and Consolas; failing those, the first installed
# fixed-pitch family by name.
family = "JetBrainsMono Nerd Font"

# Logical pixel size for cell metrics and rasterization.
# Clamped to [4.0, 72.0]. Default: 14.0.
# Either TOML spelling is accepted: 14 and 14.0 are the same size.
size_px = 14

# Explicit font fallback chain resolved per codepoint.
# Omitted (default): felis auto-discovers, in this order, the first
# installed CJK family, every installed symbol family, the first
# installed color-emoji family, and the first installed symbols-only
# Nerd Font; a group with nothing installed contributes no face.
# Non-empty: the declared entries are the whole chain, in order,
# and auto-discovery does not run on top of them.
# Omitting features inherits from primary; features = [] disables.
fallback = [
    { family = "Noto Sans CJK JP", features = ["palt"] },
    { family = "Noto Color Emoji", features = [] },
    { family = "Symbols Nerd Font" },
]

# OpenType feature tags applied to the primary face ("calt", "-liga").
# Default: [] (programming ligatures off).
features = ["calt", "liga"]

# Per-style face overrides. Derived from font.family when omitted.
[font.bold]
family = "JetBrainsMono Nerd Font"

[font.italic]
family = "Maple Mono"

[font.bold_italic]
family = "Maple Mono"
features = []

[theme]
# Default foreground and background colors (#rrggbb).
foreground = "#e5e5e5"
background = "#0d0d12"

[cursor]
# Cursor block color (#rrggbb). When unset, paints in reverse video.
color = "#ffaa00"

# Cursor blink behavior: "program" (default, per DECSCUSR),
# "never" (reduced motion), or "always".
blink = "program"

# Half-period of cursor blink in milliseconds. Minimum 50 ms. Default: 530.
blink_interval_ms = 530

[shader]
# Post-process shader: { builtin = "trail" } or { file = "path.wgsl" }.
post = { builtin = "trail" }

# Shader animation pacing: "never" (default, dirty-only) or
# "focused" (continuous 60 Hz when window has focus).
animate = "never"

[mouse]
# Multiplier for mouse wheel notch scrolling. Clamped to [0.1, 100.0]
# with a warning. Default: 3.0.
scroll_multiplier = 3.0

[clipboard]
# Back clipboard operations with OS clipboard. Default: true.
# When false, uses an in-process clipboard.
use_os_clipboard = true

# Destination for program OSC 52 write requests:
# "mirror" (default, session-local) or "system" (OS clipboard).
osc_52 = "mirror"

[window]
# Prefix prepended to shell window title ("<prefix> <title>").
title_prefix = "[work]"

# Show native window title bar and borders. Default: true.
decorations = true

# Background opacity in [0.0, 1.0], clamped with a warning.
# Default: 1.0 (opaque).
# Values < 1.0 require transparent window surface support at startup.
opacity = 0.95

# OS-native window backdrop material:
#   "none"     (default)
#   "blur"     (macOS NSVisualEffectView; requires opacity < 1.0)
#   "acrylic"  (Windows DWMSBT_TRANSIENTWINDOW)
#   "mica"     (Windows DWMSBT_MAINWINDOW)
#   "tabbed"   (Windows DWMSBT_TABBEDWINDOW)
backdrop = "none"

[theme.palette]
# Base 16 ANSI color overrides (#rrggbb).
black          = "#1d1d22"
red            = "#ff5555"
green          = "#50fa7b"
yellow         = "#f1fa8c"
blue           = "#bd93f9"
magenta        = "#ff79c6"
cyan           = "#8be9fd"
white          = "#bfbfbf"
bright_black   = "#4d4d4d"
bright_red     = "#ff6e67"
bright_green   = "#5af78e"
bright_yellow  = "#f4f99d"
bright_blue    = "#caa9fa"
bright_magenta = "#ff92d0"
bright_cyan    = "#9aedfe"
bright_white   = "#e6e6e6"

[theme.palette.indexed]
# Overrides for 256-color palette indices 16..=255. Keys are strings:
# TOML has no integer keys, so 16 and "16" name the same entry.
# Color schemes often remap the low cube slots 16..=21 for accents.
16 = "#d08770"
17 = "#5e81ac"
18 = "#3b4252"
19 = "#434c5e"
20 = "#d8dee9"
21 = "#eceff4"

[keymap]
# Keybinding overrides layered on platform defaults.
# Modifiers: ctrl, shift, alt, super. Named keys: enter, tab, escape,
# space, backspace, insert, delete, home, end, page_up, page_down,
# up, down, left, right, f1..f35.
# Every binding kind with its fields and accepted values is tabulated in
# keybindings.md; so are the character-key and literal-plus spellings.
"ctrl+shift+f" = { kind = "open_scrollback_search" }
"ctrl+shift+v" = { kind = "paste", from = "system" }
"ctrl+shift+c" = { kind = "copy", what = "system" }
"shift+insert" = { kind = "paste", from = "primary" }
"ctrl+shift+r" = { kind = "reload" }
"shift+page_up" = { kind = "scroll", step = "half_page_up" }
"ctrl+shift+=" = { kind = "font_size", step = "increase" }
"ctrl+shift+-" = { kind = "font_size", step = "decrease" }
"ctrl+shift+z" = { kind = "scroll_to_prompt", to = "previous" }
"ctrl+shift+x" = { kind = "scroll_to_prompt", to = "next" }
# send_string reads backslashes per `escapes`: "c_style" (default,
# recognizing \n \r \t \\ \0 \e \xNN) or "none" to send them verbatim.
"f5"           = { kind = "send_string", text = "clear\n" }
"f6"           = { kind = "send_string", text = "C:\\src\\", escapes = "none" }
"ctrl+shift+]" = { kind = "switch_session", to = "next" }
"ctrl+shift+[" = { kind = "switch_session", to = "previous" }
"ctrl+shift+n" = { kind = "new_session" }
"ctrl+shift+h" = { kind = "pipe", source = "scrollback", ansi = true }
"ctrl+shift+g" = { kind = "pipe", source = "visible", target = { command = ["bat", "--paging=always"] }, ansi = true }
"ctrl+shift+y" = { kind = "pipe", source = "selection", target = "clipboard" }
"ctrl+shift+p" = { kind = "run", command = ["felis-pick-session"] }
"ctrl+shift+0" = { kind = "unbind" }
```

The default keymap table, every binding kind's fields and accepted values, and the actions that ship unbound are in
[keybindings.md](keybindings.md).

## Per-client overrides (`[client.<name>]`)

`config.toml` supports client-specific overlays. Top-level sections form the shared base; a `[client.<name>]` table
merges over the base when the client matching `<name>` loads the file (the GUI client is `felis`):

```toml
[font]
family = "JetBrains Mono"
size_px = 14.0

[client.felis.font]
size_px = 16.0
```

Merge rules:

- **Tables merge recursively**: keys defined in the overlay override or add to the base; omitted keys inherit base
  values.
- **Arrays and scalars replace wholesale**: array or primitive values in an overlay replace base values entirely.
- **Other client sections are inert**: sections for different client identifiers are carried untouched by the active
  client, whatever they contain. Unknown keys and values of the wrong type there produce no diagnostic and cannot stop
  the load. The published JSON Schema draws the same line: it validates `[client.felis]` and leaves every other client's
  section open. Frozen for 1.0.
- **One nesting level only**: nested `client` tables inside overlays are rejected with a warning.

## Behavior on missing / malformed values

Diagnostics surface at two stages:

- **load**: detected during file reading and parsing before window setup.
- **window**: detected by renderer, shaper, or windowing system when applying values.

| Situation                                                | Action taken                                                       | Stage  |
| -------------------------------------------------------- | ------------------------------------------------------------------ | ------ |
| Config file absent at default path                       | Uses built-in defaults. Refuses if specified via `--config`.       | load   |
| Config file unreadable or malformed                      | Uses defaults and logs error. Refuses if specified via `--config`. | load   |
| Unknown field in root or overlay                         | Skips field and logs warning; remaining configuration applies.     | load   |
| Unknown token for a closed-enum key listed above         | Falls back to that field's default with warning; the rest applies. | load   |
| Unknown `shader.post` selector or `builtin` name         | No post-process pass, with warning; the rest applies.              | load   |
| Non-table `[client.<name>]`                              | Skips overlay with warning; base configuration applies.            | load   |
| `[client.<name>]` for other client                       | Ignored without warning, whatever it contains.                     | load   |
| `clipboard.osc_52` with OS clipboard disabled            | Inert; logs notice at startup.                                     | window |
| `window.opacity` out of range                            | Clamped to `[0.0, 1.0]` with warning.                              | load   |
| `window.opacity` not a number (`nan`)                    | Renders opaque with warning.                                       | load   |
| `window.opacity` < 1.0 on opaque platform                | Renders opaque with warning.                                       | window |
| `window.backdrop` unsupported by host OS                 | Ignored with diagnostic notice.                                    | window |
| Path-valued key target missing                           | Skips field and logs warning; remaining configuration applies.     | load   |
| `font.size_px` non-positive or NaN                       | Falls back to default with warning.                                | load   |
| `font.size_px` outside `[4.0, 72.0]`                     | Clamped into valid range with warning.                             | load   |
| `font.family` unavailable                                | Falls back to system monospace with warning.                       | window |
| No monospace face installed                              | Window fails to open with `no monospace font installed`.           | window |
| `font.fallback` entry missing                            | Skips missing entry; remainder of chain applies.                   | window |
| `font.fallback` entry malformed                          | Skips entry with warning; remainder of chain applies.              | load   |
| Font style sub-table omitted                             | Derived from `font.family` at matching weight and slant.           | window |
| Color value malformed                                    | Keeps default color and logs warning.                              | window |
| `theme.palette.indexed` key not an integer in `16..=255` | Ignores entry with warning; slots 0-15 have named fields.          | load   |
| Malformed keymap chord, binding, or binding argument     | Skips entry and logs warning; remaining keybindings apply.         | load   |
| `mouse.scroll_multiplier` outside `[0.1, 100.0]`         | Clamped into valid range with warning.                             | load   |
| `mouse.scroll_multiplier` not a number (`nan`)           | Falls back to default with warning.                                | load   |

A load-stage row that names a warning is one `felis config check` reports; rows that name none are silent. Every warning
the check can report appears above, either as a row in this table or, for a nested overlay, under the merge rules.
Window-stage rows surface only when the client applies the value, so the check passes on them: an unsupported
`window.backdrop` is a notice from the running window, not a configuration error.

Colors must use 6-digit hex format (`#rrggbb`). Three-digit shorthand (`#rgb`) is rejected as malformed.

## Checking the file (`felis config …`)

Configuration validation verbs operate locally without starting a window or dialing a daemon (see [cli.md](cli.md)
"Config verbs"):

- `felis config path`: prints resolved configuration path.
- `felis config check`: reports diagnostics for all load-stage issues.
- `felis config show-effective`: outputs merged configuration with defaults and active client overlays applied.

Diagnostic categories reported by `felis config check`:

| `kind`         | Description                           | Severity                          |
| -------------- | ------------------------------------- | --------------------------------- |
| `io`           | File read error.                      | Error                             |
| `parse`        | TOML syntax or structural type error. | Error                             |
| `unknown_key`  | Unrecognized key or section.          | Warning                           |
| `value`        | Out of range or unusable value.       | Warning                           |
| `missing_file` | Missing file referenced by path key.  | Warning (Error if via `--config`) |

## Live reload

Modifications to `config.toml` take effect automatically at runtime. A file watcher observes saves and re-applies
changes. Reloading may also be triggered via `Ctrl+Shift+R` (`reload` action).

Live application by property:

| Key                       | Live reload behavior                                                           |
| ------------------------- | ------------------------------------------------------------------------------ |
| `font.*`                  | Re-measures metrics and reflows grid. Resets active Ctrl+Wheel zoom.           |
| `theme.*`, `cursor.color` | Repaints surface and updates presentation state.                               |
| `cursor.blink*`           | Restarts blink timer immediately.                                              |
| `mouse.scroll_multiplier` | Takes effect on next wheel event.                                              |
| `shader.*`                | Rebuilds post-process pipeline.                                                |
| `clipboard.*`             | Rebuilds clipboard subsystem.                                                  |
| `window.title_prefix`     | Updates window title immediately.                                              |
| `window.decorations`      | Toggles OS title bar and borders.                                              |
| `window.opacity`          | Applies live within `< 1.0`. Toggling opaque vs translucent requires relaunch. |
| `window.backdrop`         | Re-applies backdrop material on host window.                                   |
| `[keymap]`                | Rebuilds active keybinding lookup tables.                                      |
| `[client.<name>]`         | Re-applies keys modified in the active client overlay.                         |

On syntax errors during save, live reload preserves the previous valid in-memory configuration and logs warnings without
crashing.

## Terminal identity (environment, not TOML)

Terminal identity variables (`TERM`, `TERM_PROGRAM`) are set by the daemon environment rather than `config.toml`. See
[terminal-identity.md](terminal-identity.md) for defaults and overrides.
