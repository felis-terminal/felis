---
title: Accessibility tree
sidebar:
  order: 16
---

The GUI client exposes each window to the platform's assistive technology through
[AccessKit](https://github.com/AccessKit/accesskit): UI Automation on Windows, NSAccessibility on macOS, and AT-SPI on
Linux. Why felis builds the tree, and what it leaves to the screen reader, is
[accessibility.md](../explanation/accessibility.md).

## Nodes

| Node     | AccessKit role | AT-SPI     | UI Automation | NSAccessibility | Name               |
| -------- | -------------- | ---------- | ------------- | --------------- | ------------------ |
| Window   | `Window`       | `frame`    | Window        | `AXGroup`       | the window title   |
| Terminal | `Terminal`     | `terminal` | Document      | `AXTextArea`    | `Terminal`         |
| Text run | `TextRun`      | (internal) | (internal)    | (internal)      | none; carries text |

The Terminal node holds the focus and carries the text interface: its value is the text of its runs in order. Each run
holds at most 255 characters of one row, and the runs of a row chain into one line. Character and line bounds are in
physical pixels relative to the window, sized by the cells each character covers.

## Text

The text is the viewport: the rows on screen, top to bottom, including scrollback rows while the viewport is scrolled
over them. Rows outside the viewport are not in the tree.

| Grid content                                     | Text                                                                   |
| ------------------------------------------------ | ---------------------------------------------------------------------- |
| A character in a cell                            | that character, one per grapheme cluster                               |
| A wide character, or OSC 66 text spanning cells  | one character whose bounds cover the columns it spans on its first row |
| A blank cell inside the row                      | a space                                                                |
| Blank cells at the end of the row                | nothing, except up to the caret or a selection end on that row         |
| The end of a row the next row does not wrap into | a line break                                                           |
| A soft-wrapped row                               | joins the next row with no line break                                  |
| A soft-wrapped row that is entirely blank        | one space                                                              |

Word boundaries fall where the character class (word, whitespace, punctuation) changes from one character to the next,
and a word continues across a soft wrap.

## Caret and selection

| State                                                            | Text selection                                           |
| ---------------------------------------------------------------- | -------------------------------------------------------- |
| A linear selection                                               | its anchor to its focus, in the direction it was dragged |
| No linear selection; cursor visible, viewport at the live bottom | a collapsed selection (the caret) at the cursor          |
| Otherwise                                                        | none                                                     |

A block selection is not reported; the caret stands in for it.

## Lifetime and updates

The client builds no tree until an assistive technology first queries the window, and stops sending updates when the
platform adapter reports that none is listening. While active, a new frame, a title change, a selection change, or a
resize schedules a redraw, and that redraw sends an update carrying only the nodes that differ from the last one.

felis announces nothing on its own: whether and how a change is spoken is the screen reader's decision, made from the
platform's text-change and caret events.
