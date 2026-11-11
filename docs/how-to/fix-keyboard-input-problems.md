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
input with the ¥ key"), because that setting acts through the input method, which felis does not see outside a
composition.

To map the key to a backslash, add a keybinding:

```toml
[keymap]
"¥" = { kind = "send_string", text = '\', escapes = "none" }
```

Use a TOML single-quoted literal string so that the backslash is treated as literal text rather than an escape sequence,
and specify `escapes = "none"` so that felis does not interpret it as an escape. Under the default
`escapes = "c_style"`, write `text = "\\\\"`.

This binding matches on the composed character `¥`, so it also remaps Option+Y on US/ABC keyboard layouts.

For the upstream cause, see [input.md](../explanation/input.md).

## Modifier combinations reach applications as plain keys

felis encodes modifier combinations such as Shift+Enter for applications that enable the Kitty keyboard protocol. Some
applications enable it only for an allowlist of terminal names; to get on it, see
[fix-terminfo-problems.md](fix-terminfo-problems.md).

Alternatively, bind specific key combinations directly using `send_string`:

```toml
[keymap]
"shift+enter" = { kind = "send_string", text = '\e\r' }
```

`\e\r` decodes under the default `escapes = "c_style"`.

This binding intercepts the key chord before encoding and sends the raw byte sequence instead of the Kitty keyboard
protocol sequence. Bind only chords that specific target applications require.
