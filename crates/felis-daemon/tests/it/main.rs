//! Every felis-daemon integration test, linked as one binary.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod alt_enter_then_place_same_burst;
#[cfg(unix)]
mod capacity_real_daemon;
mod clear_then_place_same_burst;
mod common;
#[cfg(unix)]
mod cross_host_stdio;
#[cfg(unix)]
mod head_of_line_rehydrate;
mod image_outbox_materialization;
#[cfg(unix)]
mod in_place_upgrade;
mod live_emission_kitty_graphics;
#[cfg(unix)]
#[expect(
    clippy::print_stderr,
    reason = "skip diagnostics are intentional, mirroring real_app_harness"
)]
mod localhost_ssh;
#[expect(
    clippy::type_complexity,
    reason = "proptest strategy return types are inherently nested; a named alias would not aid the test"
)]
mod proptest_alt_screen_placements;
mod proptest_image_mirror;
#[expect(
    clippy::manual_let_else,
    reason = "integration harness: explicit match reads clearly"
)]
mod real_app_harness;
mod reflow_remaps_placements;
mod security_kitty_graphics;
mod snapshot_kitty_graphics_dispatcher;
#[cfg(windows)]
mod windows_upgrade;
mod yazi_kgp_old_direct_placement;
