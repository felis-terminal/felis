//! The bytes [`RowCache::paint_frame`] hands the GPU for a set of small
//! screens, pinned against `tests/fixtures/paint_frame/`.
//! `UPDATE_GOLDEN=1` rewrites the fixtures (`just golden`).

use std::{num::NonZeroU32, path::PathBuf};

use bytemuck::{Pod, cast_slice};
use felis_grid::Grid;
use felis_render_core::{
    SelectionRange,
    glyphs::{GLYPH_ATLAS_SIDE, GlyphIndex, GridWalker, ShapeFrame},
    overlay::{ConfirmOverlay, LinkPreviewOverlay, PreeditOverlay, SearchHitSpan, SearchOverlay},
    painter::BAR_ELLIPSIS,
    palette::{ResolvedTheme, Theme},
};
use felis_shaping::{FontStack, Shaper, SizingKey};
use felis_vt::Parser;

use super::{CellFrame, RowCache};
use crate::gpu_instances::InstanceBuffers;

const FONT_SIZE_PX: f32 = 16.0;

#[derive(Default)]
struct Scenario {
    rows: u16,
    cols: u16,
    input: &'static str,
    viewport: (u32, u32),
    selection: Option<SelectionRange>,
    reverse_video: bool,
    preedit: Option<PreeditOverlay>,
    search: Option<SearchOverlay>,
    confirm: Option<ConfirmOverlay>,
    link_preview: Option<LinkPreviewOverlay>,
    minimum_contrast: Option<f32>,
}

/// The overlay priming `Renderer::populate_overlay_glyphs` does against
/// the GPU cache, done against the bare index.
fn prime_overlays(index: &mut GlyphIndex, shaper: &mut Shaper, s: &Scenario) {
    let h = index.cell_metrics().height;
    index.trim_overlay_clusters();
    let texts = [
        s.preedit.as_ref().map(|o| o.text.as_str()),
        s.search.as_ref().map(|o| o.label.as_str()),
        s.confirm.as_ref().map(|o| o.label.as_str()),
        s.link_preview.as_ref().map(|o| o.text.as_str()),
    ];
    for text in texts.into_iter().flatten() {
        for (range, _) in felis_grid::text_cells(text) {
            let cluster = &text[range];
            let mut chars = cluster.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => {
                    index.ensure(c, h, SizingKey::default());
                }
                _ => index.ensure_overlay_cluster(shaper, cluster),
            }
        }
    }
    if s.link_preview.is_some() {
        index.ensure(BAR_ELLIPSIS, h, SizingKey::default());
    }
}

fn paint(s: &Scenario) -> InstanceBuffers {
    let mut grid = Grid::new(s.rows, s.cols);
    Parser::new().advance(&mut grid, s.input.as_bytes());
    let screen = grid.screen();
    let side = NonZeroU32::new(GLYPH_ATLAS_SIDE).expect("non-zero atlas side");
    let mut index = GlyphIndex::new(FontStack::pinned_test_stack(), FONT_SIZE_PX, side);
    let mut shaper = Shaper::new();
    let mut shape_frame = ShapeFrame::empty();
    GridWalker::default().walk(&mut index, screen, &mut shaper, &[], &mut shape_frame);
    prime_overlays(&mut index, &mut shaper, s);
    assert!(
        !index.atlas_was_reset(),
        "the scenario fits one atlas sheet"
    );
    let frame = CellFrame {
        viewport: s.viewport.0,
        viewport_max: s.viewport.1,
        cursor_visible: true,
        selection: s.selection,
        reverse_video: s.reverse_video,
        font_features: &[],
        atlas_generation: index.atlas_resets(),
        preedit: s.preedit.as_ref(),
        search: s.search.as_ref(),
        confirm: s.confirm.as_ref(),
        link_preview: s.link_preview.as_ref(),
    };
    let mut cache = RowCache::new(1 << 28);
    let theme = ResolvedTheme::new(&Theme {
        minimum_contrast: s.minimum_contrast.unwrap_or(1.0),
        ..Theme::default()
    });
    let metrics = index.cell_metrics();
    let (out, _) = cache.paint_frame(screen, metrics, &theme, &index, &shape_frame, &frame);
    InstanceBuffers {
        bg: out.bg.clone(),
        fg: out.fg.clone(),
        deco: out.deco.clone(),
    }
}

fn encode(out: &InstanceBuffers) -> Vec<u8> {
    let mut bytes = Vec::new();
    for layer in [
        cast_slice(&out.bg),
        cast_slice(&out.fg),
        cast_slice(&out.deco),
    ] {
        bytes.extend_from_slice(&u32::try_from(layer.len()).unwrap_or(u32::MAX).to_le_bytes());
        bytes.extend_from_slice(layer);
    }
    bytes
}

/// Splits a fixture back into its three length-prefixed layers.
fn decode(mut bytes: &[u8]) -> [&[u8]; 3] {
    std::array::from_fn(|_| {
        let (len, rest) = bytes.split_at(4);
        let len = u32::from_le_bytes(len.try_into().expect("4-byte length")) as usize;
        let (layer, rest) = rest.split_at(len);
        bytes = rest;
        layer
    })
}

fn first_mismatch<T: Pod>(want: &[u8], got: &[T]) -> Option<String> {
    let size = size_of::<T>();
    let got: &[u8] = cast_slice(got);
    if want.len() != got.len() {
        return Some(format!(
            "{} instances, fixture has {}",
            got.len() / size,
            want.len() / size
        ));
    }
    let index = want
        .chunks(size)
        .zip(got.chunks(size))
        .position(|(w, g)| w != g)?;
    Some(format!("instance {index} differs"))
}

fn assert_golden(name: &str, s: &Scenario) {
    let out = paint(s);
    let path: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "tests/fixtures/paint_frame",
        &format!("{name}.bin"),
    ]
    .iter()
    .collect();
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, encode(&out)).expect("write fixture");
        return;
    }
    let fixture = std::fs::read(&path)
        .unwrap_or_else(|e| panic!("{}: {e}; regenerate with `UPDATE_GOLDEN=1`", path.display()));
    let [bg, fg, deco] = decode(&fixture);
    let diffs = [
        ("bg", first_mismatch(bg, &out.bg)),
        ("fg", first_mismatch(fg, &out.fg)),
        ("deco", first_mismatch(deco, &out.deco)),
    ];
    let report: Vec<String> = diffs
        .iter()
        .filter_map(|(layer, diff)| diff.as_ref().map(|d| format!("{layer}: {d}")))
        .collect();
    assert!(
        report.is_empty(),
        "`{name}` diverged ({}); regenerate with `UPDATE_GOLDEN=1` after checking the change is wanted",
        report.join(", ")
    );
}

#[test]
fn sgr_colors_faint_conceal_reverse_and_a_linear_selection_paint_as_pinned() {
    assert_golden("sgr_colors", &sgr_colors(None));
}

#[test]
fn the_sgr_colors_screen_at_a_minimum_contrast_of_4_5_paints_as_pinned() {
    assert_golden("sgr_colors_minimum_contrast", &sgr_colors(Some(4.5)));
}

fn sgr_colors(minimum_contrast: Option<f32>) -> Scenario {
    Scenario {
        rows: 5,
        cols: 16,
        input: concat!(
            "plain \x1b[31mred\x1b[42mgrn\x1b[0m \x1b[91mbri\x1b[0m\r\n",
            "\x1b[38;5;196;48;5;21m256\x1b[0m \x1b[38;2;10;200;30;48;2;90;0;90mtrue\x1b[0m\r\n",
            "\x1b[7mrev\x1b[27m \x1b[2mfaint\x1b[22m \x1b[8mhide\x1b[0m\r\n",
            "\x1b[1mbold\x1b[0m \x1b[3mital\x1b[0m \x1b[2;7mfr\x1b[0m\r\n",
            "\x1b[2 qblock",
        ),
        selection: Some(SelectionRange {
            start: (2, 8),
            end: (3, 3),
            rectangle: false,
        }),
        minimum_contrast,
        ..Scenario::default()
    }
}

#[test]
fn screen_reverse_video_with_a_rectangular_selection_and_a_block_cursor_on_a_wide_cell_paints_as_pinned()
 {
    assert_golden(
        "screen_reverse",
        &Scenario {
            rows: 3,
            cols: 12,
            input: "ab \x1b[34mblue\x1b[0m\r\n\x1b[7mrev\x1b[0m \x1b[43mbg\x1b[0m\r\n\x1b[2 q\u{845B}\x1b[3;1H",
            reverse_video: true,
            selection: Some(SelectionRange {
                start: (0, 1),
                end: (1, 4),
                rectangle: true,
            }),
            ..Scenario::default()
        },
    );
}

#[test]
fn wide_emoji_and_cluster_cells_under_a_bar_cursor_paint_as_pinned() {
    assert_golden(
        "wide_and_clusters",
        &Scenario {
            rows: 3,
            cols: 12,
            input: "a\u{845B}b\u{1F44D}c\r\ne\u{301}n\u{303} \x1b[7m\u{845B}\x1b[0m\r\n\x1b[6 q\u{1F44D}\x1b[3;1H",
            selection: Some(SelectionRange {
                start: (0, 1),
                end: (0, 2),
                rectangle: false,
            }),
            ..Scenario::default()
        },
    );
}

#[test]
fn decorations_link_and_bidi_cells_under_an_underline_cursor_paint_as_pinned() {
    assert_golden(
        "decorations",
        &Scenario {
            rows: 4,
            cols: 16,
            input: concat!(
                "\x1b[4mu\x1b[4:2md\x1b[4:3mc\x1b[4:4mo\x1b[4:5ma\x1b[0m ",
                "\x1b[4;58;5;9mx\x1b[58:2::0:255:0my\x1b[59mz\x1b[0m\r\n",
                "\x1b[9mstrike\x1b[29m \x1b[53mover\x1b[0m\r\n",
                "\x1b]8;;https://example.com\x07link\x1b]8;;\x07 a\u{202E}b\r\n",
                "\x1b[4 q\x1b[4m\u{845B}\x1b[0m\x1b[4;1H",
            ),
            ..Scenario::default()
        },
    );
}

#[test]
fn preedit_over_cells_and_a_search_bar_clipping_a_scaled_row_paint_as_pinned() {
    assert_golden(
        "preedit_search",
        &Scenario {
            rows: 4,
            cols: 16,
            input: "find ab here\r\nxx ab\r\n\x1b]66;s=2;Tall\x07\x1b[1;3H",
            viewport: (2, 10),
            preedit: Some(PreeditOverlay {
                anchor: (0, 2),
                text: "\u{845B}e\u{301}q".to_owned(),
                cursor: Some((3, 6)),
            }),
            search: Some(SearchOverlay {
                label: "ab 1/2".to_owned(),
                visible_hits: vec![
                    SearchHitSpan {
                        row: 0,
                        col_start: 5,
                        col_end: 7,
                        is_current: true,
                    },
                    SearchHitSpan {
                        row: 1,
                        col_start: 3,
                        col_end: 5,
                        is_current: false,
                    },
                ],
            }),
            ..Scenario::default()
        },
    );
}

#[test]
fn a_truncated_link_preview_bar_paints_as_pinned() {
    assert_golden(
        "link_preview",
        &Scenario {
            rows: 3,
            cols: 12,
            input: "top\r\n\x1b]66;s=2;Hi\x07",
            link_preview: Some(LinkPreviewOverlay {
                text: "https://example.com/a/long/path".to_owned(),
            }),
            ..Scenario::default()
        },
    );
}

#[test]
fn a_confirm_bar_wins_the_row_over_search_while_hits_still_paint_as_pinned() {
    assert_golden(
        "confirm_over_search",
        &Scenario {
            rows: 3,
            cols: 12,
            input: "ab ab\r\nab",
            search: Some(SearchOverlay {
                label: "ab".to_owned(),
                visible_hits: vec![SearchHitSpan {
                    row: 0,
                    col_start: 0,
                    col_end: 2,
                    is_current: true,
                }],
            }),
            confirm: Some(ConfirmOverlay {
                label: "Quit? y/n".to_owned(),
            }),
            ..Scenario::default()
        },
    );
}
