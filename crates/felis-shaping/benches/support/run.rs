//! The row the Criterion `shape_run` bench and its instruction-count twin
//! shape, so the gated numbers and the local wall-clock numbers measure the
//! same code.

/// Ligature-bait operators are included so feature lookups actually fire.
pub(crate) const ROW: &str =
    "user@host:~/src/felis$ cargo test -p felis-shaping -- --nocapture # => != ->";

pub(crate) const PX: f32 = 16.0;

pub(crate) fn features() -> Vec<String> {
    vec!["calt".to_owned(), "liga".to_owned()]
}
