//! Row-codec decoder fuzz target.
//!
//! `ipc_body` covers the protobuf body decode, but it treats
//! `packed_cells` as opaque bytes — which is exactly what the row codec
//! is not. This target covers the rest of that trust boundary: the
//! hand-written decoder in `felis_grid::wire` that every attached client
//! runs on every row the daemon sends, and that the daemon runs on rows
//! coming back through the region/transcode paths. A panic there lets a
//! malicious peer crash the process on one frame.
//!
//! For any input, `decode_row` must return `Ok` or a typed
//! `RowCodecError` — never panic, never reserve on a forged length
//! prefix. What it accepts must additionally re-encode to a **fixed
//! point**: encoding an accepted row and decoding that again must
//! reproduce the same bytes. The encoder emits maximal runs, so its
//! output is the canonical spelling of a row, and a second pass must not
//! move it — a decoder arm that mis-read a field would show up as two
//! different canonical forms for one payload.

#![no_main]

use felis_grid::{DecodedRow, RowEncode, StyleTable, decode_row, encode_row};
use libfuzzer_sys::fuzz_target;

fn re_encode(row: &DecodedRow, styles: &StyleTable) -> Vec<u8> {
    encode_row(
        RowEncode {
            cells: &row.cells,
            pad_to: row.cells.len(),
            sized_cells: &row.sized_cells,
            soft_wrap_continued: row.soft_wrap_continued,
        },
        styles,
    )
    .expect("a row that decoded is within every encode limit")
}

fuzz_target!(|data: &[u8]| {
    // The decoder interns this row's pens into its table, so re-encoding
    // through that same table resolves the ids it just minted.
    let mut styles = StyleTable::new();
    let Ok(row) = decode_row(data, &mut styles) else {
        return;
    };
    let canonical = re_encode(&row, &styles);

    let mut round_trip_styles = StyleTable::new();
    let again = decode_row(&canonical, &mut round_trip_styles)
        .expect("the encoder's own output must decode");
    assert_eq!(
        re_encode(&again, &round_trip_styles),
        canonical,
        "the canonical encoding of an accepted row must be a fixed point"
    );
});
