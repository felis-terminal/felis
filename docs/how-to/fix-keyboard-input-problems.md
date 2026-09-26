---
title: Fix keyboard input problems
sidebar:
  order: 10
---

Override what a key sends when the operating system composes it into the wrong bytes, using
[`send_string` bindings](../reference/keybindings.md) in the `[keymap]` section of `config.toml`. Find your symptom
below.

## The JIS yen key types ¥ instead of a backslash on macOS

On Japanese JIS keyboards under macOS, pressing the yen keycap may produce `¥` (U+00A5) instead of `\` (U+005C), even
when macOS is configured to output a backslash (System Settings → Keyboard → Input Sources → Japanese → "character to
input with the ¥ key"). That setting routes through the input method editor (IME), which passes characters through an
event channel that winit ignores outside active compositions. As a result, felis receives U+00A5.

To map the key to a backslash, add a keybinding:

```toml
[keymap]
"¥" = { kind = "send_string", text = '\', escapes = "none" }
```

Use a TOML single-quoted literal string so that the backslash is treated as literal text rather than an escape sequence,
and specify `escapes = "none"` so that felis does not interpret it as an escape. Under the default
`escapes = "c_style"`, write `text = "\\\\"`.

This binding matches on the composed character `¥`, so it also remaps Option+Y on US/ABC keyboard layouts.

For the windowing library rationale and upstream issue details, see the input architecture guide in
[input.md](../explanation/input.md).

## Modifier combinations reach applications as plain keys

When an application supports the Kitty keyboard protocol, felis encodes extended modifier combinations (such as
Shift+Enter) accurately. Some applications fail to enable the protocol because they check terminal name allowlists
rather than querying terminal capabilities dynamically. To resolve this via terminal identification, see the allowlist
guide in [fix-terminfo-problems.md](fix-terminfo-problems.md).

Alternatively, bind specific key combinations directly using `send_string`:

```toml
[keymap]
"shift+enter" = { kind = "send_string", text = '\e\r' }
```

`\e\r` decodes under the default `escapes = "c_style"`.

This binding intercepts the key chord before encoding and sends the raw byte sequence instead of the Kitty keyboard
protocol sequence. Bind only chords that specific target applications require.
