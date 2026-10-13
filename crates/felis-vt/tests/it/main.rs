//! Every felis-vt integration test, linked as one binary.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod proptest_kitty_graphics;
mod proptest_kitty_text_sizing;
mod proptest_parser;
mod proptest_placeholder;
mod proptest_program_status;
mod snapshot_kitty_graphics;
mod snapshot_kitty_text_sizing;
