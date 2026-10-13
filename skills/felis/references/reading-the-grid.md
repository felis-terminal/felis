# Reading the grid precisely

Read this when you aim a mouse report at text, match against a fullscreen TUI, or see image placeholders in a capture.

A character's index in `text` is not its column: a wide character fills two columns and a combining mark none. To aim a
mouse report (1-based) at text, take a `col_spans` segment on a live row (`row >= 0`) and add 1 to its row and start
column.

The default search skips the alternate screen, where a fullscreen TUI lives; `search --source visible` reads it, and
every segment it returns is on a live row. A daemon too old for `--source visible` makes it exit `2` with `unsupported`.

A Kitty image drawn with Unicode placeholders captures as runs of U+10EEEE plus combining diacritics; filter those rows
out of text you match, and treat their presence as the evidence that an image was placed.
