---
title: Kitty graphics protocol
sidebar:
  order: 3
---

felis's implementation of the Kitty graphics protocol: wire format, transmission media, placement, animation, and
resource limits.

Architectural design rationale, upstream differences, and implementation tradeoffs are detailed in
[kitty-graphics.md](../../explanation/protocols/kitty-graphics.md). The upstream Kitty graphics specification is
published at <https://sw.kovidgoyal.net/kitty/graphics-protocol/>.

## Wire format

Transmitted via APC sequences:

```
ESC _ G <controls> ; <payload> ESC \
```

`<controls>` is a comma-separated `key=value` list. `<payload>` is base64-encoded image data (for direct transmission)
or a base64-encoded path / shm name (for file or SHM references).

A `t=d` transfer may arrive in chunks, `m=1` on every chunk but the last: the payloads concatenate in arrival order, and
base64 decoding plus any `o=z` inflate run once, over the concatenation. A continuation chunk's controls are ignored
apart from `m=` and a non-zero `q=`, which overrides the first chunk's. An `a=p`, `a=d`, `a=c` or `a=a` command ignores
`m=` and runs at once, even mid-transfer. A delete drops the open transfer; after the others it continues with the next
chunk. A command with both `i=` and `I=` is refused at once and leaves any open transfer alone. felis does not police
the producer's chunking. A chunk whose payload passes the upstream 4096-byte cap is accepted to the
[APC body limit](#limits), and a payload decodes with or without its RFC 4648 padding, which is what kitten emits.

## Transmission methods

Transmission method support (`t=d/f/t/s`) is summarized in the
[protocol support matrix](support-matrix.md#kitty-graphics). On Unix targets, four methods are implemented: `t=d`
(direct), `t=f` (file), `t=t` (temp file), and `t=s` (POSIX shared memory). On Windows (`x86_64-pc-windows-msvc`), only
`t=d` is supported; `t=f`, `t=t`, and `t=s` return `ENOTSUP`, allowing producer `a=q` probes to fall back to direct
transmission; see [kitty-graphics.md](../../explanation/protocols/kitty-graphics.md).

All three Unix methods read the range `S=` bytes long from offset `O=`; an absent or zero `S=` reads to the end. An `O=`
or `O=+S=` past the end of the file or segment is refused with `EINVAL`; a temp file felis could open is deleted even
then.

Shared-memory transfer (`t=s`) behavior:

- The daemon opens named segments read-only and copies the selected range into internal buffers.
- Segment names are not unlinked per read; unlinking is deferred to session teardown. A session defers up to 16
  successfully completed transfers; adding a seventeenth immediately unlinks the oldest name.
- Names are accepted with or without a leading slash. On non-Linux Unix platforms, a leading slash is added
  (`normalize_shm_name`) before `shm_open`. Names containing `/`, `.`, or `..` within the body are rejected with
  `EINVAL`.
- Data copying is split by platform: Linux uses `pread` (safe against concurrent truncations); macOS uses `mmap` (see
  `copy_shm_range_mmap` in `crates/felis-daemon/src/graphics/image_decode.rs`).
- Raw payloads (`f=24`/`f=32`) longer than `s × v × bpp` keep the leading bytes and trim trailing padding (e.g.
  page-boundary rounding on macOS). Payloads shorter than expected return `ENODATA`.

Chunking: `m=` only applies to `t=d`. A command specifying `t=f`, `t=t`, or `t=s` completes immediately regardless of
`m=1` and aborts any active in-flight direct stream. Design details are documented in
[kitty-graphics.md](../../explanation/protocols/kitty-graphics.md).

## Image formats and compression

Supported formats include `f=24` (RGB), `f=32` (RGBA), and `f=100` (PNG, decoded daemon-side). An omitted `f=` is
`f=32`, upstream's default. JPEG and WebP are not supported directly (encode as PNG or raw RGB/RGBA). Zlib (`o=z`)
compression is supported across all transmission media. Feature status is tracked in the
[support matrix](support-matrix.md#image-formats-f-and-compression).

## Lifecycle

Images reside in the daemon's image store and are indexed by:

- `i=<u32>`: Caller-assigned image ID.
- `I=<u32>`: Caller-assigned image number.

Transmissions (`a=t` or `a=T`) may omit both IDs: felis allocates an ID from the top of the `u32` range and sends no
reply. An explicit `i=0` or `I=0` is the same as an omitted key. `a=p` and the ID-targeted deletes (`a=d` with `d=i`/`I`
or `d=f`/`F`) require an explicit ID (see [kitty-graphics.md](../../explanation/protocols/kitty-graphics.md)).

A transmission with `I=` always stores a new image, under the lowest image ID from `1` up that no image holds, and its
reply carries both: `i=<assigned>,I=<number>`. Several images may share a number. Any other command that names an image
by `I=` (`a=p`, `a=f`, `a=a`, `a=c`, `d=f`/`F`) acts on the newest of them and fails with `ENOENT` when there is none; a
put's reply adds the ID it resolved. `d=n` deletes the placements of the newest image with the number (only `p=`'s when
given), and `d=N` also frees that image once none is left. `d=i`/`I` match `i=` alone.

Image data is retained until:

- The producer deletes the image (`a=d`) matching its ID or number.
- The producer transmits again under the same ID: the new data replaces it, and its placements are deleted as in kitty.
- It was sent without `i=` or `I=` and has no placement left after a delete or a scroll; nothing could place it again.
- A screen clear leaves it without a placement (see below).
- A later transmission needs room under the session byte cap and evicts it, placements included (see [Limits](#limits)).
- The session is destroyed.

Detaching a client evicts nothing. Rehydrating an attached session re-transmits the images the store still holds.

A screen clear drops placements as kitty's does: `ED 2`, its selective twin `DECSED 2`, and `RIS` drop every placement
with a row on the screen, `C=1` included, and `ED 3` drops scrollback placements too; `U=1` extents survive both. Each
then frees every image no placement references, transmitted-only images included. Partial erases (`ED 0`, `ED 1`, `EL`)
and `DECSTR` leave placements alone. The daemon sends `PlacementRemoved` and `Delete` to attached clients to drop the
corresponding textures.

Primary and alternate screens maintain isolated placement contexts. Switching to alternate screen (`?1049h`) stashes
primary placements; switching back (`?1049l`) restores them and frees alternate placements.

## Scrollback-anchored placements

Images remain anchored to text across scroll operations. felis models this using signed, live-relative anchor rows: row
`1` represents the top live screen row, row `0` the most recent scrollback row, and `-(n-1)` the `n`-th scrollback row.

Anchor positions shift as text scrolls: `Placements::shift_up` preserves entries within the scrollback ring and evicts
entries pushed beyond the retention horizon. The alternate screen operates with `retain_rows = 0`, evicting placements
as they leave the live viewport.

Scrolling triggers `ImageMsg::PlacementsShifted { lines }` to notify client shadows of anchor adjustments
([ipc.md](../ipc.md)). The client never drops placements unilaterally, waiting for `PlacementRemoved` from the daemon.
Live queries (`ED`, `d=c/p/q/x/y`) operate in live coordinate space, ignoring non-visible scrollback placements unless
their extents cross into the viewport. Coordinate transformation details are in
[kitty-graphics.md](../../explanation/protocols/kitty-graphics.md).

## Display

Action support (`a=`) is detailed in the [support matrix](support-matrix.md#display--lifecycle-a). Supported actions
include `a=t/T/p/q`, `a=d` (basic and extended deletion), and `a=a/f/c` animation actions.

### Placement parameters

- `x`, `y`, `w`, `h`: Pixel sub-region of source image.
- `c`, `r`: Target width and height in terminal cells. An omitted (or zero) axis is the source size in pixels (the `x`,
  `y`, `w`, `h` region when all four are given, clipped to the image) over the cell size, rounded up. Like kitty, felis
  re-resolves it whenever the cell size changes (font size, scale factor), so a naturally sized image keeps its pixel
  size. Until a client has reported a cell size, one pixel counts as one cell.
- `z`: Z-index stacking order (negative behind text, positive above).
- `C=1`: Prevent cursor motion after placement. Without it the cursor lands as in kitty: on the placed cell box's last
  row, one column right of it. A column past the right edge wraps to column 0 one row further down; a row past the
  bottom margin scrolls the scrolling region up by the overshoot. The cursor then stays inside the region if origin mode
  (DECOM) is set and the box's last row was inside it, and inside the screen otherwise. A zero-sized box moves nothing.
  Text written after the placement, even in the same write, prints at the moved cursor.
- `q`: Quiet mode (suppress response acknowledgments).
- `i`, `I`, `p`: Image and placement identifiers. A put with a non-zero `p=` replaces the image's placement under that
  id; a put without one, or with `p=0`, adds a placement of its own, as kitty does, and no `p=` addresses it afterwards.
  On an image sent without `i=` or `I=`, `p=` is ignored.

A placement's destination pixel offsets (`X`, `Y`) parse and are ignored: the image lands at the origin of its anchor
cell. The uppercase pair carries a source offset for `a=c` frame composition, which felis does read ("Animation" below).

## Unicode-placeholder placement

The Unicode-placeholder method (`U=1`) is supported. The daemon decodes diacritic-encoded row, column, and image IDs. A
cell's foreground color is the low 24 bits of its image id; the default foreground counts as `0`. An id of `0` names no
image. A cell continues the run of the cell before it when their foreground ids match and each diacritic it carries
agrees with the run (the same row, the next column, the same high byte); a continuing cell takes the diacritics it omits
from the run, and a cell that starts a run takes `0` for them. Clients render placeholder cells with corresponding image
regions matching direct placement z-index rules. The image is fit into the placement's `c=` by `r=` cell box with its
aspect ratio kept and centered, so cells in the margin show nothing.

A `U=1` transmission (`a=T`) or put (`a=p`) anchors nothing at the cursor and leaves it in place; it records placement
extents (`c=`, `r=`, `z=`) in the session's placement table (keyed by image ID). Rehydration replays recorded extents to
preserve placeholders across reattachments. An omitted `c=` or `r=` resolves when the client draws, with its own cell
size. Extents persist until their backing image is freed, and transfer across screen buffer transitions
(`?1049h`/`?1049l`).

## Animation

felis supports `a=f` (frame transmission and editing), `a=a` (animation control), `a=c` (frame composition), and
`a=d, d=f/d=F` (frame deletion). Semantics follow the upstream Kitty animation specification.

### Frame model

Images maintain ordered lists of frames; `frames[0]` is the root image (frame 1 in Kitty 1-based numbering), and `a=f`
appends or modifies subsequent frames. felis stores pre-coalesced frames where each frame is a complete `width × height`
buffer composite of base frame (`c=`) and new rectangle (`x, y, s, v`). Storage specifics are detailed in
`crates/felis-daemon/src/graphics/` and [image-store.md](../../explanation/data-model/image-store.md).

### `a=f`: Transmit or edit a frame

- `r`: 1-based frame number to edit; omitted (or `N+1`) appends a frame.
- `c`: 1-based source frame for compositing; omitted fills with background color `Y` (32-bit RGBA, default transparent
  black).
- `x`, `y`, `s`, `v`: Offset and dimensions within frame canvas.
- `C`: Composition mode (`C=1` overwrites pixels; default alpha-blends).
- `z`: Frame duration in milliseconds (`z>0` sets duration; `z<0` marks frame gapless; `z=0` or omitted defaults to 40
  ms).
- Standard transmission keys (`f`, `t`, `o`, `m`) apply.

### `a=a`: Animation control

- `s`: Execution state: `s=1` stop (resets loop count); `s=2` run in loading mode (halt at end until more frames
  arrive); `s=3` loop.
- `v`: Loop counter: stored as `v − 1` bound (`v=1` loops infinitely; `v=N` plays `N − 1` iterations).
- `c`: 1-based frame to display immediately.
- `r`, `z`: Updates frame `r` duration to `z` milliseconds.

### `a=c`: Frame composition

Blends a `w × h` rectangle from source frame `r` (offset `X, Y`) onto destination frame `c` (offset `x, y`), using
composition mode `C`. `w` and `h` default to full image bounds.

### `a=d, d=f` / `d=F`

Deletes frame `r`; `d=F` frees backing pixel allocations if unreferenced. The root frame (frame 1) cannot be deleted
while additional frames exist.

### Playback and wire protocol

The daemon manages animation timers, advancing active animations by wall-clock intervals, skipping gapless frames, and
looping according to configured bounds. Animations continue advancing when detached.

Frame transitions emit `ShowFrame { id, number }` to clients. Frame pixel data transfers once via `Header` → `Chunk*` →
`Complete` sequences using 1-based Kitty frame numbers ([ipc.md](../ipc.md#image-kind--3)). Clients track frame
presentation without computing tick intervals locally.

## Z-ordering

Image layers stack relative to text according to Kitty rules:

1. Window background fill.
2. Images with `z < -1073741824` (`INT32_MIN / 2`), in z order.
3. Cell backgrounds.
4. Images with `-1073741824 <= z < 0`, in z order.
5. Cell glyphs.
6. Images with `z >= 0`, in z order.
7. Cursor.

Interface chrome (search, confirmation, or link preview bars) clips all image rendering; images do not draw into chrome
rows at any z-index (see [kitty-graphics.md](../../explanation/protocols/kitty-graphics.md)).

## File transmission safety (`t=f` / `t=t`)

File-based transfers enforce path traversal defenses before I/O:

- Paths must be absolute and contain no `..` traversal segments.
- `t=f` opens the path with `O_NOFOLLOW`, so a symlink as the final component is refused.
- `t=t` opens the parent directory first, then opens the file with `openat` and `O_NOFOLLOW` relative to that directory
  descriptor, and always deletes the file afterwards with `unlinkat` on the same descriptor.

`t=f` accesses any absolute path readable by the daemon's effective UID; threat model and privilege boundaries are
detailed in [security-model.md](../../explanation/security-model.md).

## Replies and errors

A reply is an APC of the request's shape:

```
ESC _ G <id keys> ; <status> ESC \
```

The id keys echo whichever of `i=<image id>`, `I=<image number>` and a non-zero `p=<placement id>` the request carried,
in that order; a transmission or put by `I=` adds the `i=` it resolved to, and an `a=f` reply ends with the non-zero
`r=` it carried. `<status>` is `OK` on success, and `<code>` or `<code>:<message>` on failure; in a message `;`, C0
bytes and DEL are replaced with `_`, so a message can neither end the APC early nor put bytes of its own on the PTY.

A command carrying both a non-zero `i=` and a non-zero `I=` is refused with `EINVAL` before its action runs, a delete
included. Otherwise, only a command with a non-zero `i=` or `I=` is answered, as in kitty; a command without one has
nowhere to address the reply, and the reply would reach the shell as text. A query (`a=q`) is answered only when it
carries `i=`, and its reply echoes `i=` alone. A command with an unknown `a=` is dropped. Among answered commands,
[`q=`](#response-suppression-q) decides.

| Code      | Cause                                                                         |
| --------- | ----------------------------------------------------------------------------- |
| `ENOENT`  | The command names an image or frame that does not exist                       |
| `EINVAL`  | A control value felis cannot use, or base64 or zlib data that does not decode |
| `ENODATA` | Decoded pixel data shorter than `s=`×`v=` requires                            |
| `EBADPNG` | `f=100` data that does not decode as PNG                                      |
| `EBADF`   | A `t=f`, `t=t` or `t=s` source that cannot be opened or read                  |
| `ENOTSUP` | A limit, an unsupported medium, or a feature felis does not implement         |

### Querying

Producers query capabilities with `a=q`. Probes invoke the decoder path to guarantee parity with transmission. On Unix
targets, direct (`t=d`), file (`t=f`), temporary file (`t=t`), and shared memory (`t=s`) report success; on Windows,
only `t=d` succeeds. RGB, RGBA, PNG formats, zlib compression, Unicode placeholders, and animation controls report
support identically across all supported targets.

### Response suppression (`q=`)

The `q=` parameter controls daemon acknowledgments:

| `q=` | Behavior                            |
| ---- | ----------------------------------- |
| `0`  | All responses emitted (OK + errors) |
| `1`  | Errors emitted; success suppressed  |
| `2`  | All responses suppressed            |

Values above 2 clamp to `q=2`. A successful `a=a` (animation control) is never acknowledged; its errors follow the `q=`
table. A delete (`a=d`) is never answered, whether it succeeds or fails, unless it carries both `i=` and `I=`.

### Malformed input

A `G` escape whose controls do not parse is dropped without a reply. A chunked `t=d` transfer exceeding 64 MiB is
answered with `EINVAL`, honoring the first chunk's ids and the `q=` in effect.

Non-`G` APC escapes are silently ignored to prevent interfering with unrelated APC protocol handlers.

## Limits

- Maximum APC body: 256 KiB between the `ESC _` introducer and the terminating `ST`, controls and payload together,
  kitty's own escape-code limit. A longer body is truncated to the cap, the bell rings, and the truncated body still
  reaches the dispatcher.
- Maximum decoded image memory: 256 MiB per session. A transmit that would exceed it first frees every image without a
  placement, then evicts the least recently transmitted or placed images with their placements until it fits (emitting
  `PlacementRemoved` and `Delete`), as kitty does. While the alternate screen is up, the primary screen's placements
  count as placements.
- Budget accounting: Decoded pixel size plus fixed per-entry and per-frame overheads count toward the 256 MiB session
  ceiling.
- Maximum decoded image size: 64 MiB per image. A larger image is rejected with `ENOTSUP` before its buffer is sized.
- Maximum frames per image: 4096 frames (including root). Additional frames are rejected with `ENOTSUP`.
- APC command queue: at most 64 APC bodies and 1 MiB buffered between daemon drains. The body that fills the queue
  pauses the PTY parse until the daemon drains it, so a burst of any length arrives whole.
- Shared-memory unlinks: Up to 16 deferred segment names per session. Subsequent segments trigger immediate unlinking of
  the oldest entry.
