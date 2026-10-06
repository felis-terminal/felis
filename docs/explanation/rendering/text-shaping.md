---
title: Text shaping
sidebar:
  order: 2
---

Shaping turns a sequence of Unicode codepoints (the cell content) into a sequence of _glyph runs_: which font face,
which glyph IDs, which positions. felis delegates the shaping itself to `swash` (the engine choice is in "Prior art"
below) and runs it on the client, after grid diffs arrive, because a shaped run is a property of the font the window
happens to hold. It caches aggressively.

## Shaping unit: the ligature run

Ligatures are the only thing shaping context buys a monospace grid, so the run exists only where a ligature can form.
When `font.features` names features to apply, each row is scanned for the longest span of cells that is uniformly
**ASCII at default sizing in one style, entirely covered by that style's primary face**, and the span is shaped as one
string. Every other cell is shaped on its own, and a cell holding a multi-codepoint cluster is composited from its parts
(see "Compositing a multi-codepoint cluster"). With no features configured, no span is ever cut and the whole grid takes
the per-cell path.

Each condition marks where a wider run would be wrong rather than merely slower. A non-ASCII grapheme may need a
fallback face or may be a cluster, and either way it is not part of the primary face's ligature vocabulary. A cell
carrying an OSC 66 sizing handle shapes against different metrics. A bold or italic change selects a different styled
face, so a mixed-style span would draw the minority cells in the wrong face. Coverage is checked per character because a
span reaching into an uncovered codepoint would shape it as `.notdef` inside the run instead of letting fallback pick a
face for it.

Color and hyperlink boundaries deliberately do _not_ cut a run. Syntax highlighting recolors the middle of ligature
sequences (fish paints `<->` as `<-` plus a colored `>`), and a shaper that stopped there would suppress ligatures kitty
forms; color is per-cell paint, not shaping input, so the cross-cell quads are sliced per cell after shaping instead.
Shaping direction is hard-coded left-to-right: a terminal cell row is monospace LTR by construction.

## Cache

Caching happens at three layers: swash's own per-font caches, the per-glyph bitmap cache, and a per-run shape memo.

**swash per-font caches.** swash's `ShapeContext` / `ScaleContext` cache per-font state (GSUB shape data, feature lists,
scaler setup) keyed on `FontRef::key`. felis generates that key once per loaded `Font` and reuses one `ShapeContext` (in
`Shaper`) and one `ScaleContext` (owned by the per-glyph `ShapeCache`, which is the single rasterization gateway) so
those caches actually hit. Both contexts hold up to 8 fonts internally, above the fallback chain's typical 4.

**Per-glyph bitmap cache.** Rasterization is cached per glyph rather than per run, because the same glyph recurs across
runs that share nothing else: shells re-print one prompt thousands of times and most output is plain ASCII. An entry is
a rasterized bitmap plus its placement offsets.

The key has to separate every rendering of a glyph that would rasterize differently, so the cache keys on
`(codepoint, font_size_px, SizingKey)` (REQ-709). Three of those axes (the OSC 66 sizing state, the style it is drawn
in, and which face it resolved against) are folded into the one packed `SizingKey` rather than widened into a longer key
tuple, which is what keeps the several maps that thread it (the per-glyph cache, the renderer's atlas slot maps, the
`AtlasView` trait) from each having to grow a field when a new axis appears. A scale-1 `'A'` and a scale-2 `'A'`, a bold
`'A'` and a regular one, and one raw glyph id resolved against two different faces therefore each get their own slot
without a single map's signature changing.

The cache holds a hard-coded soft cap of 16 MiB, not configurable, and charges each entry its bookkeeping as well as its
pixels. A pixels-only budget would leave the map unbounded: a glyph that rasterizes blank (a space, a codepoint no face
covers) has no pixels, and PTY output picks the key, so a producer walking OSC 66 sizings adds entries that cost
nothing. Eviction is deliberately **not** LRU: past the cap it drops arbitrary other entries one at a time until it is
back under budget, excluding the entry just inserted. Recency ordering costs a touch per hit on the hottest path in the
renderer, to reorder a cache whose working set is a screenful of glyphs that all stay hot; the arbitrary drop is wrong
only for the entries that were about to be evicted anyway. A true LRU is future work.

**Per-run shape memo.** The renderer re-walks every visible run on every frame, and the per-glyph layers only cache
_rasterization_, so when `font.features` is active, shaping itself (cluster analysis + GSUB) would recur per run per
frame. The glyph index memoizes `shape_run` output under `(style, run text)`, one map per style so the lookup is an
index rather than a composite key. Nothing else needs to be in the key: font, size, and features are fixed for the life
of a glyph index (every reload rebuilds it), so the memo invalidates exactly when the remaining shaping inputs change.
Capped at 4096 entries per style (~10 MiB; same arbitrary-eviction discipline as the bitmap cache). A steady-state hit
is a hash lookup plus a copy into a per-frame scratch buffer, ~110× cheaper than re-shaping the run.

## Font fallback

A run may contain codepoints that the primary font does not cover (e.g. emoji in a programming font). The fallback chain
(`FontStack`) is built **eagerly** at startup, not assembled lazily per miss: when the user leaves `font.fallback`
unset, `auto_discover` walks the CJK, **symbol**, emoji, and Nerd-symbol candidate lists, in that order, and appends the
first family installed in each group, except the symbol group (see below). The cost (a single fontdb system scan shared
across every probe) is paid up front so the per-codepoint `resolve` call is a cheap charmap walk with no I/O.

A caller that must draw the same faces on every machine, such as a recording-to-video converter or a screenshot test,
runs the same discovery over a given set of font files instead of the system scan. A stack built from a single primary
face would not do: the styled faces and the fallback walk would then differ from what a window draws with those fonts.

An explicit `font.fallback` list in `config.toml` (keys and grammar in the
[config reference](../../reference/config.md)) replaces auto-discovery outright: felis appends each entry in declared
order and skips the discovery walk, so the declared order _is_ the resolution order, except for an emoji-presentation
character, which takes the first color face covering it, the primary included (see below). A missing family is skipped
with a warning rather than failing the chain: a config written on one machine degrades gracefully on another that lacks
a face.

The **symbol group sits before the emoji group, and appends every installed family rather than the first**. Both
deviations are deliberate.

The ordering serves text-presentation-default symbols (Unicode `Emoji_Presentation=No`: the asterisk dingbats
`✳`/`✻`/`✶`/`✢`, the media-control symbols `⏸`/`⏹`/`⏺`), which should render as a tinted mono glyph, not a color-emoji
tile; ordering a mono symbol face ahead of the color font is what makes `resolve`'s first-cover walk pick mono: the same
result the macOS CoreText / fontconfig cascade reaches, and why kitty shows `⏺` as a glyph.

First-cover alone is enough _because_ the symbol families are symbol-specific (Zapf Dingbats, Noto Sans Symbols, Apple
Symbols, Segoe UI Symbol; STIX Two Math is the one near-general face, kept only as the sole mono home of `⏸`/`⏹`/`⏺` on
stock macOS); a broad text font here would hijack general resolution for any non-symbol codepoint the primary lacks.

The group appends _all_-installed families rather than the first because, unlike CJK / emoji where one platform family
is a complete substitute, symbol coverage is fragmented: on macOS the asterisk dingbats live in Zapf Dingbats while
`⏸`/`⏹`/`⏺` live only in STIX Two Math, so stopping at the first match would leave half the glyphs falling through to
color.

The list order decides only for text-presentation characters. The other half of Unicode's default, an emoji-presentation
character (`Emoji_Presentation=Yes`: `⭐`, `⚡`, `☕`, `⌚`, `🀄`), takes `FontStack::resolve_color_index` first: the
first face, the primary included, that both covers it and carries color glyphs (`Font::has_color_glyphs`, a `COLR` or
color-bitmap check cached at load). First coverage decides only when no color face covers it. Without this, a symbol
face that also covers `⭐` draws it as a small mono star in its two cells. kitty does the same: `font_for_cell` in
[`fonts.c`](https://github.com/kovidgoyal/kitty/blob/master/kitty/fonts.c) skips the main font for an emoji-presentation
cell and falls back preferring color. The default presentation is a property of the character alone, so single-codepoint
`resolve` decides it from a table of the property (Unicode 17.0.0 `emoji-data.txt`, the version the grid's widths come
from).

Rejected: **keep first coverage and center the glyph in its two cells.** It fixes the left-aligned star but leaves it
monochrome where the application asked for an emoji, unlike kitty and WezTerm. It also turns color on or off depending
on which symbol fonts a host happens to install.

A presentation that depends on the codepoints around a character is decided in `Shaper::shape_cluster`, where they are
visible. Resolving `❤` (U+2764, text by default and a Zapf Dingbat) on its own picks the mono face: correct for a plain
`❤`, wrong the moment VS16 or a ZWJ join asks for the color emoji.

Three cluster signals override the default, checked in this order. A VS15 directly after the base (`⭐︎`, `🀄︎`) asks for
text, so the base takes the first face covering it even when it defaults to emoji; that is mono only when a mono face
covers it. Otherwise an emoji ZWJ / modifier sequence anchors its face on the first astral emoji scalar
(`is_emoji_face_anchor`, U+1F000..=U+1FAFF) so `❤️‍🔥` resolves off the fire. Failing both, a VS16 anywhere in the cluster
(a bare `❤️`, a keycap) routes the base through `resolve_color_index`. A VS16-less dingbat (`⏸`, a plain `❤`) has no
signal and stays mono.

Once the face is chosen, the cluster is shaped without the variation selectors that face does not map. swash 0.2.10
never looks a selector up in the cmap's variation-sequence subtable, and it drops VS15/VS16 only after an emoji base;
after a base such as `#` the VS16 is shaped as a `.notdef` with a full advance, which splits the keycap ligature into
three glyphs. Dropping it loses nothing: the presentation it asked for is already the face, and the variant it might
name is never looked up. A selector the face maps directly is kept. Revisit if swash starts mapping variation sequences.

The stack is queried per codepoint: `resolve(c)` returns the first font whose charmap covers `c` (the first color one
for an emoji-presentation character), falling back to the primary (which then renders its `.notdef` box) when nothing
covers it. A cell whose character the primary face does not cover is therefore never inside a ligature run: the run scan
stops at it, and it is shaped alone against the face `resolve` picked.

Holding the whole eager chain open is cheap even when several fallback faces are large, because font bytes are
memory-mapped, not heap-copied (next section). The symbol group's all-installed rule adds at most a handful of faces (≤3
on stock macOS: Zapf Dingbats ~50 KB, STIX Two Math and Apple Symbols a few MB each on disk), and each is mapped, not
read, so the cost is virtual address space plus the pages actually faulted when a glyph is drawn, KB-scale resident, the
same trade the ~183 MB emoji face already makes. The only steady-state cost is a slightly longer `resolve` walk for
codepoints nothing covers, which the shape cache amortizes after the first miss.

## Per-style faces

Bold and italic cells render in **separate font faces**, the felis analogue of kitty's `bold_font` / `italic_font` /
`bold_italic_font`. The `FontStack` holds four styled primaries indexed by a 2-bit `FontStyle` (regular, bold, italic,
bold-italic); `resolve(c, style)` picks the styled primary that covers `c` (a color one for an emoji-presentation `c`),
then walks the **shared** codepoint fallback chain. A cell's `AttrFlags::BOLD` / `ITALIC` map to the `FontStyle`, and
`resolve` is asked for that style.

**Config shape: nested tables, not four flat keys.** The face is configured with `[font.bold]` / `[font.italic]` /
`[font.bold_italic]` sub-tables (config reference), each carrying its own optional `family` and `features`. The nested
form is chosen over four flat `bold_family = …` keys because a per-style `features` override belongs _with_ its family
(a script italic may want different stylistic sets than the roman), and a table groups them without a naming convention
that pairs `bold_family` with `bold_features` by prefix. When a table (or its `family`) is omitted, the style is derived
from the base `font.family` at the matching weight/slant, so a plain single-`family` config still gets real bold/italic
variants automatically, matching the first-run expectation set by every other terminal.

**Missing styled face: reuse regular, never synthesize.** felis does **not** synthesize fake-bold (outline emboldening)
or fake-italic (a shear transform) when a family ships no real styled face. fontdb's matcher returns the closest face
(the regular one) for a family without a Bold; felis dedups that by fontdb face id and reuses the _same_ `Arc<Font>`, so
the missing style costs no extra bytes and draws the regular glyph. This mirrors the fallback policy elsewhere (a
missing `font.fallback` family is skipped, not substituted): the config is a description of intent that degrades
gracefully across machines, and a user who wants bold to stand out installs a bold face rather than accepting a
mechanically-smeared approximation.

_Revisit if_ users ask for synthesized styles for faces that genuinely lack them: the hook is a synthesis pass (zeno
emboldening + a shear in the scaler) gated behind an opt-in config, layered on top of this face-selection path without
disturbing it.

**Shared fallback, shared metrics.** The CJK / emoji / symbol / Nerd fallback chain is _not_ styled: those faces ship
one weight each, and a per-script styled cascade (kitty's configurable per-script chains) is out of scope for felis's
one-flat-chain model. Cell metrics also come from the _regular_ primary only (the four faces must share an advance width
or the monospace grid would break), so only codepoint coverage and the rasterized outline differ per style, never the
cell box.

Style rides the sizing key like every other axis that makes two lookups of one glyph differ (see "Cache" above), so
`GlyphIndex::ensure` recovers the styled face from the key alone.

## Feature defaults and inheritance

OpenType features (`font.features` in the [config reference](../../reference/config.md)) default to **off**:
programming-ligature fonts (FiraCode, JetBrains Mono, Cascadia Code) render without ligatures unless the user opts in.
This matches the first-run behavior of xterm, alacritty, kitty, and wezterm. Flipping the default would shift cell
metrics for users who never asked for ligatures, a silent layout change on upgrade that no performance win justifies.

The primary face's `features` list is also the inherited default for every `font.fallback` entry that omits its own.
Inheritance is safe because shapers silently ignore feature tags a font does not support: `"calt"` set globally is a
no-op on a CJK or color-emoji face that lacks the table, so the common case needs no per-font duplication. For the rare
case where inheritance _would_ be wrong, `features = [...]` overrides per-fallback, and an explicit empty
`features = []` is the opt-out, useful for color-emoji, where the primary's `calt` should not leak in.

Omission means inherit for a fallback entry and for a `[font.bold]`-style table alike, because they are the same table:
one `family`, one optional `features`, one rule. Rejected: a bespoke fallback-entry shape (a bare family string, or a
table whose omitted `features` opts _out_); it would put the two surfaces in silent disagreement over what an absent key
means. One type costs the bare-string spelling and buys a rule the user learns once.

## Font loading

Font bytes are **memory-mapped, not heap-copied**. fontdb hands system faces back as file paths; felis opens each one
and maps it with `memmap2::Mmap::map`, so a large face (Apple Color Emoji is ~183 MB) stays file-backed (shared, clean,
reclaimable page cache) instead of becoming dirty heap. Faces fontdb already shares (`Binary` / `SharedFile`) reuse the
existing `Arc` directly; no system face is copied onto the heap. fontdb's own system scan runs with its `memmap` feature
on for the same reason. The one exception is a caller's pinned font files (see "Font fallback"), which are read into
memory: mapping is sound only while nothing truncates the file, which holds for a root-owned system font directory but
not for a file the caller owns.

The numbers force this. With `fs::read`, the client's physical footprint on macOS is ~450 MB, almost entirely font heap:
the system scan reads every font file onto the heap just to parse it (macOS malloc then retains ~240 MB of the freed
blocks in `phys_footprint`), and the eagerly loaded emoji + CJK fallback faces are ~190 MB of _live_ heap. Mapped, the
footprint is ~43 MB and Apple Color Emoji costs 16 KB resident: coverage probes (`has_glyph`) and rasterization fault
pages on demand, so cold faces stay unfaulted. That puts felis in the Alacritty/Kitty/Ghostty band (~20–100 MB) instead
of far above it. The field agrees: every surveyed terminal (Alacritty/`crossfont`, WezTerm, Ghostty, Kitty, foot/`fcft`)
keeps font bytes file-backed, via the OS font API (CoreText) or FreeType's path-based `FT_New_Face` (which mmaps on
Unix); WezTerm mmaps the file itself and feeds FreeType an `FT_Stream` (`wezterm-font/src/ftwrap.rs`), the same shape
felis uses.

The cost is one `unsafe` call. fontdb stores kept faces as bare paths and exposes a persistent mmap only through its
`unsafe fn make_shared_face_data`; swash needs a persistent `&[u8]` to rasterize from, so there is no safe route to a
file-backed face. `felis-shaping` therefore relaxes the workspace-wide `unsafe_code = "deny"` lint crate-wide (the
policy lives in `Cargo.toml` / `clippy.toml`), with `Mmap::map` as the **single audited `unsafe` site**, carrying a
`// SAFETY:` comment. A mapped file truncated underneath the process would SIGBUS; accepted, because the mapped paths
are root-owned read-only OS font assets that are not rewritten in place during a session, the identical trade-off
fontdb's `make_shared_face_data` and FreeType already make.

Rejected alternatives:

- **Keep `fs::read`, accept the footprint.** ~190 MB of dirty heap for fonts is anomalous against every surveyed
  terminal.
- **fontdb's `make_shared_face_data`.** Equally `unsafe`, so it needs the same lint relaxation, but it requires
  threading `&mut Database` into the load path and caching inside fontdb. The direct `memmap2::Mmap::map` is one
  self-contained line and matches WezTerm's reference implementation.
- **Lazy fallback-face opening (no `unsafe`).** The field's other technique: defer opening emoji/CJK until a glyph
  misses the primary. Helps only ASCII-only sessions, needs interior mutability in `FontStack::resolve`, and still reads
  183 MB the first time an emoji appears; with mmap a held face costs ~16 KB resident, so the laziness buys nothing once
  the file is mapped, and Nerd-Font / emoji glyphs are common enough that the fast path would rarely fire. Could still
  be layered on later if cold-start face _count_ ever matters.
- **Keep `fs::read` but switch to a returning allocator (jemalloc/mimalloc).** Hands the freed transient blocks back to
  the OS but leaves the live kept faces on the heap: treats the malloc-retention symptom, not the heap-copying cause.

Revisit if: a second `unsafe` block is ever proposed in `felis-shaping` (the relaxation covers exactly this one site:
widen it deliberately, not silently); fontdb grows a **safe** API for persistent mmap'd sources (then drop the local
`unsafe` and restore `#![forbid(unsafe_code)]`); or a platform appears where mapping system fonts is unsafe in practice
(fonts mutated in place, or no mmap support): fall back to `fs::read` there behind `cfg`.

## Atlas integration

Shaping emits glyph ids that are face-relative, so the atlas resolves them through the same key the bitmap cache uses
and rasterizes on miss, at the effective pixel size against the face the glyph resolved to. Metrics reaching the
renderer are already post-scale, so an OSC 66 run needs no second multiplication at layout time.

Glyphs rasterize pixel-aligned: there is no sub-pixel offset axis in the key and cell metrics land on whole pixels. A
sub-pixel axis would multiply every entry by the number of phases and buy positioning a monospace grid cannot use, since
every cell origin is already on a pixel boundary.

The cell width rounds the primary face's unhinted `M` advance to the nearest pixel, the width kitty, alacritty and foot
give on Linux and ghostty gives everywhere. kitty reaches it by another route: it ceils each ASCII advance
([`freetype.c`](https://github.com/kovidgoyal/kitty/blob/master/kitty/freetype.c) `calc_cell_width`), but on Linux it
reads that advance from FreeType after loading the glyph under the fontconfig hint style, and whenever fontconfig
enables hinting (slight included) FreeType returns an advance already rounded to a whole pixel. DejaVu Sans Mono's `M`
at 14 pt (18.67 px) advances 11.24 px unhinted; kitty receives 11.0 and its ceil leaves 11. On macOS kitty reads
CoreText's unhinted advance instead, so its ceil acts on the fraction: Menlo's `M` at 14 pt advances 8.43 px, kitty
sizes 9, and ghostty ([`Metrics.zig`](https://github.com/ghostty-org/ghostty/blob/main/src/font/Metrics.zig) `@round`)
and alacritty (floor) size 8.

Rounding lets ink cross the cell edge, and between cells that costs no ink. The renderer draws every cell background
before any glyph, and each glyph quad at its bitmap size from its own bearing, never clipped to the cell box, so ink
past the edge lands on top of the neighbor; only the window edge bounds the last column, as it bounds any overhanging
glyph.

How far it reaches depends on the raster. Unhinted, as on macOS, a glyph whose ink fills its advance crosses by the
rounded-off fraction, under half a pixel. On Linux, where felis hints the raster, hinting widens some outlines further:
at 18.67 px in an 11 px cell, DejaVu Sans Mono's `_`, `#` and `W` put one column of partial coverage (up to 44%) into
the next cell. kitty on Linux accepts the unhinted form of the same overlap (slight hinting rounds the advance but
leaves the outline unhinted horizontally), and ghostty states it as its trade.

Rejected:

- **Ceil the advance.** Glyphs then never touch, but DejaVu Sans Mono at 14 pt gets a 12 px cell where every Linux peer
  gets 11, so the same window holds fewer columns than in kitty, and a grid of a given column count is 9% wider.
- **Round on Linux, ceil on macOS.** It reproduces kitty on both platforms, but only by mirroring where kitty reads its
  advance: hinted from FreeType, unhinted from CoreText. felis reads the same unhinted swash advance on both, so the
  split would encode a difference felis does not have.

A glyph whose advance overruns the cells it is placed in (a `※` the CJK fallback face draws full-width in its one cell)
rasterizes smaller and is centred in its block. The advance decides whether a glyph is fitted: an italic overhang or a
hinted stem's extra column is ink past the advance on a glyph drawn for one cell, and fitting on the ink box, as kitty
does ([`freetype.c`](https://github.com/kovidgoyal/kitty/blob/master/kitty/freetype.c) `render_bitmap`), would shrink
ordinary text. The ink decides how far: a full-width face pads its ink with side bearings, and shrinking the whole
advance into the cell leaves the glyph visibly smaller than in kitty or WezTerm. Centring trades the baseline for
balance; nearly every fitted glyph is a symbol, CJK punctuation or an emoji, where a glyph shrunk toward the baseline
sits low in its cell.

Rejected: **clip to the cell box.** It keeps the neighbor clean but cuts the glyph, and it would also cut the rounding
overlap above.

Ascent and descent ceil **separately** rather than the cell height rounding their sum: that puts the baseline itself on
a pixel boundary and gives each half whole rows, so a glyph reaching the full ascent keeps its top row. Rounding the sum
leaves the baseline a fraction of a pixel high and clips that row. This is fcft's rule, which foot inherits
([`fcft.c`](https://codeberg.org/dnkl/fcft) ceils each FreeType size metric), and it agrees with FreeType's own
pixel-rounded size metrics (ascender CEIL, descender FLOOR). Rejected: kitty's extra step of rasterizing `_` and growing
the cell when the bitmap escapes the box. It reaches the same height on the faces compared here, but only by putting
glyph rasterization inside metric computation, and its result then varies with the hinting mode the rasterizer ran
under.

No height rule matches every peer on both platforms. Menlo at 14 pt has a 3.30 px descent: ceiling it gives felis a 17
px cell, where kitty, alacritty and ghostty size 16 (CoreText sets Menlo's lines 16 px apart, alacritty rounds each
metric, ghostty rounds the sum). Rounding the descent would reach 16 on macOS, but it turns DejaVu Sans Mono's 4.40 px
descent at 14 pt on Linux into 4 and the cell into 22 px, one row short of the 23 px kitty, alacritty and foot all size.
The separate ceil keeps Linux parity and costs macOS one row.

Revisit if: the cell width reads a hinted advance (it then arrives whole, and the width rule stops mattering), or the
renderer clips glyph quads to the cell box or paints a cell's background after its neighbor's glyph (the overlap
rounding allows then loses ink).

## Compositing a multi-codepoint cluster

A grapheme that occupies one cell but spans several codepoints (a base letter plus stacked combining marks, an emoji ZWJ
sequence such as 👨‍👩‍👧, a skin-tone modifier, or a base plus a variation selector) is segmented and stored intact (the cell
holds a `Grapheme::Cluster` handle into the grid's cluster table) and the renderer composites the whole cluster. It is
shaped as a unit and drawn as one `FgInstance` per output glyph, at default size or at an OSC 66 scale (see the
sized-cluster note below). The daemon is unaffected: it stays font-blind and ships the cluster text as a
`GridMsg::Cluster { id, text }` registry entry, the wire path segmentation already uses; compositing is entirely
client-side.

**Shape the whole cluster; let the font decide the glyph count.** The cluster text goes through swash and the renderer
emits one instance per glyph the font's own tables produce. That count is not fixed per input: a base plus a combining
mark the font positions with GPOS stays two glyphs, a ZWJ sequence the font covers renders as one color glyph, a
precomposed form is one glyph from the start. felis pre-decides none of that. The alternative, inspecting the codepoints
to route marks and joins through its own stacking, reproduces work the shaper already does from the font's GSUB, GPOS,
and COLR tables, and does it worse for every script felis did not special-case. The per-glyph positioning offsets swash
returns are applied as given, including the sign flip from the font's y-up space to the screen's y-down one, so a
combining mark rides its base onto the anchor the font specifies rather than one felis invented.

One consequence is worth stating because it constrains the atlas: a composited cluster can pull glyphs from more than
one face, a base from the primary and a mark from a fallback that covers it. The resolved face is therefore part of the
cache key ("Cache" above), or a fallback mark and a primary base that happen to share a raw glyph id would collide.

**A cluster wider than its cells shrinks to fit them.** The font, not the grid, decides how wide a cluster draws, and
the two disagree whenever the face cannot join what the grid joined: a regional-indicator pair with no flag glyph, a ZWJ
sequence the face does not ligate, or a skin-tone modifier on a base that takes none each draw as two full emoji in two
cells. The grid can also give a cluster fewer cells than its glyph wants, as when it refuses a VS16 widen at the last
column. Such a cluster fits by the single-glyph rule above: its summed advance decides whether it is fitted, its union
ink decides how far it shrinks, and it is centred in its block. All its glyphs shrink by one factor, so a mark stays on
its base. The block is the cells the grid gave the cluster (or OSC 66 `w`), never its base character's width, which
undercounts a widened `❤️` and overcounts a refused one.

**Sized clusters composite at scale.** A cluster carrying an OSC 66 scale rasterizes at the scaled size and scales its
pen advances and positioning offsets by the same factor, so the marks track the scaled base rather than drifting off it.

_Non-default_ block alignment (`h`/`v` = Center / Right / Bottom) on a multi-glyph cluster aligns the cluster's **union
ink box**, every resolved glyph's bitmap box at its baseline placement, so a mark that extends past its base keeps the
whole cluster inside the block edge. Aligning only the base glyph's box is the cheaper alternative, by one atlas probe
per glyph, and it lets a wide mark overflow the block by exactly the amount it extends past its base. Top/Left, the OSC
66 default, takes an early-out rather than the ink formula, which is what keeps default-sized output identical to the
path that never looks at alignment at all. A fitted cluster is the exception: Top/Left centres it, as a fitted glyph is
centred.

## What happens on font change

A live font swap invalidates every derived structure at once: the shaping caches, the glyph atlas, and the cell metrics,
followed by a `Resize` to the daemon when the new metrics change the column or row count. Nothing is salvaged and
nothing is invalidated incrementally.

That is deliberately heavyweight, and the trade is one-sided. A font swap happens when a user edits their config; the
steady state happens sixty times a second. Incremental invalidation would put a validity check on every cache lookup to
save work in the case nobody is waiting on.

## Prior art and alternatives considered

**Shaper: swash, not HarfBuzz or a platform API.** HarfBuzz is the reference shaper (Kitty, foot, WezTerm), with the
widest coverage but an FFI dependency and a large per-face cache; CoreText (iTerm2) and DirectWrite (Windows Terminal)
are platform-locked; Alacritty ships _no_ shaper by default, rendering one glyph per codepoint with ligatures off, a
speed-over-correctness choice felis's principles reject. felis uses swash: pure Rust (no FFI lifetime juggling),
coverage sufficient for the scripts a terminal sees, and it rasterizes too, so one crate covers both shaping and
rendering. A rustybuzz fallback for swash's complex-script gaps is unnecessary and is not wired.

**Ligatures: shape, then validate.** A general shaper does not know about cells. felis shapes a run once and checks each
ligature's advances against `n_graphemes × cell_width`; a ligature that would overflow its cell budget falls back to
per-cell glyphs. The budget allows a pixel per cell, because the cell width is the primary advance rounded to a pixel.
Kitty and WezTerm do the same. Pre-restricting the shaper's input is hard to get right, and never shaping at all
(Alacritty) gives up ligatures; the validation step is cheap because cost is dominated by cache hit rate, not shaping
calls.

**Font fallback: built from the OS, not a per-script table.** Kitty makes the per-script chain configurable; WezTerm and
iTerm2 defer to the platform font API. felis keeps the font config to one flat chain: the user's chosen primary, then OS
discovery (fontconfig / CoreText / DirectWrite) of the CJK / symbol / emoji / Nerd-Font groups, assembled eagerly at
startup (see "Font fallback" above), with an explicit `font.fallback` list replacing discovery wholesale rather than
adding a per-script table. The primary-face binding is client-side policy; the daemon stores no font information.

**Default face: fontdb's alias, then a fixed list.** With `font.family` unset, felis asks fontdb for its `monospace`
generic. fontdb reads fontconfig's XML itself but keeps only the first preferred family of the last `monospace` alias it
parses, where fontconfig walks every alias and takes the first installed family; on Debian and Ubuntu that leaves the
generic naming `FreeMono` (from `69-unifont.conf`) on a host that has only DejaVu installed. When the generic names
nothing installed, felis tries a fixed list of common monospace families, then the first installed fixed-pitch family by
name. Asking fontconfig itself is rejected both ways: shelling out to `fc-match` needs fontconfig's CLI, which a host
with only `fontconfig-config` lacks, and linking libfontconfig puts back the C dependency fontdb's own parser exists to
avoid, one the relocatable Linux archive would have to take from the host.

_Revisit if_ fontdb resolves generics through the whole alias chain: the fallback then only covers hosts with no alias
at all.

**Sub-pixel positioning: not done.** Kitty, WezTerm, Ghostty, and iTerm2 rasterize each glyph at four horizontal
sub-pixel offsets. felis rasterizes pixel-aligned, sizing cells in whole pixels with no sub-pixel offset axis in the
atlas key (see "Atlas integration" above). At HiDPI the difference is marginal, and one raster per glyph keeps the atlas
smaller and the cache key simpler.

**Hinting, variable, and color fonts.** Hinting is off on macOS, where CoreText renders every native window unhinted,
and on everywhere else; fontconfig's hinting preference is not consulted. Bold / italic select a separate styled face
per [Per-style faces](#per-style-faces). For a variable font that ships one file, fontdb's weight/slant query lands the
right named instance (weight 400 / 700) of that file, so the styled primary and the regular primary can be two views of
one variable face. Color fonts (COLR/CPAL, sbix, CBDT, all supported by swash) rasterize into RGBA atlas slots; the cell
shader samples RGBA instead of R8.

**Bidi: not performed.** felis runs no UAX#9 reordering across the cell grid (a non-goal: the grid is linear visual
order), and the shaper is fixed left-to-right inside a run as well ("Shaping unit: the ligature run" above), so RTL text
is shaped and drawn in the order its cells hold.
