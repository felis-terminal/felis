//! Every felis-cli integration test, linked as one binary.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
mod config;
#[cfg(unix)]
mod daemon;
mod doctor;
#[cfg(unix)]
mod fonts;
mod schema_fixtures;
#[cfg(unix)]
mod sessions;
mod stream_openers;
#[cfg(target_os = "linux")]
mod systemd_handoff;
#[cfg(unix)]
mod version;
