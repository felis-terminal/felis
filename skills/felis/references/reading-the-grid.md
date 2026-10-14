# Reading the grid precisely

Read this when you click on text, match against a fullscreen TUI, or see image placeholders in a capture.

A character's index in `text` is not its column: a wide character fills two columns and a combining mark none. To click
text, take a `col_spans` segment `[row, start, end]` from `search --source visible` and pass its row and a column inside
`start..end` as they are; `--mouse` cells are 0-based like the segment:

```sh
felis sessions search "$ID" --source visible --format jsonl 'Save'
# {"v":1,"line_index":4,"text":"   Save","byte_spans":[[3,7]],"col_spans":[[4,3,7]]}
felis sessions send "$ID" --mouse click:3,4
```

`send --mouse` exits `1` with `invalid_request` and sends nothing when the program has no mouse reporting on (drive it
with `--key` instead) or the cell is off the grid.

The default search skips the alternate screen, where a fullscreen TUI lives; `search --source visible` reads it, and
every segment it returns is on a live row. A daemon too old for `--source visible` makes it exit `2` with `unsupported`.

A Kitty image drawn with Unicode placeholders captures as runs of U+10EEEE plus combining diacritics; filter those rows
out of text you match, and treat their presence as the evidence that an image was placed.
